// Copyright 2026 Martin Pool

//! Reuse mutant outcomes from earlier runs when the code they depend on hasn't changed.
//!
//! Each outcome is keyed by a hash of the mutated function's text, the mutation, where the
//! mutation is within the function, and the package's integration tests. Moving a function
//! within or between files therefore keeps its results, unlike `--iterate`, which matches
//! mutants by line and column.
//!
//! This is a heuristic: a change to unit tests elsewhere in the package's source, or to code
//! the function calls, won't invalidate the cached outcome. Run without `--cache` from
//! time to time to be sure.

#![warn(clippy::pedantic)]

use std::collections::BTreeMap;
use std::fs::{read_dir, read_to_string, write};

use anyhow::Context;
use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::Result;
use crate::mutant::Mutant;
use crate::outcome::SummaryOutcome;

/// Name of the cache file within `mutants.out`.
pub const CACHE_JSON: &str = "cache.json";

/// Changing this invalidates all existing caches, if the key computation changes.
const KEY_VERSION: &str = "cargo-mutants-cache-1";

/// An outcome that can be reused from the cache.
///
/// Timeouts aren't cached, because they may depend on load on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachedOutcome {
    Caught,
    Missed,
    Unviable,
}

impl CachedOutcome {
    pub fn from_summary(summary: &SummaryOutcome) -> Option<CachedOutcome> {
        match summary {
            SummaryOutcome::CaughtMutant => Some(CachedOutcome::Caught),
            SummaryOutcome::MissedMutant => Some(CachedOutcome::Missed),
            SummaryOutcome::Unviable => Some(CachedOutcome::Unviable),
            _ => None,
        }
    }
}

/// Outcomes of mutants by their cache key.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResultCache {
    outcomes: BTreeMap<String, CachedOutcome>,
}

impl ResultCache {
    /// Load the cache from `mutants.out` in the given directory, before it's rotated.
    ///
    /// A missing or unreadable cache is treated as empty.
    pub fn load(output_parent_dir: &Utf8Path) -> ResultCache {
        let path = output_parent_dir
            .join(crate::output::OUTDIR_NAME)
            .join(CACHE_JSON);
        if !path.is_file() {
            debug!(?path, "no results cache");
            return ResultCache::default();
        }
        match read_to_string(&path)
            .context("read")
            .and_then(|json| serde_json::from_str(&json).context("parse"))
        {
            Ok(cache) => cache,
            Err(err) => {
                warn!(?path, "Ignoring unreadable results cache: {err:#}");
                ResultCache::default()
            }
        }
    }

    /// Write the cache into an output directory.
    pub fn save(&self, output_dir: &Utf8Path) -> Result<()> {
        let path = output_dir.join(CACHE_JSON);
        write(&path, serde_json::to_string_pretty(self)?).with_context(|| format!("write {path}"))
    }

    pub fn get(&self, key: &str) -> Option<CachedOutcome> {
        self.outcomes.get(key).copied()
    }

    pub fn insert(&mut self, key: String, outcome: CachedOutcome) {
        self.outcomes.insert(key, outcome);
    }

    pub fn len(&self) -> usize {
        self.outcomes.len()
    }
}

/// Compute the cache key for a mutant.
///
/// `tests_fingerprint` is from [`tests_fingerprint`] for the mutant's package.
pub fn cache_key(mutant: &Mutant, tests_fingerprint: u64) -> String {
    let mut hasher = Fnv1a::default();
    hasher.add(KEY_VERSION);
    hasher.add(&mutant.source_file.package.name);
    hasher.add(&format!("{:?}", mutant.genre));
    hasher.add(&mutant.describe_change());
    if let Some(function) = &mutant.function {
        // Where the mutation is relative to the function, so that moving the function
        // vertically doesn't change the key.
        hasher.add(&function.span.extract(mutant.source_file.code()));
        hasher.add(&format!(
            "{}:{}-{}:{}",
            mutant.span.start.line - function.span.start.line,
            mutant.span.start.column,
            mutant.span.end.line - function.span.start.line,
            mutant.span.end.column,
        ));
    } else {
        hasher.add(&mutant.source_file.tree_relative_slashes());
        hasher.add(&format!("{:?}", mutant.span));
        hasher.add(mutant.source_file.code());
    }
    hasher.add(&tests_fingerprint.to_string());
    format!("{:016x}", hasher.finish())
}

/// Hash the Rust source of a package's integration tests, in its `tests` directory.
///
/// Returns a fixed value if there are none.
pub fn tests_fingerprint(package_dir: &Utf8Path) -> Result<u64> {
    let mut hasher = Fnv1a::default();
    let tests_dir = package_dir.join("tests");
    if tests_dir.is_dir() {
        hash_rust_files(&tests_dir, &tests_dir, &mut hasher)?;
    }
    Ok(hasher.finish())
}

fn hash_rust_files(root: &Utf8Path, dir: &Utf8Path, hasher: &mut Fnv1a) -> Result<()> {
    let mut entries = read_dir(dir)
        .with_context(|| format!("read {dir}"))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        let Some(path) = Utf8Path::from_path(&path) else {
            continue;
        };
        if path.is_dir() {
            hash_rust_files(root, path, hasher)?;
        } else if path.extension() == Some("rs") {
            hasher.add(path.strip_prefix(root).unwrap_or(path).as_str());
            hasher.add(&read_to_string(path).with_context(|| format!("read {path}"))?);
        }
    }
    Ok(())
}

/// 64-bit FNV-1a: simple and stable across Rust versions, unlike `DefaultHasher`.
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    /// Add a string, with a terminator so that consecutive strings can't run together.
    fn add(&mut self, s: &str) {
        for &b in s.as_bytes().iter().chain(&[0xff]) {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use indoc::indoc;

    use crate::Genre;
    use crate::mutant::Function;
    use crate::source::SourceFile;
    use crate::span::Span;

    use super::*;

    fn fn_value_mutant(code: &str, fn_span: Span, body_span: Span) -> Mutant {
        Mutant::new_discovered(
            SourceFile::for_tests("src/lib.rs", code, "cargo-mutants-testdata-cache", true),
            Some(Arc::new(Function {
                function_name: "f".to_owned(),
                return_type: "-> u32".to_owned(),
                span: fn_span,
            })),
            body_span,
            None,
            "0".to_owned(),
            Genre::FnValue,
            None,
            None,
        )
    }

    #[test]
    fn cache_key_is_unchanged_when_function_moves_down() {
        let before = fn_value_mutant(
            "fn f() -> u32 {\n    1\n}\n",
            Span::quad(1, 1, 3, 2),
            Span::quad(2, 5, 2, 6),
        );
        let after = fn_value_mutant(
            "// new comment\n\nfn f() -> u32 {\n    1\n}\n",
            Span::quad(3, 1, 5, 2),
            Span::quad(4, 5, 4, 6),
        );
        assert_eq!(cache_key(&before, 7), cache_key(&after, 7));
    }

    #[test]
    fn cache_key_changes_with_function_text_or_tests() {
        let original = fn_value_mutant(
            "fn f() -> u32 {\n    1\n}\n",
            Span::quad(1, 1, 3, 2),
            Span::quad(2, 5, 2, 6),
        );
        let edited = fn_value_mutant(
            "fn f() -> u32 {\n    2\n}\n",
            Span::quad(1, 1, 3, 2),
            Span::quad(2, 5, 2, 6),
        );
        assert_ne!(cache_key(&original, 7), cache_key(&edited, 7));
        assert_ne!(cache_key(&original, 7), cache_key(&original, 8));
    }

    #[test]
    fn result_cache_save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = Utf8Path::from_path(tmp.path()).unwrap();
        let out = parent.join(crate::output::OUTDIR_NAME);
        std::fs::create_dir(&out).unwrap();
        let mut cache = ResultCache::default();
        cache.insert("abc".to_owned(), CachedOutcome::Caught);
        cache.save(&out).unwrap();
        let loaded = ResultCache::load(parent);
        assert_eq!(loaded.get("abc"), Some(CachedOutcome::Caught));
        assert_eq!(loaded.get("def"), None);
    }

    #[test]
    fn result_cache_load_ignores_corrupt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = Utf8Path::from_path(tmp.path()).unwrap();
        let out = parent.join(crate::output::OUTDIR_NAME);
        std::fs::create_dir(&out).unwrap();
        write(out.join(CACHE_JSON), indoc! {"not json"}).unwrap();
        assert_eq!(ResultCache::load(parent).len(), 0);
    }

    #[test]
    fn tests_fingerprint_changes_with_test_source() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let empty = tests_fingerprint(dir).unwrap();
        std::fs::create_dir(dir.join("tests")).unwrap();
        write(dir.join("tests/a.rs"), "#[test] fn a() {}").unwrap();
        let one = tests_fingerprint(dir).unwrap();
        write(dir.join("tests/a.rs"), "#[test] fn a() { panic!() }").unwrap();
        let changed = tests_fingerprint(dir).unwrap();
        assert_ne!(empty, one);
        assert_ne!(one, changed);
    }
}
