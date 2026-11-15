# jig lint

`jig lint` lints the crates of the project with clippy, which are those not from the toolchain, or only those named,
as `<name>`, or `<name>@<target>` if the target is not the host target. A crate is linted as it is checked, with the
levels of the lints set by the `clippy/<lint>` properties where it is declared. Every crate which can be is linted,
even if another fails, and the diagnostics of each are shown, even if nothing needed linting, as they are saved. It
fails if a lint is an error, such as one which is denied, and with `-D` or `--deny-warnings`, if there is a warning,
which is found in what was saved, so the lints are not run again, as `jig lint -D` is in CI.
