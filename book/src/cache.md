# Caching results

With `--cache`, cargo-mutants stores the outcome of each mutant in `mutants.out/cache.json`, and
on the next run with `--cache` it reuses the outcome of any mutant whose code hasn't changed,
without building or testing it again. Cached caught, missed, and unviable outcomes are reported
as usual, so a cached missed mutant still causes a non-zero [exit code](exit-codes.md). Timeouts
aren't cached.

    cargo mutants --cache

Each outcome is keyed by a hash of:

- the package name;
- the text of the function containing the mutant;
- the mutation, and where it is relative to the start of the function;
- all the Rust source in the package's `tests` directory.

Unlike [`--iterate`](iterate.md), which matches mutants by line and column, moving a function
within a file or adding code above it doesn't invalidate its results.

## Caution

Like `--iterate`, this is a heuristic. A change to code that the function calls, or to unit tests
inside the package's `src` directory, doesn't invalidate the cached outcomes, though it could
change whether the mutant is caught. Run without `--cache` from time to time, for example on
the main branch, to be sure.
