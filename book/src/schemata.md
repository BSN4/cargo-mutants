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
::std::convert::identity({
    let __cargo_mutants_active: usize = /* read CARGO_MUTANTS_SCHEMATA_ID */;
    if __cargo_mutants_active != 3 { a == b } else { a != b }
})
```

Replacing a whole function body is done the same way, with any mutants inside the body
nested within the original branch. Deleting a match arm adds a guard to the arm that is false
when that mutant is active.

The tree is built with `--message-format=json`. If any mutated code doesn't compile, cargo-mutants
maps each compiler error back to the mutant whose code it's in, leaves that mutant out, and
builds again. This repeats for up to 10 rounds. Mutants that are left out, and mutants that
can't be switched at runtime, are built and tested one at a time as usual, after the schemata
mutants. Mutants that commonly can't be switched include:

- deletion of fields from struct literals;
- mutants in `const fn` and other `const` contexts;
- mutants in `#![no_std]` crates;
- mutants whose replacement doesn't type-check, which would otherwise be unviable.

After the schemata build succeeds, the tests are run once with no mutant active. If they fail,
for example because a test depends on line numbers that changed, all mutants are built and
tested separately.

All `--jobs` share the one build directory while testing schemata mutants, since nothing needs
to be rebuilt.

## Choosing tests from coverage

With [nextest](nextest.md) as the test tool, each mutant site also records which tests reach it
during the run with no mutant active: nextest runs each test in its own process, so the site
can tell which test is running from `NEXTEST_TEST_NAME`. Then:

- Each mutant is tested by running only the tests that reach it, using a nextest filterset.
- A mutant that no test reaches is reported as missed without running any tests.
- Tests that caught other mutants in the same function, or failing that, tests that caught the
  most mutants so far, are run first, in a separate nextest invocation. If they catch the
  mutant, the other tests aren't run.

The recorded coverage is in `mutants.out/schemata_coverage`.

With `cargo test`, all tests in a binary share one process, so this isn't possible and each
mutant runs the whole test suite.

Doctests aren't run by nextest, so a mutant reached only by doctests is reported as missed.

## Limitations

- The source line numbers of code after a mutated expression change in the schemata build. Tests
  that check line numbers, for example in panic messages or snapshots, cause the schemata to be
  abandoned.
- The generated code may trigger lints such as `unused_parens`; if your tree denies warnings,
  use [`--cap-lints=true`](lints.md).
- `--schemata` has no effect with `--check` or `--in-place`.
