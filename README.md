# Jig

A build system for Rust.

**note:** This project is almost entirely vibe-coded with Claude.

jig builds Rust projects from a `Jig` file, a configuration of crates, custom commands, tests and targets, with rig, its
own build engine, which caches what it builds, and can send it to other machines to build.

## Installing

Each release on GitHub has a statically linked `jig` for x86_64, aarch64 and riscv64 Linux, and a `jig` for macOS on
Apple silicon.

From source, which needs Rust 1.97 or later, `bootstrap.sh` builds `jig` with rustc alone and configures the project,
then the bootstrapped `jig` installs itself:

```
sh bootstrap.sh
build/bootstrap/jig install [prefix]
```

From then on, the project is rebuilt with `jig build`, and tested with `jig test`.

## Documentation

The documentation is a book, [published from main](https://aidop.github.io/jig/), whose pages are in
[`docs/`](docs/src/SUMMARY.md), built with [mdBook](https://rust-lang.github.io/mdBook/) by `jig build custom/book`. What
was in this README is now in the book:

- Installing and Building: [Installing](docs/src/installing.md), [Developing jig](docs/src/developing.md) and
  [rig as a library](docs/src/rig.md)
- Usage: [Commands](docs/src/commands.md), [Toolchains](docs/src/toolchains.md), [Caching](docs/src/caching.md),
  [Workers](docs/src/workers.md) and [The user's configuration](docs/src/user-configuration.md)
- Statements, Meta-statements, Expressions, Builtin properties and Diagnostics:
  [The configuration language](docs/src/language.md)

rig's design is in [rig's design](docs/src/rig/design.md).
