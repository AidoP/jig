# jig bench

`jig bench [<name> ...] [-- <argument> ...]` builds the harnesses of the benchmarks of the crates with `bench`, or
only those named, and runs each in turn, from the project root, with `--bench` and the arguments after `--`, such as a
filter, as in `jig bench app -- parse`. What a harness prints is shown as it prints it, as a benchmark is measured
rather than passed or failed, and it fails if a harness does. It takes the options of [jig build](build.md) which are
about building.

A crate's benchmarks are its `#[bench]` functions, which libtest only has on nightly, behind `#![feature(test)]`, so
a crate which has them declares the feature under `cfg(test)`, as its harness is built with `--test`:

```rust
#![cfg_attr(test, feature(test))]
#[cfg(test)]
extern crate test;

#[cfg(test)]
mod benches {
    #[bench]
    fn sums(b: &mut test::Bencher) {
        b.iter(|| (0..test::black_box(1000u64)).sum::<u64>());
    }
}
```

The harness is the crate's test harness built with the codegen options `profile/release` sets for speed, `opt-level`
3, a single codegen unit, and no debug information, debug assertions or overflow checks, in place of its own, but
neither LTO, nor `strip`, nor `panic=abort`, as a harness unwinds. The crates it uses are built as they are for its
tests. On stable, `--bench` runs no tests, so a harness built there runs nothing unless its arguments say otherwise.
