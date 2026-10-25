// Copyright 2021-2024 Martin Pool

//! Calculation of timeouts for the build and test phases.

use std::{
    cmp::{max, min},
    time::Duration,
};

use tracing::{info, warn};

use crate::{
    options::Options,
    outcome::{Phase, ScenarioOutcome},
};

#[derive(Debug, Copy, Clone)]
pub struct Timeouts {
    pub build: Option<Duration>,
    pub test: Option<Duration>,
}

impl Timeouts {
    pub fn for_baseline(options: &Options) -> Timeouts {
        Timeouts {
            test: options.test_timeout,
            build: None,
        }
    }

    pub fn from_baseline(baseline: &ScenarioOutcome, options: &Options) -> Timeouts {
        Timeouts {
            build: build_timeout(
                baseline.phase_result(Phase::Build).map(|pr| pr.duration),
                options,
            ),
            test: Some(test_timeout(
                baseline.phase_result(Phase::Test).map(|pr| pr.duration),
                options,
            )),
        }
    }

    pub fn without_baseline(options: &Options) -> Timeouts {
        Timeouts {
            build: build_timeout(None, options),
            test: Some(test_timeout(None, options)),
        }
    }
}

/// The timeout for running only some of the tests, whose baseline durations add up to
/// `selected`, in a run that costs `overhead` apart from the tests: scaled like the whole
/// suite's, but never longer than `full`.
///
/// An explicit `--timeout` is the whole suite's timeout, so it's only the upper limit here:
/// the selected tests' own durations were measured in this run, which makes them as good a
/// basis as the whole suite's.
pub fn selected_tests_timeout(
    full: Option<Duration>,
    selected: Duration,
    overhead: Duration,
    options: &Options,
) -> Duration {
    let timeout = scaled_test_timeout(selected, options) + overhead;
    full.map_or(timeout, |full| min(full, timeout))
}

/// Scale a baseline test duration by the multiplier, but not below the minimum.
fn scaled_test_timeout(baseline_duration: Duration, options: &Options) -> Duration {
    max(
        options.minimum_test_timeout,
        Duration::from_secs_f64(
            (baseline_duration.as_secs_f64() * options.test_timeout_multiplier.unwrap_or(5.0))
                .ceil(),
        ),
    )
}

const FALLBACK_TIMEOUT_SECS: u64 = 300;
fn warn_fallback_timeout(phase_name: &str, option: &str) {
    warn!(
        "An explicit {phase_name} timeout is recommended when using {option}; using {FALLBACK_TIMEOUT_SECS} seconds by default"
    );
}

fn test_timeout(baseline_duration: Option<Duration>, options: &Options) -> Duration {
    if let Some(explicit) = options.test_timeout {
        explicit
    } else if let Some(baseline_duration) = baseline_duration {
        let timeout = scaled_test_timeout(baseline_duration, options);
        if options.show_times {
            info!("Auto-set test timeout to {}s", timeout.as_secs());
        }
        timeout
    } else if options.check_only {
        // We won't have run baseline tests, and we won't run any other tests either.
        Duration::from_secs(0)
    } else {
        warn_fallback_timeout("test", "--baseline=skip");
        Duration::from_secs(FALLBACK_TIMEOUT_SECS)
    }
}

fn build_timeout(baseline_duration: Option<Duration>, options: &Options) -> Option<Duration> {
    if let Some(t) = options.build_timeout {
        Some(t)
    } else if let Some(baseline) = baseline_duration {
        if let Some(multiplier) = options.build_timeout_multiplier {
            let timeout = Duration::from_secs_f64(baseline.as_secs_f64() * multiplier);
            if options.show_times {
                info!("Auto-set build timeout to {}s", timeout.as_secs());
            }
            Some(timeout)
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use clap::Parser;
    use indoc::indoc;

    use super::*;
    use crate::{Args, config::Config};

    #[test]
    fn selected_tests_timeout_scales_selected_duration_within_full_timeout() {
        let options = Options::from_arg_strs([
            "mutants",
            "--timeout-multiplier",
            "3",
            "--minimum-test-timeout",
            "5",
        ]);
        let full = Some(Duration::from_secs(21));
        assert_eq!(
            selected_tests_timeout(full, Duration::from_millis(300), Duration::ZERO, &options),
            Duration::from_secs(5),
            "minimum"
        );
        assert_eq!(
            selected_tests_timeout(
                full,
                Duration::from_secs(4),
                Duration::from_secs(1),
                &options
            ),
            Duration::from_secs(13)
        );
        assert_eq!(
            selected_tests_timeout(full, Duration::from_secs(60), Duration::ZERO, &options),
            Duration::from_secs(21),
            "never more than the full suite's timeout"
        );
    }

    #[test]
    fn timeout_multiplier_from_option() {
        let options = Options::from_arg_strs(["mutants", "--timeout-multiplier", "1.5"]);

        assert_eq!(options.test_timeout_multiplier, Some(1.5));
        assert_eq!(
            test_timeout(Some(Duration::from_secs(40)), &options),
            Duration::from_secs(60),
        );
    }

    #[test]
    fn test_timeout_unaffected_by_in_place_build() {
        let options =
            Options::from_arg_strs(["mutants", "--timeout-multiplier", "1.5", "--in-place"]);

        assert_eq!(
            test_timeout(Some(Duration::from_secs(40)), &options),
            Duration::from_secs(60),
        );
    }

    #[test]
    fn build_timeout_multiplier_from_option() {
        let args = Args::try_parse_from(["mutants", "--build-timeout-multiplier", "1.5"]).unwrap();
        let config = Config::default();
        let options = Options::new(&args, &config).unwrap();

        assert_eq!(options.build_timeout_multiplier, Some(1.5));
        assert_eq!(
            build_timeout(Some(Duration::from_secs(40)), &options),
            Some(Duration::from_secs(60)),
        );
    }

    #[test]
    fn build_timeout_is_affected_by_in_place_build() {
        let args =
            Args::try_parse_from(["mutants", "--build-timeout-multiplier", "5", "--in-place"])
                .unwrap();
        let config = Config::default();
        let options = Options::new(&args, &config).unwrap();

        assert_eq!(
            build_timeout(Some(Duration::from_secs(40)), &options),
            Some(Duration::from_secs(40 * 5))
        );
    }

    #[test]
    fn timeout_multiplier_from_config() {
        let args = Args::try_parse_from(["mutants"]).unwrap();
        let config = Config::from_str(indoc! {r#"
            timeout_multiplier = 2.0
        "#})
        .unwrap();
        let options = Options::new(&args, &config).unwrap();

        assert_eq!(options.test_timeout_multiplier, Some(2.0));
        assert_eq!(
            test_timeout(Some(Duration::from_secs(42)), &options),
            Duration::from_secs(42 * 2),
        );
    }

    #[test]
    fn build_timeout_multiplier_from_config() {
        let args = Args::try_parse_from(["mutants"]).unwrap();
        let config = Config::from_str(indoc! {r#"
            build_timeout_multiplier = 2.0
        "#})
        .unwrap();
        let options = Options::new(&args, &config).unwrap();

        assert_eq!(options.build_timeout_multiplier, Some(2.0));
        assert_eq!(
            build_timeout(Some(Duration::from_secs(42)), &options),
            Some(Duration::from_secs(42 * 2)),
        );
    }

    #[test]
    fn timeout_multiplier_default() {
        let args = Args::try_parse_from(["mutants"]).unwrap();
        let options = Options::new(&args, &Config::default()).unwrap();

        assert_eq!(options.test_timeout_multiplier, None);
        assert_eq!(
            test_timeout(Some(Duration::from_secs(42)), &options),
            Duration::from_secs(42 * 5),
        );
    }

    #[test]
    fn build_timeout_multiplier_default() {
        let args = Args::try_parse_from(["mutants"]).unwrap();
        let options = Options::new(&args, &Config::default()).unwrap();

        assert_eq!(options.build_timeout_multiplier, None);
        assert_eq!(build_timeout(Some(Duration::from_secs(42)), &options), None,);
    }

    #[test]
    fn timeout_from_option() {
        let args = Args::try_parse_from(["mutants", "--timeout=8"]).unwrap();
        let options = Options::new(&args, &Config::default()).unwrap();

        assert_eq!(options.test_timeout, Some(Duration::from_secs(8)));
    }

    #[test]
    fn build_timeout_from_option() {
        let args = Args::try_parse_from(["mutants", "--build-timeout=4"]).unwrap();
        let options = Options::new(&args, &Config::default()).unwrap();

        assert_eq!(options.build_timeout, Some(Duration::from_secs(4)));
    }

    #[test]
    fn no_default_build_timeout() {
        let args = Args::try_parse_from(["mutants"]).unwrap();
        let options = Options::new(&args, &Config::default()).unwrap();

        assert_eq!(options.build_timeout, None);
    }

    #[test]
    fn timeout_multiplier_default_with_baseline_skip() {
        // The --baseline option is not used to set the timeout but it's
        // indicative of the realistic situation.
        let args = Args::try_parse_from(["mutants", "--baseline", "skip"]).unwrap();
        let options = Options::new(&args, &Config::default()).unwrap();

        assert_eq!(options.test_timeout_multiplier, None);
        assert_eq!(test_timeout(None, &options), Duration::from_secs(300));
        assert_eq!(build_timeout(None, &options), None);
    }
}
