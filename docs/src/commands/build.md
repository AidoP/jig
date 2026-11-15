# jig build and jig generate

`jig generate` evaluates `Jig`, writes rig's `graph` to the build directory and records the build
directory in `.jig`, at the project root. `jig build`, `jig check`, `jig test`, `jig run`, `jig lint` and `jig install`
find `.jig` in the working directory or its parents, so they can be run from anywhere in the project. They configure the
project first if it is not, from the `Jig` in the closest of those directories with one, or again from what it was
configured with if its build directory has been removed or `.jig` is from another version of jig. `jig build` takes the
targets to build, which are relative to the build directory, or the names `custom/<name>`, `check/<label>`, `test`,
`test/<name>`, `doc`, `doc/<name>`, `lint` and `lint/<name>`. Without targets, it builds the crates and custom commands
with `default`, or if none has it, every crate, test harness, documentation and custom command, but not the checks,
which are for rust-analyzer, nor the lints. It takes options before them: `-j <jobs>`, `-k <failures>`, `-n` for a
dry run, which shows why each edge would run, `-v` to show commands rather than descriptions, and `--quiet`.
rustc and clippy are run in colour, as their output is captured, and the colour is left out where the build's output
is not a terminal, so that what is built does not depend on where its output goes. `jig --color <when>`, before the
command, chooses otherwise, as `auto`, `always` or `never`, for the build, `jig lint` and jig's own diagnostics, such as
`jig --color=always build | less -R`.
With colour, where the build's output is a terminal, rig shows the build in place:
a line for each of the commands which run at once, by the order it started in, with how long it has run, such as
`[ 2/15] RUSTC libexample.rlib (3m)`, or `[  /15] -` once its command has finished and no other has started in its
place. What has finished is not kept, but what it printed is, above the lines, and once the build is done, the lines
are replaced by a summary, such as `[15/15] completed in 3m 14s`, or `[12/15] stopped after 2.0s` if it stopped first.
`jig --timings <command>` shows on stderr, once the command has finished, how long configuring, building, running a
program and running tests took, each in total, and the ten commands of the build which took longest, from
when each started until it finished, such as `jig --timings test`.
The graph regenerates itself when the configuration, or `jig`, changes, as [when a project is configured
again](../reconfiguring.md) describes. It does so with the `jig` which configured
the project, so running another one reports the `other-jig` warning, which names both, until `jig generate` configures
the project with the one which is running. Its severity is that of `diagnostic/other-jig`
when it was configured, where `'error'` stops the command before building.

```
./build/bootstrap/jig build
./build/bootstrap/jig build -j 8 -k 0 test
./build/bootstrap/jig build -n
./build/bootstrap/jig --timings build
```

## Paths

rig builds in the build directory, so every path of its graph is relative to it, with those outside it leading out
of it with `..`, from where it is once symbolic links are followed. rustc, rustdoc and clippy run there too, and are
given those paths, but the paths they write other than in dep-info, as in diagnostics, panics, `file!()` and debug
info, are remapped to be relative to the project root, as `src/main.rs`, or `build/gen.rs` for a crate generated in the
build directory, with the project root as the working directory of debug info, so that what they write does not depend
on where the project is, and a build in another directory is the same. Custom commands, tests and `run` run from the
project root, so their paths are relative to it as usual.
A path, such as an input, an output, a crate or a linker script, cannot contain a line break, as a depfile cannot have
one, which is the error `newline-in-path`. rig runs a command with its arguments as they are, so an argument, such as an
`env` value or a codegen option, can contain one. A path cannot be empty either, as it would be the directory it is
relative to, which is the error `empty-path`.
