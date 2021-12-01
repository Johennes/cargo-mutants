# cargo-mutants changelog

## 0.1.0

Released 2021-11-30

  * Logs and other information are written into `mutants.out` in the source
    directory, rather than `target/mutants`.

  * New `--all-logs` option prints all Cargo output to stdout, which is verbose
    but useful for example in CI, by making all the output directly available
    in captured stdout.

  * The output distinguishes check or build failures (probably due to an
    unviable mutant) from test failures (probably due to lacking coverage.)

  * A new file `mutants.out/mutants.json` lists all the generated mutants.

  * Show function return types in some places, to make it easier to understand
    whether the mutants were useful or viable.

  * Run `cargo check --tests` and `cargo build --tests` in the source directory
    to freshen the build and download any dependencies, before copying it to a
    scratch directory.

  * New `--check` option runs `cargo check` on generated mutants to see if they
    are viable, without actually running the tests. This is useful in tuning
    cargo-mutants to generate better mutants.

  * New `--no-times` output hides times (and tree sizes) from stdout, mostly to
    make the output deterministic and easier to match in tests.

  * Mutate methods too!

## 0.0.4

Released 2021-11-10

  * Fixed `cargo install cargo-mutants` (sometimes?) failing due to the
    `derive` feature not getting set on the `serde` dependency.

  * Show progress while copying the tree.

  * Respect the `$CARGO` environment variable so that the same toolchain is
    used to run tests as was used to invoke `cargo mutants`. Concretely, `cargo
    +nightly mutants` should work correctly.

## 0.0.3

Released 2021-11-06

  * Skip functions or modules marked `#[test]`, `#[cfg(test)]` or
    `#[mutants::skip]`.

  * Early steps towards type-guided mutations: 

    * Generate mutations of `true` and `false` for functions that return `bool`
    * Empty and arbitrary strings for functions returning `String`.
    * Return `Ok(Default::default())` for functions that return `Result<_, _>`.

  * Rename `--list-mutants` to just `--list`.

  * New `--list --json`.

  * Colored output makes test names and mutations easier to read (for me at least.)

  * Return distinct exit codes for different situations including that uncaught
    mutations were found.

## 0.0.2

  * Functions that should not be mutated can be marked with `#[mutants::skip]`
    from the [`mutants`](https://crates.io/crates/mutants) helper crate.

## 0.0.1
 
First release.
