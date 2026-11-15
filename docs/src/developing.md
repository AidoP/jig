# Developing jig

`jig` is built for each of the `targets` property's targets whose standard library is installed, such as the musl
targets added with `rustup target add`. The `release-target` property, which is the host target by default, is the one
built by default, installed, and run by the integration tests. A release sets it to a musl target in `Jig.local`:

```
set release-target 'x86_64-unknown-linux-musl'
```

## Tests

The tests need a nightly toolchain as well as Rust 1.97 or later, for those of custom targets.

The `perf` test times `jig` on large generated projects, failing if one takes far longer than expected, or grows much
faster than its size where it should be linear. It is slow, so it is tagged `perf`, and
`jig test` leaves it out unless it is named, or run with `jig test --tag perf` or `jig test --all`, as CI does. Its
time budgets are multiplied by the `JIG_PERF_SCALE` environment variable, such as `JIG_PERF_SCALE=4 jig test perf` on
a slow machine.

The unit tests, run by `jig test jig`, include micro-benchmarks of parts of `jig`, such as reading a configuration and
finding the crates passed to each crate, which fail if one grows much faster than its input. Their times are shown by
running the test harness with `--nocapture`, as in `build/<target>/test/jig benches --nocapture`.

## Windows

`jig` and rig are being ported to Windows, which is developed on Linux, building for `x86_64-pc-windows-gnu` with
MinGW, and running the tests with Wine, as CI does, with this in `Jig.local`, after `rustup target add
x86_64-pc-windows-gnu` and installing `wine64` and `gcc-mingw-w64-x86-64`:

```
set targets [host-target, 'x86_64-pc-windows-gnu']
target x86_64-pc-windows-gnu
    override
    linker 'x86_64-w64-mingw32-gcc'
    runner '/usr/lib/wine/wine64'
```

Then `jig test rig@x86_64-pc-windows-gnu jig@x86_64-pc-windows-gnu` runs their tests for Windows, and `jig lint` lints
them for Windows too.

## The book

This book is in `docs/`, and is built with [mdBook](https://rust-lang.github.io/mdBook/), of the version CI gives as
`MDBOOK`, by the custom command `book`:

```
./build/bootstrap/jig build custom/book
```

It is built into `build/book`, from the pages `docs/src/SUMMARY.md` lists, which `Jig` reads from it, so that a page
which is added to the summary is built from too. CI builds it for each pull request, failing on a warning, and keeps
it as the artifact `book`, and publishes it from main to [GitHub Pages](https://aidop.github.io/jig/).

jig's tests check the book: each example of a `Jig` file, a code block marked `jig`, configures, and the diagnostics and
builtin properties it lists, and its links, are those jig has. An example which is part of a configuration has what it
needs in a comment directly before it, which the book doesn't show:

````
<!-- jig-test
files: local.jig
declare uart-base
    default 0x1000_0000
-->
```jig
use 'local.jig'
    file
log note 'UART at {uart-base}'
```
````

