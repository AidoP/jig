# jig doc

`jig doc [--open] [<name> ...]` documents the crates with `doc`, or only those named, with rustdoc, as `jig build doc`
and `jig build doc/<name>` do, and then shows where the first page of each crate's documentation is, relative to the
working directory if it is below it, or else in full, as `<name>: <path>`. With `--open`, it opens the first of them in
the browser, with `xdg-open`, `open` on macOS, or `start` on Windows. It takes the options of [jig build](build.md)
which are about building.

A crate is named as `jig build doc/<name>` names it: by its name, with `@<target>` if its target is not the host's. A
name which no crate with `doc` has is an error, as is `jig doc` in a project which documents no crate. See `doc` in
[crates](../language/crates.md) for where the documentation is written.
