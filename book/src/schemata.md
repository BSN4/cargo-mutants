# Building mutants together (schemata)

By default cargo-mutants applies each mutant to the source, rebuilds, and runs the tests, so a
run costs roughly one incremental build per mutant. With `--schemata`, cargo-mutants instead
compiles every mutant it can into the tree at once, builds that tree once, and then tests each
mutant by running the tests with the environment variable `CARGO_MUTANTS_SCHEMATA_ID` set to
that mutant's id. Changing an environment variable doesn't cause Cargo to rebuild anything, so
each mutant costs only a test run.

    cargo mutants --schemata

This is experimental.

## How it works

Each mutated expression is replaced by code that reads the active mutant id once, and runs
either the original or the mutated code. For example, `a == b` becomes something like:

```rust,ignore
::__cargo_mutants_std::convert::identity({
    let __cargo_mutants_active: usize = /* read CARGO_MUTANTS_SCHEMATA_ID */;
    if __cargo_mutants_active != 3 { a == b } else { a != b }
})
```

Replacing a whole function body is done the same way, with any mutants inside the body
nested within the original branch. Deleting a match arm adds a guard to the arm that is false
when that mutant is active.

While the schemata are built, each crate root gets `extern crate std as __cargo_mutants_std;`
appended, and the generated code reaches the standard library through that name, so that it
works in `#![no_std]` crates too.

Some mutants can't be switched at runtime, and are built and tested one at a time as usual,
after the schemata mutants:

- deletion of fields from struct literals;
- mutants in `const fn`s, `const` and `static` items, and other const contexts;
- replacements of a whole function returning `impl Trait`, since the replacement is a different
  type from the original;
- operators joining `let` conditions, as in `if let Some(x) = y && x > 0`.

The tree is built with `--message-format=json`. If any mutated code doesn't compile, for
example a replacement whose type doesn't implement `Default`, cargo-mutants maps each compiler
error back to the mutant whose code it's in, leaves that mutant out to be built separately, and
builds again, for up to 10 rounds.

All `--jobs` share the one build directory while testing schemata mutants, since nothing needs
to be rebuilt.

## Tests that fail in the schemata build

After the schemata build succeeds, the tests are run once with no mutant active. A test can fail
there although nothing is mutated: for example a test that counts allocations, since recording
coverage (below) allocates, or a test that checks source line numbers, which the schemata
change. cargo-mutants then skips those tests, and runs the tests again, until they pass. The
skipped tests are left out of every schemata mutant's run, and any mutant missed there is
tested again in a build of its own with all the tests, so a mutant that only a skipped test
catches is still caught.

If the tests can't be made to pass this way, all mutants are built and tested separately.

## Choosing tests from coverage

During that run, each mutant site records which tests reach it: under [nextest](nextest.md)
from `NEXTEST_TEST_NAME`, since nextest runs each test in its own process, and under
`cargo test` from the name of the thread, which libtest sets to the name of the test. Then:

- Each mutant is tested by running only the tests that reach it: with a nextest filterset, or
  with `cargo test --tests -- --exact` and the test names. (`--tests` leaves out doctests,
  unless you choose targets yourself.)
- A mutant that no test reaches is reported as missed without running any tests.
- Tests that caught other mutants in the same function, or failing that, tests that caught the
  most mutants so far, are run first, in a separate invocation. If they catch the mutant, the
  other tests aren't run.

A mutant reached from a thread that isn't a test, such as one a test spawned, or from a doctest,
runs all the tests, since we can't tell which test reached it.

The recorded coverage is in `mutants.out/schemata_coverage`.

## Timeouts

When only some tests run, their timeout is scaled from those tests' own durations in the run
with no mutant active, by `--timeout-multiplier` and not below `--minimum-test-timeout`, plus
the time a run takes apart from the tests. It's never longer than the timeout for the whole
suite, including one given with `--timeout`. So a mutant that hangs costs a few seconds rather
than a multiple of the whole suite's time.

nextest reports each test's duration. libtest only does with `-Zunstable-options --report-time`:
if your test arguments already include `-Zunstable-options`, cargo-mutants adds `--report-time`
to that run. Otherwise, the whole suite's timeout is used.

## Limitations

- The generated code may trigger lints such as `unused_parens`; if your tree denies warnings,
  use [`--cap-lints=true`](lints.md).
- `--schemata` has no effect with `--check`.
- With [`--in-place`](in-place.md), the schemata are written into your source files while their
  mutants are tested, and the files are restored afterwards, including when the run is
  interrupted with Ctrl-C. If cargo-mutants is killed outright, restore them with
  `git checkout`, just as for an ordinary in-place mutant.
