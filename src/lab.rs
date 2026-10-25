// Copyright 2021-2025 Martin Pool

//! Successively apply mutations to the source code and run cargo to check,
//! build, and test them.

#![warn(clippy::pedantic)]

use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{create_dir_all, read_to_string, remove_dir_all};
use std::panic::resume_unwind;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::{thread, vec};

use camino::{Utf8Path, Utf8PathBuf};
use itertools::Itertools;
use jiff::Timestamp;
use tracing::{debug, debug_span, error, info, trace, warn};

use crate::{
    BaselineStrategy, BuildDir, Console, Context, Mutant, Options, Phase, Result, Scenario,
    ScenarioOutcome,
    cache::{CachedOutcome, ResultCache, cache_key, tests_fingerprint},
    cargo::{json_message_format_args, run_cargo},
    console::plural,
    options::{TestPackages, TestTool},
    outcome::{LabOutcome, PhaseResult},
    output::OutputDir,
    package::Package,
    package::PackageSelection,
    process::Exit,
    schemata::{self, ACTIVE_ENV},
    test_select::{
        Coverage, KillHistory, TRACE_ENV, TestId, failed_tests, listed_tests, test_args,
        test_durations, total_duration,
    },
    timeouts::{Timeouts, selected_tests_timeout},
    workspace::Workspace,
};

/// The most times to build the schemata, leaving out mutants that cause errors each time.
const MAX_SCHEMATA_ROUNDS: usize = 10;

/// The most times to run the tests in the schemata build with no mutant active, leaving out
/// tests that fail each time.
const MAX_SCHEMATA_BASELINE_ROUNDS: usize = 5;

/// Run all possible mutation experiments.
///
/// This is called after all filtering is complete, so all the mutants here will be tested
/// or checked.
///
/// Before testing the mutants, the lab checks that the source tree passes its tests with no
/// mutations applied.
///
/// If `cache` is given, mutants with a cached outcome aren't tested again, and the outcomes
/// of this run are added to the cache.
pub fn test_mutants(
    mut mutants: Vec<Mutant>,
    workspace: &Workspace,
    output_dir: OutputDir,
    options: &Options,
    console: &Console,
    cache: Option<ResultCache>,
) -> Result<LabOutcome> {
    let start_time = Instant::now();
    console.set_debug_log(output_dir.open_debug_log()?);
    if options.shuffle {
        fastrand::shuffle(&mut mutants);
    }
    output_dir.write_mutants_list(&mutants)?;
    console.discovered_mutants(&mutants);
    if mutants.is_empty() {
        warn!("No mutants found under the active filters");
        return Ok(LabOutcome::new(Timestamp::now()));
    }
    let output_mutex = Mutex::new(output_dir);
    let jobserver = options
        .jobserver
        .then(|| {
            let n_tasks = options.jobserver_tasks.unwrap_or_else(num_cpus::get);
            debug!(n_tasks, "starting jobserver");
            jobserver::Client::new(n_tasks)
        })
        .transpose()
        .context("Start jobserver")?;
    let tests_for_mutant = TestsForMutant::new(options, workspace);
    let cache = cache
        .map(|results| LabCache::new(results, workspace, &mutants))
        .transpose()?;
    let lab = Lab {
        output_mutex,
        jobserver,
        tests_for_mutant,
        kill_history: Mutex::default(),
        test_durations: OnceLock::new(),
        schemata_skipped_tests: OnceLock::new(),
        schemata_recheck: Mutex::default(),
        cache,
        options,
        console,
    };
    let mutants = lab.reuse_cached_outcomes(mutants)?;
    if mutants.is_empty() {
        info!("All mutants had cached outcomes, so nothing needs to be built or tested");
        let output_dir = lab.into_output_dir()?;
        console.lab_finished(&output_dir.lab_outcome, start_time, options);
        return output_dir.finish();
    }
    let baseline_build_dir = BuildDir::for_baseline(workspace, options, console)?;
    let timeouts = match options.baseline {
        BaselineStrategy::Run => {
            let outcome = lab.run_baseline(&baseline_build_dir, &mutants)?;
            if outcome.success() {
                Timeouts::from_baseline(&outcome, options)
            } else {
                error!(
                    "cargo {phase} failed in an unmutated tree, so no mutants were tested",
                    phase = outcome.last_phase(),
                );
                return lab.into_output_dir()?.finish();
            }
        }
        BaselineStrategy::Skip => Timeouts::without_baseline(options),
    };
    debug!(?timeouts);

    console.start_testing_mutants(mutants.len());
    let mutants = if options.schemata {
        lab.test_schemata(&baseline_build_dir, mutants, timeouts)?
    } else {
        mutants
    };

    let build_dir_0 = Mutex::new(Some(baseline_build_dir));
    // Create n threads, each dedicated to one build directory. Each of them tries to take a
    // scenario to test off the queue, and then exits when there are no more left.
    let n_threads = max(1, min(options.jobs.unwrap_or(1), mutants.len()));
    let jobs = mutants.into_iter().map(Job::separate).collect_vec();
    let work_queue = &Mutex::new(jobs.into_iter());
    thread::scope(|scope| -> crate::Result<()> {
        let mut threads = Vec::new();
        for _i_thread in 0..n_threads {
            threads.push(scope.spawn(|| -> crate::Result<()> {
                trace!(thread_id = ?thread::current().id(), "start thread");
                // First thread to start can use the baseline's build dir;
                // others need to copy a new one
                let build_dir_0 = build_dir_0.lock().expect("lock build dir 0").take(); // separate for lock
                let build_dir = &if let Some(d) = build_dir_0 {
                    d
                } else {
                    BuildDir::copy_from(workspace.root(), options, console)?
                };
                lab.make_worker(build_dir, build_dir.path().to_owned())
                    .run_queue(work_queue, timeouts)
            }));
        }
        join_threads(threads)
    })?;

    let output_dir = lab.into_output_dir()?;
    console.lab_finished(&output_dir.lab_outcome, start_time, options);
    let lab_outcome = output_dir.finish()?;
    if lab_outcome.total_mutants == 0 {
        // This should be unreachable as we also bail out before copying
        // the tree if no mutants are generated.
        warn!("No mutants were generated");
    } else if lab_outcome.unviable == lab_outcome.total_mutants {
        warn!(
            "No mutants were viable: perhaps there is a problem with building in a scratch directory. Look in mutants.out/log/* for more information."
        );
    }
    Ok(lab_outcome)
}

#[mutants::skip] // it's a little hard to observe that the threads were collected?
fn join_threads(threads: Vec<thread::ScopedJoinHandle<'_, Result<()>>>) -> Result<()> {
    // The errors potentially returned from `join` are a special `std::thread::Result`
    // that does not implement error, indicating that the thread panicked.
    // Probably the most useful thing is to `resume_unwind` it.
    // Inside that, there's an actual Mutants error indicating a non-panic error.
    // Most likely, this would be "interrupted" but it might be some IO error
    // etc. In that case, print them all and return the first.
    let errors = threads
        .into_iter()
        .filter_map(|thread| match thread.join() {
            Err(panic) => resume_unwind(panic),
            Ok(Ok(())) => None,
            Ok(Err(err)) => {
                // To avoid console spam don't print "interrupted" errors for each thread,
                // since that should have been printed by check_interrupted but do return them.
                if err.to_string() != "interrupted" {
                    error!("Worker thread failed: {:?}", err);
                }
                Some(err)
            }
        })
        .collect_vec();
    if let Some(first_err) = errors.into_iter().next() {
        Err(first_err)
    } else {
        Ok(())
    }
}

/// Options for a test run that doesn't stop at the first failure, at either cargo's or libtest's
/// level, so that one run finds every test that fails, and every test's duration.
fn without_fail_fast(options: &Options) -> Options {
    let mut options = options.clone();
    options
        .additional_cargo_args
        .push("--no-fail-fast".to_owned());
    let test_args = &mut options.additional_cargo_test_args;
    if let Some(separator) = test_args.iter().position(|a| a == "--") {
        let binary_args = test_args.split_off(separator);
        test_args.extend(binary_args.into_iter().filter(|a| a != "--fail-fast"));
    }
    options
}

/// Return the packages containing any of these mutants, in a stable order.
fn mutated_packages(mutants: &[Mutant]) -> Vec<Arc<Package>> {
    mutants
        .iter()
        .map(|m| Arc::clone(&m.source_file.package))
        .sorted_by_key(|p| p.name.clone())
        .unique()
        .collect_vec()
}

/// A mutant waiting to be tested.
struct Job {
    mutant: Mutant,
    /// The mutant's id in the schemata build, if it's switched on at runtime there rather
    /// than being built separately.
    schemata_id: Option<usize>,
    /// The tests that reach this mutant, if known from the schemata coverage run.
    tests: Option<Vec<TestId>>,
}

impl Job {
    /// A mutant that will be applied to the source and built on its own.
    fn separate(mutant: Mutant) -> Job {
        Job {
            mutant,
            schemata_id: None,
            tests: None,
        }
    }
}

/// The result of running the tests in the schemata build with no mutant active.
enum SchemataBaseline {
    /// The tests failed, so the schemata can't be used.
    Failed,
    /// The tests passed, and the tests reaching each mutant were recorded.
    Passed { coverage: Coverage },
}

/// The result of a cargo command that's part of setting up the schemata.
struct TaskResult {
    success: bool,
    log: String,
    duration: Duration,
}

/// How long tests took in the schemata baseline, to set timeouts for running some of them.
struct TestDurations {
    durations: HashMap<TestId, Duration>,
    /// The time a run of tests takes apart from the tests themselves.
    overhead: Duration,
}

/// The results cache, and what's needed to compute keys into it.
struct LabCache {
    results: Mutex<ResultCache>,
    /// Fingerprints of the integration tests, by package name.
    tests_fingerprints: HashMap<String, u64>,
}

impl LabCache {
    fn new(results: ResultCache, workspace: &Workspace, mutants: &[Mutant]) -> Result<LabCache> {
        let tests_fingerprints = mutated_packages(mutants)
            .into_iter()
            .map(|package| {
                let dir = workspace.root().join(&package.relative_dir);
                Ok((package.name.clone(), tests_fingerprint(&dir)?))
            })
            .collect::<Result<_>>()?;
        Ok(LabCache {
            results: Mutex::new(results),
            tests_fingerprints,
        })
    }

    fn key(&self, mutant: &Mutant) -> String {
        let fingerprint = self
            .tests_fingerprints
            .get(&mutant.source_file.package.name)
            .copied()
            .unwrap_or_default();
        cache_key(mutant, fingerprint)
    }
}

/// Common context across all scenarios, threads, and build dirs.
struct Lab<'a> {
    output_mutex: Mutex<OutputDir>,
    jobserver: Option<jobserver::Client>,
    tests_for_mutant: TestsForMutant,
    /// Which tests caught which mutants, to choose which tests to run first.
    kill_history: Mutex<KillHistory>,
    /// How long each test took in the schemata baseline, once it has run.
    test_durations: OnceLock<TestDurations>,
    /// Tests that fail in the schemata build with no mutant active, and so are skipped there.
    schemata_skipped_tests: OnceLock<Vec<TestId>>,
    /// Mutants missed in the schemata build while some tests were skipped, to be checked again
    /// in a build of their own.
    schemata_recheck: Mutex<Vec<Mutant>>,
    cache: Option<LabCache>,
    options: &'a Options,
    console: &'a Console,
}

impl Lab<'_> {
    /// Run the baseline scenario, which is the same as running `cargo test` on the unmutated
    /// tree.
    ///
    /// If it fails, return None, indicating that no further testing should be done.
    ///
    /// If it succeeds, return the timeouts to be used for the other scenarios.
    fn run_baseline(&self, build_dir: &BuildDir, mutants: &[Mutant]) -> Result<ScenarioOutcome> {
        self.make_worker(build_dir, build_dir.path().to_owned())
            .run_one_scenario(
                &Scenario::Baseline,
                &PackageSelection::Explicit(mutated_packages(mutants)),
                Timeouts::for_baseline(self.options),
            )
    }

    fn make_worker<'a>(&'a self, build_dir: &'a BuildDir, console_key: Utf8PathBuf) -> Worker<'a> {
        Worker {
            lab: self,
            build_dir,
            console_key,
        }
    }

    /// Release the output directory, after saving the results cache into it.
    fn into_output_dir(self) -> Result<OutputDir> {
        let output_dir = self.output_mutex.into_inner().expect("unlock output dir");
        if let Some(cache) = self.cache {
            cache
                .results
                .into_inner()
                .expect("unlock cache")
                .save(output_dir.path())?;
        }
        Ok(output_dir)
    }

    /// Record the outcomes of mutants that are in the results cache.
    ///
    /// Returns the mutants that still need to be tested.
    fn reuse_cached_outcomes(&self, mutants: Vec<Mutant>) -> Result<Vec<Mutant>> {
        let Some(cache) = &self.cache else {
            return Ok(mutants);
        };
        let mut remaining = Vec::with_capacity(mutants.len());
        let mut n_reused = 0;
        for mutant in mutants {
            let cached = cache
                .results
                .lock()
                .expect("lock cache")
                .get(&cache.key(&mutant));
            if let Some(cached) = cached {
                self.record_cached_outcome(mutant, cached)?;
                n_reused += 1;
            } else {
                remaining.push(mutant);
            }
        }
        if n_reused > 0 {
            info!(
                "Reused cached outcomes for {}",
                plural(n_reused, "unchanged mutant")
            );
        }
        Ok(remaining)
    }

    fn record_cached_outcome(&self, mutant: Mutant, cached: CachedOutcome) -> Result<()> {
        let scenario = Scenario::Mutant(mutant);
        let mut scenario_output = self
            .output_mutex
            .lock()
            .expect("lock output_dir to start scenario")
            .start_scenario(&scenario)?;
        let mutant = scenario.mutant().expect("mutant scenario");
        scenario_output.write_diff(&mutant.diff(&mutant.mutated_code()))?;
        scenario_output.message(&format!(
            "Reused outcome {cached:?} from the results cache: the code hasn't changed"
        ))?;
        let (phase, process_status) = match cached {
            CachedOutcome::Caught => (Phase::Test, Exit::Failure(101)),
            CachedOutcome::Missed => (Phase::Test, Exit::Success),
            CachedOutcome::Unviable => (Phase::Build, Exit::Failure(101)),
        };
        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        outcome.add_phase_result(PhaseResult {
            phase,
            duration: Duration::ZERO,
            process_status,
            argv: Vec::new(),
        });
        self.finish_scenario(Utf8Path::new("cache"), &scenario, &outcome)
    }

    /// Record the outcome of a scenario in the output directory, cache, and console.
    fn finish_scenario(
        &self,
        console_key: &Utf8Path,
        scenario: &Scenario,
        outcome: &ScenarioOutcome,
    ) -> Result<()> {
        self.output_mutex
            .lock()
            .expect("lock output dir to add outcome")
            .add_scenario_outcome(outcome)?;
        if let (Some(cache), Some(mutant)) = (&self.cache, scenario.mutant())
            && let Some(cached) = CachedOutcome::from_summary(&outcome.summary())
        {
            let key = cache.key(mutant);
            cache
                .results
                .lock()
                .expect("lock cache")
                .insert(key, cached);
        }
        debug!(outcome = ?outcome.summary());
        self.console
            .scenario_finished(console_key, scenario, outcome, self.options);
        Ok(())
    }

    /// Which packages to build in the schemata so that every mutant's tests can run.
    fn schemata_packages(&self, mutants: &[Mutant]) -> PackageSelection {
        match &self.tests_for_mutant {
            TestsForMutant::Workspace => PackageSelection::All,
            TestsForMutant::Mutated => PackageSelection::Explicit(mutated_packages(mutants)),
            TestsForMutant::Explicit(packages) => PackageSelection::Explicit(packages.clone()),
        }
    }

    /// Test the mutants that can be compiled together into one schemata build.
    ///
    /// The source files in the build dir are restored afterwards. Returns the mutants that
    /// still need to be built and tested separately.
    fn test_schemata(
        &self,
        build_dir: &BuildDir,
        mutants: Vec<Mutant>,
        timeouts: Timeouts,
    ) -> Result<Vec<Mutant>> {
        if self.options.check_only {
            warn!("--schemata has no effect with --check");
            return Ok(mutants);
        }
        // The original text of every file the schemata change: those with switched mutants,
        // and the crate roots, which get an alias for the standard library.
        let switched = mutants
            .iter()
            .filter(|mutant| mutant.switch.is_some())
            .cloned()
            .collect_vec();
        let mut originals: BTreeMap<Utf8PathBuf, String> = BTreeMap::new();
        for mutant in &switched {
            originals
                .entry(mutant.source_file.tree_relative_path.clone())
                .or_insert_with(|| mutant.source_file.code().to_owned());
        }
        let crate_roots: BTreeSet<Utf8PathBuf> = mutated_packages(&switched)
            .iter()
            .flat_map(|package| package.top_sources.iter().cloned())
            .collect();
        for root in &crate_roots {
            if !originals.contains_key(root) {
                let path = build_dir.path().join(root);
                let code = read_to_string(&path).with_context(|| format!("read {path}"))?;
                originals.insert(root.clone(), code);
            }
        }
        let result =
            self.build_and_test_schemata(build_dir, mutants, &originals, &crate_roots, timeouts);
        for (path, code) in &originals {
            build_dir.overwrite_file(path, code)?;
        }
        result
    }

    fn build_and_test_schemata(
        &self,
        build_dir: &BuildDir,
        mutants: Vec<Mutant>,
        originals: &BTreeMap<Utf8PathBuf, String>,
        crate_roots: &BTreeSet<Utf8PathBuf>,
        timeouts: Timeouts,
    ) -> Result<Vec<Mutant>> {
        let packages = self.schemata_packages(&mutants);
        let ids = self.build_schemata(
            build_dir,
            &mutants,
            originals,
            crate_roots,
            &packages,
            timeouts,
        )?;
        if ids.is_empty() {
            return Ok(mutants);
        }
        let coverage = match self.run_schemata_baseline(build_dir, &packages, timeouts)? {
            SchemataBaseline::Failed => return Ok(mutants),
            SchemataBaseline::Passed { coverage } => coverage,
        };
        let mut jobs = Vec::new();
        let mut separate = Vec::new();
        for (i, mutant) in mutants.into_iter().enumerate() {
            let id = i + 1;
            if ids.contains(&id) {
                jobs.push(Job {
                    mutant,
                    schemata_id: Some(id),
                    tests: coverage.tests_for(id),
                });
            } else {
                separate.push(mutant);
            }
        }
        self.console.message(&format!(
            "Testing {} in one schemata build; {} will be built separately\n",
            plural(jobs.len(), "mutant"),
            separate.len(),
        ));
        // All the threads share the one build dir, since nothing needs to be rebuilt.
        let n_threads = max(1, min(self.options.jobs.unwrap_or(1), jobs.len()));
        let work_queue = &Mutex::new(jobs.into_iter());
        thread::scope(|scope| -> Result<()> {
            let threads = (0..n_threads)
                .map(|i_thread| {
                    scope.spawn(move || -> Result<()> {
                        // Each thread needs its own key in the console's progress view.
                        let console_key = build_dir.path().join(format!("schemata-{i_thread}"));
                        self.make_worker(build_dir, console_key)
                            .run_queue(work_queue, timeouts)
                    })
                })
                .collect_vec();
            join_threads(threads)
        })?;
        separate.extend(
            self.schemata_recheck
                .lock()
                .expect("lock recheck")
                .drain(..),
        );
        Ok(separate)
    }

    /// Write the schemata into the build dir and build it, leaving out mutants that cause
    /// compiler errors until it builds.
    ///
    /// Returns the ids (indexes in `mutants`, plus one) of the mutants built into the
    /// schemata, or an empty set if the schemata couldn't be built.
    fn build_schemata(
        &self,
        build_dir: &BuildDir,
        mutants: &[Mutant],
        originals: &BTreeMap<Utf8PathBuf, String>,
        crate_roots: &BTreeSet<Utf8PathBuf>,
        packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<BTreeSet<usize>> {
        let mut enabled: BTreeSet<usize> = mutants
            .iter()
            .enumerate()
            .filter(|(_, mutant)| mutant.switch.is_some())
            .map(|(i, _)| i + 1)
            .collect();
        let in_file =
            |id: usize, path: &Utf8Path| mutants[id - 1].source_file.tree_relative_path == path;
        for round in 1..=MAX_SCHEMATA_ROUNDS {
            let mut generated = BTreeMap::new();
            for (path, original) in originals {
                let file_mutants = enabled
                    .iter()
                    .filter(|&&id| in_file(id, path))
                    .map(|&id| (id, &mutants[id - 1]))
                    .collect_vec();
                let mut schemata = schemata::generate(original, &file_mutants);
                for id in &schemata.unswitched {
                    enabled.remove(id);
                }
                if crate_roots.contains(path) {
                    schemata.code.push_str(schemata::CRATE_ROOT_SUFFIX);
                }
                build_dir.overwrite_file(path, &schemata.code)?;
                generated.insert(path.as_path(), schemata);
            }
            if enabled.is_empty() {
                return Ok(enabled);
            }
            let log_name = format!("schemata_build_{round}");
            let task = self.run_task(
                build_dir,
                &log_name,
                Phase::Build,
                packages,
                timeouts.build,
                &json_message_format_args(self.options),
                &[],
                self.options,
            )?;
            if task.success {
                info!(
                    round,
                    "Built {} in one schemata build",
                    plural(enabled.len(), "mutant")
                );
                return Ok(enabled);
            }
            let mut blamed = BTreeSet::new();
            let mut attributed_files = BTreeSet::new();
            let mut unattributed_files = BTreeSet::new();
            for locations in schemata::error_locations(&task.log) {
                // Blame the mutants at the first location, most relevant first, that's in one.
                let blame = locations.iter().find_map(|location| {
                    let path =
                        schemata::tree_relative_error_path(&location.file_name, build_dir.path());
                    let ids = generated.get(path)?.blame(location.byte_offset)?;
                    Some((path.to_owned(), ids))
                });
                if let Some((path, ids)) = blame {
                    blamed.extend(ids.iter().copied());
                    attributed_files.insert(path);
                } else if let Some(primary) = locations.first() {
                    let path =
                        schemata::tree_relative_error_path(&primary.file_name, build_dir.path());
                    if generated.contains_key(path) {
                        unattributed_files.insert(path.to_owned());
                    }
                }
            }
            // An error outside any mutant can't be traced to one. If other errors in the same
            // file were, it's most likely a consequence of them, such as a name that a broken
            // mutant failed to define, so it should go away in the next round. Otherwise, all
            // the file's mutants are left out.
            for path in unattributed_files.difference(&attributed_files) {
                blamed.extend(enabled.iter().filter(|&&id| in_file(id, path)));
            }
            blamed.retain(|id| enabled.contains(id));
            if blamed.is_empty() {
                warn!(
                    "Schemata build failed with errors not attributable to any mutant, so all mutants will be built separately: see mutants.out/log/{log_name}.log"
                );
                return Ok(BTreeSet::new());
            }
            debug!(
                round,
                n_blamed = blamed.len(),
                "leaving out mutants that don't compile in the schemata"
            );
            enabled.retain(|id| !blamed.contains(id));
        }
        warn!(
            "Schemata build still failed after {MAX_SCHEMATA_ROUNDS} rounds, so all mutants will be built separately"
        );
        Ok(BTreeSet::new())
    }

    /// Whether the user's test arguments enable libtest's unstable options, such as
    /// `--report-time`.
    fn unstable_test_options(&self) -> bool {
        let args = &self.options.additional_cargo_test_args;
        args.iter().any(|a| a == "-Zunstable-options")
            || args
                .windows(2)
                .any(|pair| pair[0] == "-Z" && pair[1] == "unstable-options")
    }

    /// Run the tests in the schemata build with no mutant active, to check that the
    /// schemata don't change behavior, and record which tests reach which mutants and how
    /// long each test takes.
    fn run_schemata_baseline(
        &self,
        build_dir: &BuildDir,
        packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<SchemataBaseline> {
        let output_path = self
            .output_mutex
            .lock()
            .expect("lock output dir")
            .path()
            .join("schemata_coverage");
        create_dir_all(&output_path).with_context(|| format!("create {output_path}"))?;
        // Tests run in the build dir, so the path must be absolute.
        let trace_dir = output_path.canonicalize_utf8()?;
        let test_tool = self.options.test_tool();
        let (known_tests, overhead) = self.list_tests(build_dir, packages, timeouts)?;
        let report_time = matches!(test_tool, TestTool::Cargo) && self.unstable_test_options();
        // Tests that fail with no mutant active, for example because they count allocations
        // and the schemata's tracing allocates, are left out of every schemata run, and any
        // mutant missed there is checked again in a build of its own.
        let mut skipped: Vec<TestId> = Vec::new();
        let mut round = 1;
        let baseline_options = without_fail_fast(self.options);
        let task = loop {
            if trace_dir.exists() {
                remove_dir_all(&trace_dir).with_context(|| format!("remove {trace_dir}"))?;
            }
            create_dir_all(&trace_dir).with_context(|| format!("create {trace_dir}"))?;
            let Some(mut args) = test_args(test_tool, None, &skipped) else {
                warn!(
                    "Can't name the tests that fail in the schemata build to skip them, so all mutants will be built separately"
                );
                return Ok(SchemataBaseline::Failed);
            };
            if report_time {
                if args.is_empty() {
                    args.push("--".to_owned());
                }
                args.push("--report-time".to_owned());
            }
            let log_name = if round == 1 {
                "schemata_baseline".to_owned()
            } else {
                format!("schemata_baseline_{round}")
            };
            let task = self.run_task(
                build_dir,
                &log_name,
                Phase::Test,
                packages,
                timeouts.test,
                &args,
                &[(TRACE_ENV.to_owned(), trace_dir.to_string())],
                &baseline_options,
            )?;
            if task.success {
                break task;
            }
            let new_failures = failed_tests(&task.log)
                .into_iter()
                .filter(|test| !skipped.contains(test))
                .collect_vec();
            if new_failures.is_empty() || round == MAX_SCHEMATA_BASELINE_ROUNDS {
                warn!(
                    "Tests failed in the schemata build with no mutant active, so all mutants will be built separately: see mutants.out/log/{log_name}.log"
                );
                return Ok(SchemataBaseline::Failed);
            }
            info!(
                "{} failed in the schemata build with no mutant active, so they're skipped there, and mutants missed there are checked again in a build of their own: {}",
                plural(new_failures.len(), "test"),
                new_failures
                    .iter()
                    .map(|test| test.name.as_str())
                    .join(", "),
            );
            skipped.extend(new_failures);
            round += 1;
        };
        let _ = self.schemata_skipped_tests.set(skipped);
        let durations = test_durations(&task.log);
        debug!(
            n_durations = durations.len(),
            ?overhead,
            "read test durations from schemata baseline"
        );
        let _ = self.test_durations.set(TestDurations {
            durations,
            overhead,
        });
        let coverage = Coverage::read(&trace_dir, known_tests.as_ref())?;
        Ok(SchemataBaseline::Passed { coverage })
    }

    /// Under cargo test, list the tests, and measure the cost of a run of tests apart from
    /// the tests themselves.
    fn list_tests(
        &self,
        build_dir: &BuildDir,
        packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<(Option<HashSet<String>>, Duration)> {
        // Under cargo test, a mutant is attributed to the name of the thread that reached it,
        // which is a test only if libtest lists it. Listing also measures the cost of a run
        // apart from the tests themselves: cargo, and starting each test binary.
        if matches!(self.options.test_tool(), TestTool::Cargo) {
            let mut args = self.selection_target_args();
            args.extend(["--".to_owned(), "--list".to_owned()]);
            let task = self.run_task(
                build_dir,
                "schemata_list_tests",
                Phase::Test,
                packages,
                timeouts.test,
                &args,
                &[],
                self.options,
            )?;
            if task.success {
                Ok((Some(listed_tests(&task.log)), task.duration))
            } else {
                Ok((None, Duration::ZERO))
            }
        } else {
            Ok((None, Duration::ZERO))
        }
    }

    /// Arguments that run only these tests, and a timeout for them.
    ///
    /// The timeout is scaled from the tests' own durations in the baseline, if they're all
    /// known, plus the cost of any run; otherwise it's the timeout for the whole suite.
    fn stage_args_and_timeout(
        &self,
        tests: &[TestId],
        timeouts: Timeouts,
    ) -> Option<(Vec<String>, Option<Duration>)> {
        let skipped = self.schemata_skipped();
        let tests = tests
            .iter()
            .filter(|test| !skipped.contains(test))
            .cloned()
            .collect_vec();
        let mut args = self.selection_target_args();
        args.extend(test_args(self.options.test_tool(), Some(&tests), skipped)?);
        let timeout = self
            .test_durations
            .get()
            .and_then(|known| {
                let selected = total_duration(&known.durations, &tests)?;
                Some(selected_tests_timeout(
                    timeouts.test,
                    selected,
                    known.overhead,
                    self.options,
                ))
            })
            .or(timeouts.test);
        Some((args, timeout))
    }

    /// Tests skipped in the schemata build because they fail with no mutant active.
    fn schemata_skipped(&self) -> &[TestId] {
        self.schemata_skipped_tests.get().map_or(&[], Vec::as_slice)
    }

    /// Under cargo test, arguments that leave out doctests when running selected tests.
    ///
    /// Coverage never selects a doctest, since they don't run on a thread named after the
    /// test, and building them can take longer than a timeout scaled to the selected tests.
    /// If the user chose targets themselves, they're left as they are.
    fn selection_target_args(&self) -> Vec<String> {
        const TARGET_ARGS: &[&str] = &[
            "--doc",
            "--lib",
            "--bins",
            "--bin",
            "--tests",
            "--test",
            "--examples",
            "--example",
            "--benches",
            "--bench",
            "--all-targets",
        ];
        let options = self.options;
        let chose_targets = options
            .additional_cargo_args
            .iter()
            .chain(&options.additional_cargo_test_args)
            .take_while(|a| *a != "--")
            .any(|a| {
                TARGET_ARGS
                    .iter()
                    .any(|t| a == t || a.starts_with(&format!("{t}=")))
            });
        if matches!(options.test_tool(), TestTool::Cargo) && !chose_targets {
            vec!["--tests".to_owned()]
        } else {
            Vec::new()
        }
    }

    /// Run a cargo command that's part of setting up the schemata, rather than a scenario.
    #[allow(clippy::too_many_arguments)]
    fn run_task(
        &self,
        build_dir: &BuildDir,
        name: &str,
        phase: Phase,
        packages: &PackageSelection,
        timeout: Option<Duration>,
        extra_args: &[String],
        extra_env: &[(String, String)],
        options: &Options,
    ) -> Result<TaskResult> {
        let mut output = self
            .output_mutex
            .lock()
            .expect("lock output dir to start task")
            .start_task(name)?;
        let console_key = build_dir.path().join(name);
        self.console
            .scenario_started(&console_key, &Scenario::Baseline, output.open_log_read()?);
        self.console.scenario_phase_started(&console_key, phase);
        let result = run_cargo(
            build_dir,
            self.jobserver.as_ref(),
            packages,
            phase,
            timeout,
            extra_args,
            extra_env,
            &mut output,
            options,
            self.console,
        );
        self.console.task_finished(&console_key);
        let phase_result = result?;
        let log_path = output.output_dir.join(output.log_path());
        let log = read_to_string(&log_path).with_context(|| format!("read {log_path}"))?;
        Ok(TaskResult {
            success: phase_result.is_success(),
            log,
            duration: phase_result.duration,
        })
    }
}

/// A worker owns one build directory and runs a single thread of testing.
///
/// It consumes jobs from an input queue and runs them until the queue is empty,
/// appending output to the output directory.
struct Worker<'a> {
    lab: &'a Lab<'a>,
    build_dir: &'a BuildDir,
    /// Identifies this worker in the console's progress view.
    console_key: Utf8PathBuf,
}

impl Worker<'_> {
    /// Run until the input queue is empty.
    fn run_queue(
        mut self,
        work_queue: &Mutex<vec::IntoIter<Job>>,
        timeouts: Timeouts,
    ) -> Result<()> {
        let _span = debug_span!("worker thread", build_dir = ?self.build_dir.path()).entered();
        loop {
            // Not a `for` statement so that we don't hold the lock
            // for the whole iteration.
            let Some(job) = work_queue.lock().expect("Lock pending work queue").next() else {
                return Ok(());
            };
            let _span = debug_span!("mutant", name = job.mutant.name(false)).entered();
            let test_packages = match &self.lab.tests_for_mutant {
                TestsForMutant::Workspace => PackageSelection::All,
                TestsForMutant::Mutated => {
                    PackageSelection::Explicit(vec![job.mutant.source_file.package.clone()])
                }
                TestsForMutant::Explicit(packages) => PackageSelection::Explicit(packages.clone()),
            };
            if let Some(id) = job.schemata_id {
                self.test_switched_mutant(job.mutant, id, job.tests, &test_packages, timeouts)?;
            } else {
                self.run_one_scenario(&Scenario::Mutant(job.mutant), &test_packages, timeouts)?;
            }
        }
    }

    fn run_one_scenario(
        &mut self,
        scenario: &Scenario,
        test_packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<ScenarioOutcome> {
        let lab = self.lab;
        let mut scenario_output = lab
            .output_mutex
            .lock()
            .expect("lock output_dir to start scenario")
            .start_scenario(scenario)?;
        let dir = &self.console_key;
        lab.console
            .scenario_started(dir, scenario, scenario_output.open_log_read()?);
        debug!(?test_packages);

        if let Some(mutant) = scenario.mutant() {
            let mutated_code = mutant.mutated_code();
            let diff = scenario.mutant().unwrap().diff(&mutated_code);
            scenario_output.write_diff(&diff)?;
            mutant.apply(self.build_dir, &mutated_code)?;
        }

        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        for &phase in lab.options.phases() {
            lab.console.scenario_phase_started(dir, phase);
            let timeout = match phase {
                Phase::Test => timeouts.test,
                Phase::Build | Phase::Check => timeouts.build,
            };
            match run_cargo(
                self.build_dir,
                lab.jobserver.as_ref(),
                test_packages,
                phase,
                timeout,
                &[],
                &[],
                &mut scenario_output,
                lab.options,
                lab.console,
            ) {
                Ok(phase_result) => {
                    let success = phase_result.is_success(); // so we can move it away
                    outcome.add_phase_result(phase_result);
                    lab.console.scenario_phase_finished(dir, phase);
                    if !success {
                        break;
                    }
                }
                Err(err) => {
                    error!(?err, ?phase, "scenario execution internal error");
                    // Some unexpected internal error that stops the program.
                    if let Some(mutant) = scenario.mutant() {
                        mutant.revert(self.build_dir)?;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(mutant) = scenario.mutant() {
            mutant.revert(self.build_dir)?;
        }
        lab.finish_scenario(dir, scenario, &outcome)?;
        Ok(outcome)
    }

    /// Test a mutant that's built into the schemata, by running the tests with it switched on.
    ///
    /// If the tests reaching the mutant are known, only they are run: first those most likely
    /// to catch it, and then the rest. If no test reaches it, it's missed without running
    /// anything.
    fn test_switched_mutant(
        &mut self,
        mutant: Mutant,
        id: usize,
        tests: Option<Vec<TestId>>,
        test_packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<ScenarioOutcome> {
        let lab = self.lab;
        let scenario = Scenario::Mutant(mutant);
        let mut scenario_output = lab
            .output_mutex
            .lock()
            .expect("lock output_dir to start scenario")
            .start_scenario(&scenario)?;
        let dir = &self.console_key;
        lab.console
            .scenario_started(dir, &scenario, scenario_output.open_log_read()?);
        let mutant = scenario.mutant().expect("mutant scenario");
        let function_name = mutant.function.as_ref().map(|f| f.function_name.as_str());
        scenario_output.write_diff(&mutant.diff(&mutant.mutated_code()))?;
        scenario_output.message(&format!(
            "Mutant is switched on in the schemata build by {ACTIVE_ENV}={id}"
        ))?;
        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        // Each stage is the arguments that select its tests, and its timeout.
        // Every test but those skipped in the schemata build, which could always be named
        // since the baseline ran with them skipped.
        let skipped = lab.schemata_skipped();
        let all_tests = (
            test_args(lab.options.test_tool(), None, skipped).unwrap_or_default(),
            timeouts.test,
        );
        let stages: Vec<(Vec<String>, Option<Duration>)> = match tests {
            None => vec![all_tests],
            Some(tests) if tests.is_empty() => {
                scenario_output.message(
                    "No test reached this mutant in the coverage run, so it's missed without running tests",
                )?;
                outcome.add_phase_result(PhaseResult {
                    phase: Phase::Test,
                    duration: Duration::ZERO,
                    process_status: Exit::Success,
                    argv: Vec::new(),
                });
                Vec::new()
            }
            Some(tests) => lab
                .kill_history
                .lock()
                .expect("lock kill history")
                .stages(function_name, tests)
                .iter()
                .map(|stage| lab.stage_args_and_timeout(stage, timeouts))
                .collect::<Option<Vec<_>>>()
                // If any tests can't be named, run them all at once.
                .unwrap_or_else(|| vec![all_tests]),
        };
        let env = [(ACTIVE_ENV.to_owned(), id.to_string())];
        for (args, timeout) in stages {
            lab.console.scenario_phase_started(dir, Phase::Test);
            let phase_result = run_cargo(
                self.build_dir,
                lab.jobserver.as_ref(),
                test_packages,
                Phase::Test,
                timeout,
                &args,
                &env,
                &mut scenario_output,
                lab.options,
                lab.console,
            )?;
            lab.console.scenario_phase_finished(dir, Phase::Test);
            let success = phase_result.is_success();
            outcome.add_phase_result(phase_result);
            if !success {
                break;
            }
        }
        if outcome.mutant_caught() {
            let killers = failed_tests(&outcome.get_log_content()?);
            lab.kill_history
                .lock()
                .expect("lock kill history")
                .record(function_name, &killers);
        }
        if outcome.mutant_missed() && !skipped.is_empty() {
            // One of the skipped tests might have caught it.
            scenario_output.message(
                "Missed with some tests skipped in the schemata build: checking again in a build of its own",
            )?;
            lab.console.task_finished(dir);
            lab.schemata_recheck
                .lock()
                .expect("lock recheck")
                .push(mutant.clone());
            return Ok(outcome);
        }
        lab.finish_scenario(dir, &scenario, &outcome)?;
        Ok(outcome)
    }
}

/// Which packages to test
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestsForMutant {
    /// Test all packages in the workspace
    Workspace,
    /// Test only the package that was mutated
    Mutated,
    /// Test specific packages
    Explicit(Vec<Arc<Package>>),
}

impl TestsForMutant {
    fn new(options: &Options, workspace: &Workspace) -> Self {
        match options.test_package {
            TestPackages::Workspace => TestsForMutant::Workspace,
            TestPackages::Mutated => TestsForMutant::Mutated,
            TestPackages::Named(ref package_names) => {
                TestsForMutant::Explicit(workspace.packages_by_name(package_names))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn without_fail_fast_removes_libtest_fail_fast_and_adds_cargo_no_fail_fast() {
        let options = Options::from_arg_strs([
            "mutants",
            "--",
            "--fail-fast",
            "--",
            "-Zunstable-options",
            "--fail-fast",
        ]);
        let options = without_fail_fast(&options);
        assert_eq!(options.additional_cargo_args, ["--no-fail-fast"]);
        assert_eq!(
            options.additional_cargo_test_args,
            ["--fail-fast", "--", "-Zunstable-options"]
        );
    }
}
