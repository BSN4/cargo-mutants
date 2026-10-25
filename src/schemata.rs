// Copyright 2026 Martin Pool

//! Compile many mutants into one build, and choose between them at runtime ("mutant schemata").
//!
//! Each mutated region is replaced by code that reads the active mutant id from
//! [`ACTIVE_ENV`] and runs either the original or the mutated code. The tree is then built once,
//! and each mutant is tested by running the tests with that variable set.
//!
//! Some mutants don't compile in this form, for example because the mutated code doesn't
//! type-check or the region is in a `const` context. Compiler errors are mapped back to the
//! mutants whose code they're in, and those mutants are left out of the schemata and tested
//! by building them one at a time as usual.

#![warn(clippy::pedantic)]

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::ops::Range;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use crate::Mutant;
use crate::mutant::Switch;
use crate::span::LineColumn;
use crate::test_select::TRACE_ENV;

/// Environment variable holding the id of the mutant to activate, or unset for none.
pub const ACTIVE_ENV: &str = "CARGO_MUTANTS_SCHEMATA_ID";

/// Name of the local variable holding the active mutant id in generated code.
const ACTIVE_VAR: &str = "__cargo_mutants_active";

/// Generated source for one file, with a map back to mutants for compiler errors.
#[derive(Debug)]
pub struct SchemataFile {
    /// The source with all the mutants switched in.
    pub code: String,
    /// Mutants that couldn't be switched in, because their region overlaps another.
    pub unswitched: Vec<usize>,
    /// Ranges of the generated code, and the mutants responsible for the code in them.
    regions: Vec<(Range<usize>, Vec<usize>)>,
}

impl SchemataFile {
    /// Return the mutants to blame for a compiler error at a byte offset in the generated code.
    ///
    /// This is the mutants in the smallest region containing the offset, or None if it's
    /// outside all of them.
    pub fn blame(&self, offset: usize) -> Option<&[usize]> {
        self.regions
            .iter()
            .filter(|(range, _)| range.contains(&offset))
            .min_by_key(|(range, _)| range.len())
            .map(|(_, ids)| ids.as_slice())
    }
}

/// A region of the original source where one or more mutants can be switched in.
#[derive(Debug)]
struct Site {
    /// Byte range in the original code.
    range: Range<usize>,
    kind: SiteKind,
    /// Mutant ids and their mutated text for this whole range (empty for match arms).
    alternatives: Vec<(usize, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SiteKind {
    /// An expression, or a whole function body if `body` is true.
    Expr { body: bool },
    /// A match arm, that can be disabled by a guard inserted at this byte offset.
    ArmGuard { at: usize },
}

/// Sites are ordered by their start, then outermost first. A whole function body comes
/// before an expression with the same range, so that it's outermost.
type SiteKey = (usize, Reverse<usize>, Reverse<SiteKind>);

/// Generate the source for one file with all the given mutants switched in.
///
/// `mutants` are pairs of a mutant id, which must be nonzero, and a mutant, which must
/// be in this file and have a [`Switch`].
pub fn generate(code: &str, mutants: &[(usize, &Mutant)]) -> SchemataFile {
    let line_starts = line_starts(code);
    let offset = |lc: LineColumn| byte_offset(code, &line_starts, lc);
    let mut unswitched = Vec::new();
    let mut sites: BTreeMap<SiteKey, Vec<(usize, String)>> = BTreeMap::new();
    for &(id, mutant) in mutants {
        assert_ne!(id, 0, "mutant id 0 means no mutant");
        let (Some(switch), Some(start), Some(end)) = (
            mutant.switch,
            offset(mutant.span.start),
            offset(mutant.span.end),
        ) else {
            unswitched.push(id);
            continue;
        };
        match switch {
            Switch::Expr { span, body } => {
                let (Some(site_start), Some(site_end)) = (offset(span.start), offset(span.end))
                else {
                    unswitched.push(id);
                    continue;
                };
                if !(site_start <= start && end <= site_end) {
                    unswitched.push(id);
                    continue;
                }
                let mutated = format!(
                    "{}{}{}",
                    &code[site_start..start],
                    mutant.replacement,
                    &code[end..site_end]
                );
                sites
                    .entry((
                        site_start,
                        Reverse(site_end),
                        Reverse(SiteKind::Expr { body }),
                    ))
                    .or_default()
                    .push((id, mutated));
            }
            Switch::ArmGuard { at } => {
                let Some(at) = offset(at).filter(|at| (start..end).contains(at)) else {
                    unswitched.push(id);
                    continue;
                };
                sites
                    .entry((start, Reverse(end), Reverse(SiteKind::ArmGuard { at })))
                    .or_default()
                    .push((id, String::new()));
            }
        }
    }
    let sites: Vec<Site> = sites
        .into_iter()
        .filter_map(|((start, Reverse(end), Reverse(kind)), alternatives)| {
            if matches!(kind, SiteKind::ArmGuard { .. }) && alternatives.len() > 1 {
                // Should be impossible: each arm is deleted by only one mutant.
                unswitched.extend(alternatives.iter().map(|(id, _)| *id));
                None
            } else {
                Some(Site {
                    range: start..end,
                    kind,
                    alternatives,
                })
            }
        })
        .collect();
    let mut renderer = Renderer {
        code,
        sites: &sites,
        next: 0,
        out: String::with_capacity(code.len() * 2),
        regions: Vec::new(),
        unswitched,
    };
    renderer.render_range(0..code.len());
    SchemataFile {
        code: renderer.out,
        unswitched: renderer.unswitched,
        regions: renderer.regions,
    }
}

/// Name that generated code uses for the standard library, so that it works in `no_std` crates.
const STD_ALIAS: &str = "__cargo_mutants_std";

/// Text to append to each crate root while the schemata are built, so that [`STD_ALIAS`]
/// names the standard library even in a `no_std` crate.
///
/// It's an alias so that it can't clash with the `std` most crates already have, and it goes
/// at the end so that it doesn't move any code whose compiler errors are mapped to mutants.
pub const CRATE_ROOT_SUFFIX: &str =
    "\n#[allow(unused_extern_crates)]\nextern crate std as __cargo_mutants_std;\n";

/// The code of [`active_expr`], with `$S` for the standard library, `$IDS` for the site's mutants,
/// and `$ACTIVE_ENV` and `$TRACE_ENV` for the environment variables.
const ACTIVE_TEMPLATE: &str = "{ \
    static ACTIVE: $S::sync::OnceLock<usize> = $S::sync::OnceLock::new(); \
    static TRACE: $S::sync::OnceLock<$S::option::Option<$S::ffi::OsString>> = \
    $S::sync::OnceLock::new(); \
    if let $S::option::Option::Some(dir) = TRACE.get_or_init(|| $S::env::var_os(\"$TRACE_ENV\")) { \
    $S::thread_local! { static TRACED: $S::cell::Cell<bool> = const { $S::cell::Cell::new(false) }; } \
    if !TRACED.try_with(|traced| traced.replace(true)).unwrap_or(true) { \
    let mut line = $S::env::var(\"NEXTEST_BINARY_ID\").unwrap_or_default(); \
    line.push('\\t'); \
    match $S::env::var(\"NEXTEST_TEST_NAME\") { \
    $S::result::Result::Ok(name) => line.push_str(&name), \
    $S::result::Result::Err(_) => line.push_str($S::thread::current().name().unwrap_or_default()), \
    } \
    line.push_str(\"\\t$IDS\\n\"); \
    let _ = $S::fs::OpenOptions::new().create(true).append(true)\
    .open($S::path::Path::new(dir).join($S::string::ToString::to_string(&$S::process::id())))\
    .and_then(|mut f| $S::io::Write::write_all(&mut f, line.as_bytes())); \
    } } \
    *ACTIVE.get_or_init(|| $S::env::var(\"$ACTIVE_ENV\").ok().and_then(|v| v.parse().ok()).unwrap_or(0)) }";

/// Expression evaluating to the active mutant id, or 0 for none, at a site holding these mutants.
///
/// This is self-contained so that it works anywhere in any module, and reads the environment
/// only once per site so that it's cheap inside loops.
///
/// If [`TRACE_ENV`] names a directory, the first time the site is reached on each thread it
/// appends a line to a file there naming the test and the site's mutants: the nextest test
/// from the environment, or else the thread's name, which libtest sets to the test's name.
/// See [`crate::test_select::Coverage`].
fn active_expr(ids: &[usize]) -> String {
    let ids = ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    ACTIVE_TEMPLATE
        .replace("$S", &format!("::{STD_ALIAS}"))
        .replace("$IDS", &ids)
        .replace("$ACTIVE_ENV", ACTIVE_ENV)
        .replace("$TRACE_ENV", TRACE_ENV)
}

struct Renderer<'a> {
    code: &'a str,
    /// Sites ordered by start, and then outermost first.
    sites: &'a [Site],
    /// Index of the next site to render.
    next: usize,
    out: String,
    regions: Vec<(Range<usize>, Vec<usize>)>,
    unswitched: Vec<usize>,
}

impl Renderer<'_> {
    /// Render a range of the original code, switching in all the sites within it.
    fn render_range(&mut self, range: Range<usize>) {
        let mut pos = range.start;
        while let Some(site) = self.sites.get(self.next) {
            if site.range.start >= range.end {
                break;
            }
            self.next += 1;
            if site.range.start < pos || site.range.end > range.end {
                // Overlaps a site already rendered, or the end of the enclosing site: this
                // shouldn't happen for syntax tree nodes, but if it does, leave it out.
                self.unswitched
                    .extend(site.alternatives.iter().map(|(id, _)| *id));
                continue;
            }
            self.out.push_str(&self.code[pos..site.range.start]);
            self.render_site(site);
            pos = site.range.end;
        }
        self.out.push_str(&self.code[pos..range.end]);
    }

    fn render_site(&mut self, site: &Site) {
        let region_start = self.out.len();
        let ids: Vec<usize> = site.alternatives.iter().map(|(id, _)| *id).collect();
        match site.kind {
            SiteKind::Expr { body } => {
                // The original comes first so that its line numbers change as little as possible.
                //
                // An expression is wrapped in a function call rather than parentheses, so that
                // it parses at the start of a statement and doesn't trigger `unused_parens`.
                if !body {
                    write!(self.out, "::{STD_ALIAS}::convert::identity({{ ").unwrap();
                }
                let not_active = ids
                    .iter()
                    .map(|id| format!("{ACTIVE_VAR} != {id}"))
                    .collect::<Vec<_>>()
                    .join(" && ");
                write!(
                    self.out,
                    "let {ACTIVE_VAR}: usize = {active}; if {not_active} {{ ",
                    active = active_expr(&ids)
                )
                .unwrap();
                self.render_range(site.range.clone());
                self.out.push_str(" }");
                let last = site.alternatives.len() - 1;
                for (i, (id, mutated)) in site.alternatives.iter().enumerate() {
                    if i == last {
                        self.out.push_str(" else { ");
                    } else {
                        write!(self.out, " else if {ACTIVE_VAR} == {id} {{ ").unwrap();
                    }
                    let start = self.out.len();
                    self.out.push_str(mutated);
                    self.regions.push((start..self.out.len(), vec![*id]));
                    self.out.push_str(" }");
                }
                if !body {
                    self.out.push_str(" })");
                }
            }
            SiteKind::ArmGuard { at } => {
                self.out.push_str(&self.code[site.range.start..at]);
                write!(self.out, " if {} != {}", ids[0], active_expr(&ids)).unwrap();
                self.render_range(at..site.range.end);
            }
        }
        self.regions.push((region_start..self.out.len(), ids));
    }
}

/// Return the byte offset of the start of each line.
fn line_starts(code: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(code.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

/// Convert a line and column, as used in [`crate::span::Span`], to a byte offset.
///
/// Columns count chars, not including `\r`. Returns None if the position isn't in the code.
fn byte_offset(code: &str, line_starts: &[usize], lc: LineColumn) -> Option<usize> {
    let line_start = *line_starts.get(lc.line.checked_sub(1)?)?;
    let line = &code[line_start..];
    let line = &line[..line.find('\n').unwrap_or(line.len())];
    let mut column = 1;
    for (i, c) in line.char_indices() {
        if column == lc.column {
            return Some(line_start + i);
        }
        if c != '\r' {
            column += 1;
        }
    }
    (column == lc.column).then_some(line_start + line.len())
}

/// The location of a compiler error.
#[derive(Debug, PartialEq, Eq)]
pub struct ErrorLocation {
    /// The file name as reported by the compiler, relative to the workspace or absolute.
    pub file_name: Utf8PathBuf,
    pub byte_offset: usize,
}

/// Find the locations of errors in cargo output that includes `--message-format=json` lines.
///
/// Each error gives a list of locations, most relevant first: its primary span, then its other
/// spans, then those of its notes and suggestions. The primary span isn't always in the code
/// that caused the error: a match made non-exhaustive by a deleted arm is reported at the
/// match's scrutinee, but suggests adding the arm back where it was.
///
/// Errors without a location, like "aborting due to previous errors", are skipped.
pub fn error_locations(cargo_output: &str) -> Vec<Vec<ErrorLocation>> {
    cargo_output
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|json| json["reason"] == "compiler-message")
        .map(|json| json["message"].clone())
        .filter(|message| message["level"] == "error")
        .map(|message| {
            let mut locations = Vec::new();
            add_span_locations(&message, &mut locations);
            locations
        })
        .filter(|locations| !locations.is_empty())
        .collect()
}

/// Add the locations of a diagnostic's spans, primary first, and then of its children.
fn add_span_locations(diagnostic: &Value, locations: &mut Vec<ErrorLocation>) {
    let spans = diagnostic["spans"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let (primary, secondary): (Vec<&Value>, Vec<&Value>) =
        spans.iter().partition(|span| span["is_primary"] == true);
    for span in primary.into_iter().chain(secondary) {
        if let (Some(file_name), Some(byte_offset)) = (
            span["file_name"].as_str(),
            span["byte_start"]
                .as_u64()
                .and_then(|b| usize::try_from(b).ok()),
        ) {
            locations.push(ErrorLocation {
                file_name: Utf8PathBuf::from(file_name),
                byte_offset,
            });
        }
    }
    for child in diagnostic["children"].as_array().into_iter().flatten() {
        add_span_locations(child, locations);
    }
}

/// Return the path of a file named in a compiler error relative to the build directory.
pub fn tree_relative_error_path<'a>(file_name: &'a Utf8Path, build_dir: &Utf8Path) -> &'a Utf8Path {
    file_name.strip_prefix(build_dir).unwrap_or(file_name)
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    use std::sync::Arc;

    use crate::Genre;
    use crate::mutant::Function;
    use crate::source::SourceFile;
    use crate::span::Span;

    use super::*;

    fn mutant(code: &str, span: Span, replacement: &str, genre: Genre, switch: Switch) -> Mutant {
        Mutant::new_discovered(
            SourceFile::for_tests("src/lib.rs", code, "cargo-mutants-testdata-schemata", true),
            Some(Arc::new(Function {
                function_name: "f".to_owned(),
                return_type: "-> u32".to_owned(),
                span: Span::quad(1, 1, 1, 1),
            })),
            span,
            Some("1".to_owned()),
            replacement.to_owned(),
            genre,
            None,
            Some(switch),
        )
    }

    #[test]
    fn byte_offset_counts_chars_and_skips_carriage_returns() {
        let code = "ab\r\ncé d\n";
        let starts = line_starts(code);
        let at = |line, column| byte_offset(code, &starts, LineColumn { line, column });
        assert_eq!(at(1, 1), Some(0));
        assert_eq!(at(1, 3), Some(2));
        assert_eq!(at(2, 3), Some(7)); // after the two-byte é
        assert_eq!(at(2, 5), Some(9)); // end of line
        assert_eq!(at(2, 6), None);
        assert_eq!(at(9, 1), None);
    }

    #[test]
    fn generate_binary_operator_schemata_keeps_original_first() {
        let code = "fn f(a: u32) -> bool { a == 1 }\n";
        let switch = Switch::Expr {
            span: Span::quad(1, 24, 1, 30),
            body: false,
        };
        let ne = mutant(
            code,
            Span::quad(1, 26, 1, 28),
            "!=",
            Genre::BinaryOperator,
            switch,
        );
        let schemata = generate(code, &[(3, &ne)]);
        assert_eq!(
            schemata.code,
            format!(
                "fn f(a: u32) -> bool {{ ::{STD_ALIAS}::convert::identity({{ let {ACTIVE_VAR}: usize = {}; \
                if {ACTIVE_VAR} != 3 {{ a == 1 }} else {{ a != 1 }} }}) }}\n",
                active_expr(&[3])
            )
        );
        assert_eq!(schemata.unswitched, [] as [usize; 0]);
        let mutated_at = schemata.code.find("a != 1").unwrap();
        assert_eq!(schemata.blame(mutated_at), Some([3].as_slice()));
        let original_at = schemata.code.find("a == 1").unwrap();
        assert_eq!(schemata.blame(original_at), Some([3].as_slice()));
        assert_eq!(schemata.blame(0), None);
    }

    #[test]
    fn generate_nests_operator_schemata_inside_function_body() {
        let code = "fn f(a: u32) -> u32 { a + 1 }\n";
        let body = Span::quad(1, 23, 1, 28);
        let fn_value = mutant(
            code,
            body,
            "0",
            Genre::FnValue,
            Switch::Expr {
                span: body,
                body: true,
            },
        );
        let sub = mutant(
            code,
            Span::quad(1, 25, 1, 26),
            "-",
            Genre::BinaryOperator,
            Switch::Expr {
                span: body,
                body: false,
            },
        );
        let schemata = generate(code, &[(1, &fn_value), (2, &sub)]);
        let active_1 = active_expr(&[1]);
        let active_2 = active_expr(&[2]);
        assert_eq!(
            schemata.code,
            format!(
                "fn f(a: u32) -> u32 {{ let {ACTIVE_VAR}: usize = {active_1}; \
                if {ACTIVE_VAR} != 1 {{ ::{STD_ALIAS}::convert::identity({{ let {ACTIVE_VAR}: usize = {active_2}; \
                if {ACTIVE_VAR} != 2 {{ a + 1 }} else {{ a - 1 }} }}) }} else {{ 0 }} }}\n"
            )
        );
        let inner_original = schemata.code.find("a + 1").unwrap();
        assert_eq!(schemata.blame(inner_original), Some([2].as_slice()));
        let outer_mutated = schemata.code.rfind("{ 0 }").unwrap() + 2;
        assert_eq!(schemata.blame(outer_mutated), Some([1].as_slice()));
    }

    #[test]
    fn generate_match_arm_schemata_adds_guard_after_pattern() {
        let code = indoc! {"
            fn f(a: u32) -> u32 {
                match a {
                    1 => 10,
                    _ => 0,
                }
            }
        "};
        let arm = mutant(
            code,
            Span::quad(3, 9, 3, 17),
            "",
            Genre::MatchArm,
            Switch::ArmGuard {
                at: LineColumn {
                    line: 3,
                    column: 10,
                },
            },
        );
        let schemata = generate(code, &[(5, &arm)]);
        assert!(
            schemata
                .code
                .contains(&format!("        1 if 5 != {} => 10,\n", active_expr(&[5]))),
            "{}",
            schemata.code
        );
    }

    #[test]
    fn generate_leaves_out_mutants_without_switch() {
        let code = "fn f() {}\n";
        let mut m = mutant(
            code,
            Span::quad(1, 8, 1, 10),
            "",
            Genre::FnValue,
            Switch::Expr {
                span: Span::quad(1, 8, 1, 10),
                body: true,
            },
        );
        m.switch = None;
        let schemata = generate(code, &[(1, &m)]);
        assert_eq!(schemata.code, code);
        assert_eq!(schemata.unswitched, [1]);
    }

    #[test]
    fn error_locations_lists_primary_then_other_spans_then_children() {
        let output = indoc! {r#"
            *** cargo test --no-run
            {"reason":"compiler-message","message":{"level":"warning","spans":[{"file_name":"src/a.rs","byte_start":1,"is_primary":true}],"children":[]}}
            {"reason":"compiler-message","message":{"level":"error","spans":[{"file_name":"src/a.rs","byte_start":5,"is_primary":false},{"file_name":"src/b.rs","byte_start":7,"is_primary":true}],"children":[{"level":"help","spans":[{"file_name":"src/b.rs","byte_start":9,"is_primary":true}],"children":[]}]}}
            {"reason":"compiler-message","message":{"level":"error","spans":[],"children":[]}}
            {"reason":"build-finished","success":false}
            error: could not compile
        "#};
        let location = |file_name: &str, byte_offset| ErrorLocation {
            file_name: file_name.into(),
            byte_offset,
        };
        assert_eq!(
            error_locations(output),
            [vec![
                location("src/b.rs", 7),
                location("src/a.rs", 5),
                location("src/b.rs", 9)
            ]]
        );
    }
}
