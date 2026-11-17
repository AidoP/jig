# jig fmt

`jig fmt [--check]` formats the project's sources: the crates which are edited, with rustfmt, which formats each
crate's root and the modules it declares, with the edition the crates are built with and the toolchain the project is
configured with, and the sources of C and C++ of its `static-library`, `shared-library` and `executable` statements,
with clang-format, if it is installed. Assembly is left as it is, as are the files the build writes and those outside
the project. Each formatter finds its own configuration, such as `rustfmt.toml` or `.clang-format`, from the files it
formats.

With `--check`, nothing is changed: it fails if a file is not formatted, showing how it would change, as for CI.
