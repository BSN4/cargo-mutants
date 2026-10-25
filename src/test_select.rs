// Copyright 2026 Martin Pool

//! Choose which tests to run against each mutant, and in what order.
//!
//! In a schemata build every switched mutant site can record when it's reached. Running the
//! tests once under nextest, which runs each test in its own process, tells us which tests
//! reach each mutant. A mutant is then tested by running only those tests, and a mutant that
//! no test reaches is missed without running anything.
//!
//! Tests that caught other mutants in the same function are run first, in a separate
//! invocation, because they are the most likely to catch this one too.

#![warn(clippy::pedantic)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{read_dir, read_to_string};

use anyhow::Context;
use camino::Utf8Path;

use crate::Result;

/// Environment variable naming a directory where mutant sites record that they were reached.
pub const TRACE_ENV: &str = "CARGO_MUTANTS_SCHEMATA_TRACE";

/// The most tests to name in a filter: beyond this, it's simpler to run all of them.
const MAX_FILTERED_TESTS: usize = 200;

/// The most tests to run in the first, likely-to-fail, stage.
const MAX_FIRST_STAGE_TESTS: usize = 3;

/// A test identified the way nextest names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TestId {
    /// The nextest binary id, like `my-crate::integration`.
    pub binary_id: String,
    /// The test name within the binary, like `tests::addition`.
    pub name: String,
}

/// Which tests reach each mutant, read from the files written by a traced test run.
#[derive(Debug, Default)]
pub struct Coverage {
    tests_by_mutant: HashMap<usize, BTreeSet<TestId>>,
    /// Mutants reached from a process that's not attributed to a test, so we can't
    /// narrow down which tests to run.
    unattributed: HashSet<usize>,
}

impl Coverage {
    /// Read the trace files from a directory.
    ///
    /// Each line of each file is `binary_id \t test_name \t id id...`, written by code
    /// generated in [`crate::schemata`].
    pub fn read(trace_dir: &Utf8Path) -> Result<Coverage> {
        let mut coverage = Coverage::default();
        for entry in read_dir(trace_dir).with_context(|| format!("read {trace_dir}"))? {
            let path = entry?.path();
            let content = read_to_string(&path)
                .with_context(|| format!("read trace file {}", path.display()))?;
            coverage.add_trace(&content);
        }
        Ok(coverage)
    }

    fn add_trace(&mut self, content: &str) {
        for line in content.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(binary_id), Some(name), Some(ids)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let ids = ids.split(' ').filter_map(|id| id.parse::<usize>().ok());
            if binary_id.is_empty() || name.is_empty() {
                self.unattributed.extend(ids);
            } else {
                let test = TestId {
                    binary_id: binary_id.to_owned(),
                    name: name.to_owned(),
                };
                for id in ids {
                    self.tests_by_mutant
                        .entry(id)
                        .or_default()
                        .insert(test.clone());
                }
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

/// Make nextest arguments that run only these tests.
///
/// Returns None if the tests can't be named in a filter, in which case all tests should run.
pub fn filterset_args(tests: &[TestId]) -> Option<Vec<String>> {
    if tests.is_empty() || tests.len() > MAX_FILTERED_TESTS {
        return None;
    }
    // Rather than relying on nextest's escaping rules, leave out names with any
    // character that could be special in a filterset.
    let plain = |s: &str| {
        !s.is_empty()
            && !s
                .chars()
                .any(|c| c.is_whitespace() || "()|&,\\!~#=".contains(c))
    };
    let mut terms = Vec::with_capacity(tests.len());
    for test in tests {
        if !plain(&test.binary_id) || !plain(&test.name) {
            return None;
        }
        terms.push(format!(
            "(binary_id(={}) & test(={}))",
            test.binary_id, test.name
        ));
    }
    Some(vec!["-E".to_owned(), terms.join(" | ")])
}

/// Find the tests that nextest reports as failing or timing out in its output.
pub fn failed_tests(nextest_output: &str) -> Vec<TestId> {
    nextest_output
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let rest = line
                .strip_prefix("FAIL [")
                .or_else(|| line.strip_prefix("TIMEOUT ["))?;
            let (_duration, rest) = rest.split_once(']')?;
            let mut words = rest.split_whitespace();
            let binary_id = words.next()?;
            let name = words.next()?;
            Some(TestId {
                binary_id: binary_id.to_owned(),
                name: name.to_owned(),
            })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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
        TestId {
            binary_id: binary_id.to_owned(),
            name: name.to_owned(),
        }
    }

    #[test]
    fn coverage_add_trace_attributes_mutants_to_tests() {
        let mut coverage = Coverage::default();
        coverage.add_trace("pkg\tt::a\t1 2\npkg\tt::b\t2\n\t\t3\n");
        assert_eq!(coverage.tests_for(1), Some(vec![test_id("pkg", "t::a")]));
        assert_eq!(
            coverage.tests_for(2),
            Some(vec![test_id("pkg", "t::a"), test_id("pkg", "t::b")])
        );
        assert_eq!(coverage.tests_for(3), None, "unattributed");
        assert_eq!(coverage.tests_for(4), Some(vec![]), "not reached");
    }

    #[test]
    fn filterset_args_names_each_test() {
        assert_eq!(
            filterset_args(&[test_id("pkg::bin/x", "t::a"), test_id("pkg", "b")]),
            Some(vec![
                "-E".to_owned(),
                "(binary_id(=pkg::bin/x) & test(=t::a)) | (binary_id(=pkg) & test(=b))".to_owned()
            ])
        );
    }

    #[test]
    fn filterset_args_gives_up_on_special_characters() {
        assert_eq!(filterset_args(&[test_id("pkg", "case(1)")]), None);
        assert_eq!(filterset_args(&[test_id("pkg", "a b")]), None);
        assert_eq!(filterset_args(&[]), None);
    }

    #[test]
    fn failed_tests_reads_nextest_fail_and_timeout_lines() {
        let output = indoc! {"
                PASS [   0.004s] pkg tests::ok
                FAIL [   0.010s] pkg tests::bad
             TIMEOUT [  60.000s] pkg::bin/x tests::slow
            ------------
                FAIL [   0.010s] pkg tests::bad
        "};
        assert_eq!(
            failed_tests(output),
            [
                test_id("pkg", "tests::bad"),
                test_id("pkg::bin/x", "tests::slow")
            ]
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
