// Copyright 2026 Martin Pool

//! Choose which tests to run against each mutant, in what order, and for how long.
//!
//! In a schemata build every switched mutant site can record when it's reached. Running the
//! tests once with tracing turned on tells us which tests reach each mutant: under nextest each
//! test has its own process, named in the environment, and under `cargo test` libtest runs each
//! test on a thread named after it. A mutant is then tested by running only those tests, and a
//! mutant that no test reaches is missed without running anything.
//!
//! Tests that caught other mutants in the same function are run first, in a separate
//! invocation, because they are the most likely to catch this one too.
//!
//! If the baseline output gives each test's duration, a run of selected tests gets a timeout
//! based on their durations rather than the whole suite's.

#![warn(clippy::pedantic)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{read_dir, read_to_string};
use std::time::Duration;

use anyhow::Context;
use camino::Utf8Path;

use crate::Result;
use crate::options::TestTool;

/// Environment variable naming a directory where mutant sites record that they were reached.
pub const TRACE_ENV: &str = "CARGO_MUTANTS_SCHEMATA_TRACE";

/// The most tests to name in a filter: beyond this, it's simpler to run all of them.
const MAX_FILTERED_TESTS: usize = 200;

/// The most tests to run in the first, likely-to-fail, stage.
const MAX_FIRST_STAGE_TESTS: usize = 3;

/// A test, identified by its name and, under nextest, its binary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TestId {
    /// The nextest binary id, like `my-crate::integration`, or empty under `cargo test`,
    /// where a test is selected by name in every binary.
    pub binary_id: String,
    /// The test name within the binary, like `tests::addition`.
    pub name: String,
}

impl TestId {
    fn new(binary_id: &str, name: &str) -> TestId {
        TestId {
            binary_id: binary_id.to_owned(),
            name: name.to_owned(),
        }
    }
}

/// Which tests reach each mutant, read from the files written by a traced test run.
#[derive(Debug, Default)]
pub struct Coverage {
    tests_by_mutant: HashMap<usize, BTreeSet<TestId>>,
    /// Mutants reached from a process or thread that's not a known test, such as a doctest
    /// or a thread a test spawned, so we can't narrow down which tests to run.
    unattributed: HashSet<usize>,
}

impl Coverage {
    /// Read the trace files from a directory.
    ///
    /// Each line of each file is `binary_id \t test_name \t id id...`, written by code
    /// generated in [`crate::schemata`]. Under `cargo test` the binary id is empty and the
    /// test name is the name of the thread, which is only a test if it's in `known_tests`.
    pub fn read(trace_dir: &Utf8Path, known_tests: Option<&HashSet<String>>) -> Result<Coverage> {
        let mut coverage = Coverage::default();
        for entry in read_dir(trace_dir).with_context(|| format!("read {trace_dir}"))? {
            let path = entry?.path();
            let content = read_to_string(&path)
                .with_context(|| format!("read trace file {}", path.display()))?;
            coverage.add_trace(&content, known_tests);
        }
        Ok(coverage)
    }

    fn add_trace(&mut self, content: &str, known_tests: Option<&HashSet<String>>) {
        for line in content.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(binary_id), Some(name), Some(ids)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let ids = ids.split(' ').filter_map(|id| id.parse::<usize>().ok());
            let is_test = !name.is_empty()
                && (!binary_id.is_empty() || known_tests.is_some_and(|known| known.contains(name)));
            if is_test {
                let test = TestId::new(binary_id, name);
                for id in ids {
                    self.tests_by_mutant
                        .entry(id)
                        .or_default()
                        .insert(test.clone());
                }
            } else {
                self.unattributed.extend(ids);
            }
        }
    }

    /// Return the tests that reach a mutant, or None if they're not known.
    ///
    /// An empty list means no test reaches the mutant.
    pub fn tests_for(&self, id: usize) -> Option<Vec<TestId>> {
        if self.unattributed.contains(&id) {
            None
        } else {
            Some(
                self.tests_by_mutant
                    .get(&id)
                    .map(|tests| tests.iter().cloned().collect())
                    .unwrap_or_default(),
            )
        }
    }
}

/// Make arguments that run only the `selected` tests, or all if None, apart from `skipped`.
///
/// Under `cargo test`, the arguments start with `--`, and the ones after it are for the test
/// binaries. Returns None if the tests can't be named, or there are too many to name.
pub fn test_args(
    test_tool: TestTool,
    selected: Option<&[TestId]>,
    skipped: &[TestId],
) -> Option<Vec<String>> {
    if selected.is_some_and(|tests| tests.is_empty() || tests.len() > MAX_FILTERED_TESTS) {
        return None;
    }
    match test_tool {
        TestTool::Cargo => {
            // libtest takes any number of names to filter by, and to skip, as separate
            // arguments; under cargo test a name selects that test in every binary.
            let names = |tests: &[TestId]| {
                tests
                    .iter()
                    .map(|test| test.name.clone())
                    .collect::<BTreeSet<_>>()
            };
            let mut args = Vec::new();
            if let Some(selected) = selected {
                args.extend(names(selected));
            }
            for name in names(skipped) {
                args.extend(["--skip".to_owned(), name]);
            }
            if !args.is_empty() {
                args.splice(0..0, ["--".to_owned(), "--exact".to_owned()]);
            }
            Some(args)
        }
        TestTool::Nextest => {
            let selected = match selected {
                Some(tests) => Some(nextest_terms(tests)?),
                None => None,
            };
            let skipped = if skipped.is_empty() {
                None
            } else {
                Some(nextest_terms(skipped)?)
            };
            let expr = match (selected, skipped) {
                (None, None) => return Some(Vec::new()),
                (Some(selected), None) => selected,
                (None, Some(skipped)) => format!("not ({skipped})"),
                (Some(selected), Some(skipped)) => format!("({selected}) & not ({skipped})"),
            };
            Some(vec!["-E".to_owned(), expr])
        }
    }
}

/// A nextest filterset matching any of these tests.
///
/// Returns None if any name has a character that could be special in a filterset, rather than
/// relying on nextest's escaping rules.
fn nextest_terms(tests: &[TestId]) -> Option<String> {
    let plain = |s: &str| {
        !s.chars()
            .any(|c| c.is_whitespace() || "()|&,\\!~#=".contains(c))
    };
    let mut terms = Vec::with_capacity(tests.len());
    for test in tests {
        if test.name.is_empty() || !plain(&test.binary_id) || !plain(&test.name) {
            return None;
        }
        if test.binary_id.is_empty() {
            terms.push(format!("test(={})", test.name));
        } else {
            terms.push(format!(
                "(binary_id(={}) & test(={}))",
                test.binary_id, test.name
            ));
        }
    }
    Some(terms.join(" | "))
}

/// One test's result line from nextest or libtest output.
struct ResultLine {
    test: TestId,
    passed: bool,
    duration: Option<Duration>,
}

/// Parse the test result lines in nextest or libtest output.
///
/// nextest writes `PASS [   0.004s] (1/9) binary_id test_name`, with the `(1/9)` counter in
/// recent versions. libtest writes `test test_name ... ok`, followed by `<0.004s>` with
/// `--report-time`.
fn result_lines(output: &str) -> impl Iterator<Item = ResultLine> + '_ {
    output.lines().filter_map(|line| {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("test ")
            && let Some((name, result)) = rest.split_once(" ... ")
        {
            let mut words = result.split_whitespace();
            let passed = match words.next()? {
                "ok" => true,
                "FAILED" => false,
                _ => return None,
            };
            let duration = words
                .next()
                .and_then(|w| w.strip_prefix('<')?.strip_suffix("s>")?.parse::<f64>().ok())
                .map(Duration::from_secs_f64);
            return Some(ResultLine {
                test: TestId::new("", name),
                passed,
                duration,
            });
        }
        let (status, rest) = trimmed.split_once(" [")?;
        let passed = match status {
            "PASS" => true,
            "FAIL" | "TIMEOUT" | "SIGSEGV" | "SIGABRT" | "SIGKILL" | "SIGTERM" | "ABORT" => false,
            _ => return None,
        };
        let (duration, rest) = rest.split_once(']')?;
        let duration = duration
            .trim()
            .strip_suffix('s')
            .and_then(|d| d.parse::<f64>().ok())
            .map(Duration::from_secs_f64);
        let mut rest = rest.trim_start();
        if rest.starts_with('(') {
            rest = rest.split_once(')')?.1.trim_start();
        }
        let mut words = rest.split_whitespace();
        let binary_id = words.next()?;
        let name = words.next()?;
        Some(ResultLine {
            test: TestId::new(binary_id, name),
            passed,
            duration,
        })
    })
}

/// Find the tests reported as failing or timing out in nextest or libtest output.
pub fn failed_tests(output: &str) -> Vec<TestId> {
    result_lines(output)
        .filter(|result| !result.passed)
        .map(|result| result.test)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Find the durations of the tests that passed, from nextest or libtest output.
///
/// libtest only reports durations with `-Zunstable-options --report-time`.
pub fn test_durations(output: &str) -> HashMap<TestId, Duration> {
    let mut durations = HashMap::new();
    for result in result_lines(output).filter(|result| result.passed) {
        if let Some(duration) = result.duration {
            // The same name in two binaries, under cargo test: keep the slower.
            let entry = durations.entry(result.test).or_insert(duration);
            *entry = (*entry).max(duration);
        }
    }
    durations
}

/// Find the names of the tests listed by `cargo test -- --list`.
pub fn listed_tests(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

/// The total duration of these tests in the baseline, if all of them are known.
pub fn total_duration(durations: &HashMap<TestId, Duration>, tests: &[TestId]) -> Option<Duration> {
    tests.iter().map(|test| durations.get(test)).sum()
}

/// Which tests have caught mutants so far in this run.
#[derive(Debug, Default)]
pub struct KillHistory {
    kills: HashMap<TestId, usize>,
    by_function: HashMap<String, HashSet<TestId>>,
}

impl KillHistory {
    /// Record that these tests caught a mutant in a function.
    pub fn record(&mut self, function_name: Option<&str>, tests: &[TestId]) {
        for test in tests {
            *self.kills.entry(test.clone()).or_default() += 1;
            if let Some(function_name) = function_name {
                self.by_function
                    .entry(function_name.to_owned())
                    .or_default()
                    .insert(test.clone());
            }
        }
    }

    /// Split the tests for a mutant into stages to run in order, stopping at the first that fails.
    ///
    /// The first stage is the tests that caught other mutants in the same function, or
    /// else the tests that caught the most mutants overall. The second is everything else.
    pub fn stages(&self, function_name: Option<&str>, tests: Vec<TestId>) -> Vec<Vec<TestId>> {
        let same_function = function_name.and_then(|name| self.by_function.get(name));
        let kills = |test: &TestId| self.kills.get(test).copied().unwrap_or(0);
        let mut likely: Vec<TestId> = tests
            .iter()
            .filter(|test| same_function.is_some_and(|killers| killers.contains(test)))
            .cloned()
            .collect();
        if likely.is_empty() {
            likely = tests.iter().filter(|t| kills(t) > 0).cloned().collect();
        }
        likely.sort_by_key(|test| std::cmp::Reverse(kills(test)));
        likely.truncate(MAX_FIRST_STAGE_TESTS);
        if likely.is_empty() || likely.len() == tests.len() {
            return vec![tests];
        }
        let rest = tests.into_iter().filter(|t| !likely.contains(t)).collect();
        vec![likely, rest]
    }
}

#[cfg(test)]
mod test {
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    use super::*;

    fn test_id(binary_id: &str, name: &str) -> TestId {
        TestId::new(binary_id, name)
    }

    #[test]
    fn coverage_add_trace_attributes_mutants_to_nextest_tests() {
        let mut coverage = Coverage::default();
        coverage.add_trace("pkg\tt::a\t1 2\npkg\tt::b\t2\n\t\t3\n", None);
        assert_eq!(coverage.tests_for(1), Some(vec![test_id("pkg", "t::a")]));
        assert_eq!(
            coverage.tests_for(2),
            Some(vec![test_id("pkg", "t::a"), test_id("pkg", "t::b")])
        );
        assert_eq!(coverage.tests_for(3), None, "unattributed");
        assert_eq!(coverage.tests_for(4), Some(vec![]), "not reached");
    }

    #[test]
    fn coverage_add_trace_attributes_cargo_test_threads_only_if_they_are_tests() {
        let known = HashSet::from(["t::a".to_owned()]);
        let mut coverage = Coverage::default();
        coverage.add_trace(
            "\tt::a\t1\n\ttokio-runtime-worker\t2\n\tmain\t3\n\tt::a\t3\n",
            Some(&known),
        );
        assert_eq!(coverage.tests_for(1), Some(vec![test_id("", "t::a")]));
        assert_eq!(coverage.tests_for(2), None, "reached from a worker thread");
        assert_eq!(
            coverage.tests_for(3),
            None,
            "reached from a doctest's main thread"
        );
    }

    #[test]
    fn test_args_for_nextest_names_each_test() {
        assert_eq!(
            test_args(
                TestTool::Nextest,
                Some(&[test_id("pkg::bin/x", "t::a"), test_id("pkg", "b")]),
                &[]
            ),
            Some(vec![
                "-E".to_owned(),
                "(binary_id(=pkg::bin/x) & test(=t::a)) | (binary_id(=pkg) & test(=b))".to_owned()
            ])
        );
    }

    #[test]
    fn test_args_for_nextest_skips_tests() {
        let skipped = [test_id("pkg", "zero_alloc")];
        assert_eq!(
            test_args(TestTool::Nextest, None, &skipped),
            Some(vec![
                "-E".to_owned(),
                "not ((binary_id(=pkg) & test(=zero_alloc)))".to_owned()
            ])
        );
        assert_eq!(
            test_args(TestTool::Nextest, Some(&[test_id("pkg", "a")]), &skipped),
            Some(vec![
                "-E".to_owned(),
                "((binary_id(=pkg) & test(=a))) & not ((binary_id(=pkg) & test(=zero_alloc)))"
                    .to_owned()
            ])
        );
        assert_eq!(test_args(TestTool::Nextest, None, &[]), Some(vec![]));
    }

    #[test]
    fn test_args_for_nextest_gives_up_on_special_characters() {
        let select = |name| test_args(TestTool::Nextest, Some(&[test_id("pkg", name)]), &[]);
        assert_eq!(select("case(1)"), None);
        assert_eq!(select("a b"), None);
        assert_eq!(test_args(TestTool::Nextest, Some(&[]), &[]), None);
    }

    #[test]
    fn test_args_for_cargo_test_passes_exact_names_to_the_test_binaries() {
        let strings = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            test_args(
                TestTool::Cargo,
                Some(&[
                    test_id("", "t::b"),
                    test_id("", "t::a"),
                    test_id("", "t::b")
                ]),
                &[test_id("", "zero_alloc")]
            ),
            Some(strings(&[
                "--",
                "--exact",
                "t::a",
                "t::b",
                "--skip",
                "zero_alloc"
            ]))
        );
        assert_eq!(
            test_args(TestTool::Cargo, None, &[test_id("", "zero_alloc")]),
            Some(strings(&["--", "--exact", "--skip", "zero_alloc"]))
        );
        assert_eq!(test_args(TestTool::Cargo, None, &[]), Some(vec![]));
    }

    #[test]
    fn failed_tests_reads_nextest_lines_with_and_without_counters() {
        let output = indoc! {"
                PASS [   0.004s] (1/4) pkg tests::ok
                FAIL [   0.010s] (2/4) pkg tests::bad
             TIMEOUT [  60.000s] ( 3/14) pkg::bin/x tests::slow
                FAIL [   0.010s] pkg tests::old_format
            ------------
                FAIL [   0.010s] (2/4) pkg tests::bad
        "};
        assert_eq!(
            failed_tests(output),
            [
                test_id("pkg", "tests::bad"),
                test_id("pkg", "tests::old_format"),
                test_id("pkg::bin/x", "tests::slow"),
            ]
        );
    }

    #[test]
    fn failed_tests_reads_libtest_lines() {
        let output = indoc! {"
            running 3 tests
            test tests::ok ... ok
            test tests::bad ... FAILED
            test tests::skipped ... ignored
        "};
        assert_eq!(failed_tests(output), [test_id("", "tests::bad")]);
    }

    #[test]
    fn test_durations_reads_nextest_and_libtest_report_time() {
        let output = indoc! {"
                PASS [   0.250s] (1/2) pkg tests::a
                FAIL [   0.010s] (2/2) pkg tests::bad
            test tests::b ... ok <1.500s>
            test tests::b ... ok <0.500s>
            test tests::c ... ok
        "};
        let durations = test_durations(output);
        assert_eq!(
            durations.get(&test_id("pkg", "tests::a")),
            Some(&Duration::from_millis(250))
        );
        assert_eq!(
            durations.get(&test_id("", "tests::b")),
            Some(&Duration::from_millis(1500))
        );
        assert_eq!(durations.len(), 2);
        assert_eq!(
            total_duration(
                &durations,
                &[test_id("pkg", "tests::a"), test_id("", "tests::b")]
            ),
            Some(Duration::from_millis(1750))
        );
        assert_eq!(total_duration(&durations, &[test_id("", "tests::c")]), None);
    }

    #[test]
    fn listed_tests_reads_cargo_test_list_output() {
        let output = indoc! {"
                 Running unittests src/lib.rs (target/debug/deps/x-123)
            tests::a: test
            tests::bench: benchmark
            src/lib.rs - f (line 3): test
            2 tests, 1 benchmark
        "};
        assert_eq!(
            listed_tests(output),
            HashSet::from(["tests::a".to_owned(), "src/lib.rs - f (line 3)".to_owned()])
        );
    }

    #[test]
    fn kill_history_stages_runs_same_function_killers_first() {
        let mut history = KillHistory::default();
        let a = test_id("pkg", "a");
        let b = test_id("pkg", "b");
        let c = test_id("pkg", "c");
        history.record(Some("f"), std::slice::from_ref(&b));
        history.record(Some("g"), std::slice::from_ref(&c));
        history.record(Some("g"), std::slice::from_ref(&c));
        assert_eq!(
            history.stages(Some("f"), vec![a.clone(), b.clone(), c.clone()]),
            [vec![b.clone()], vec![a.clone(), c.clone()]]
        );
        // With no killers in this function, the most effective tests go first.
        assert_eq!(
            history.stages(Some("h"), vec![a.clone(), b.clone(), c.clone()]),
            [vec![c.clone(), b.clone()], vec![a.clone()]]
        );
    }

    #[test]
    fn kill_history_stages_runs_everything_at_once_without_history() {
        let tests = vec![test_id("pkg", "a"), test_id("pkg", "b")];
        assert_eq!(
            KillHistory::default().stages(Some("f"), tests.clone()),
            [tests]
        );
    }
}
