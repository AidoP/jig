# Commands

jig is run as `jig [+<toolchain>] [--color <when>] [--timings] <command> [<argument> ...]`, where `+<toolchain>` chooses
a rustup toolchain (see [toolchains](toolchains.md)), `--color` is `auto`, `always` or `never`, and `--timings` shows
how long each step took. `jig help`, `jig --help` or `jig -h` shows each command, and `jig version` or
`jig --version` shows the version of jig, which is `unreleased` but for a release, which sets it from its tag by the
`jig-version` property of jig's own `Jig`.

Every command takes its options anywhere among its arguments before `--`, so that `jig lint app -D` is
`jig lint -D app`, and fails for an option it does not take, rather than taking it as a name or a path. An option
with a value takes it in the same argument or the next, as `-j4`, `-j 4`, `--jobs=4` or `--jobs 4`. What follows
`--` is never an option: it is the targets of `jig build`, and the arguments of what `jig test` and `jig run` run.
`--help` or `-h` after a command shows the usage, rather than running it. `jig test`, `jig run`, `jig lint` and
`jig install` build first, and take the options of `jig build` which are about building: `-j <jobs>`,
`-k <failures>` and `-q`, as [jig build](commands/build.md) describes them.

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
