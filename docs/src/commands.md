# Commands

jig is run as `jig [+<toolchain>] [--color <when>] [--timings] <command> [<argument> ...]`, where `+<toolchain>` chooses
a rustup toolchain (see [toolchains](toolchains.md)), `--color` is `auto`, `always` or `never`, and `--timings` shows
how long each step took. `jig help` shows each command.

| Command | What it does |
|---|---|
| [`jig generate [<input>]`](commands/build.md) | configures the project from `Jig`, or the file given |
| [`jig build`](commands/build.md) | builds the project, or the targets given |
| [`jig run`](commands/run.md) | builds a binary crate and runs it |
| [`jig test`](commands/test.md) | builds and runs the tests |
| [`jig lint`](commands/lint.md) | lints the crates with clippy |
| [`jig check <label>`](commands/check.md) | checks a crate for rust-analyzer |
| [`jig install`](commands/install.md) | installs what the project installs |
| [`jig graph`](commands/graph.md) | shows rig's graph of the project |
| [`jig cache`](caching.md) | shows, collects or clears the cache |
| [`jig serve`](caching.md) | serves the cache to other machines |
| [`jig worker`, `jig workers`](workers.md) | runs a worker, or shows the workers |
