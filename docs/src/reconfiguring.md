# When a project is configured again

Configuring a project evaluates its `Jig`, and writes the graph, `graph.d` and `.jig`. A command brings the
configuration up to date before it builds, so a project is configured again whenever what it was configured from has
changed, without being asked to.

## What it depends on

The edge which generates the graph, which `jig build -n` shows as `CONFIGURE graph`, has as its inputs:

- the root file, `Jig`, and the `jig` which configured the project;
- every file the configuration read, through `use`, `read()` and `exists()`, and the user's configuration;
- the files the `file` statements generate, so that one which is removed is written again;
- the paths it found missing, such as a file `exists()` didn't find, or the user's configuration before it is created,
  so that creating one configures the project again;
- the directories `which` searched, and the directories of the standard libraries `has-std` looked for;
- the programs of cached custom commands found in `PATH`, and the linkers of what is linked;
- the file which identifies the toolchain, when the cache is on, so that a toolchain updated in place configures it
  again;
- and the environment variables it read, as `# env-dep:` lines of `graph.d`: those `env()` read, `PATH` when `which`
  searched it, `RUSTUP_TOOLCHAIN`, and those which set the cache, the workers and the install prefix, such as
  `JIG_CACHE` and `JIG_WORKERS`. Only a hash of each value is kept.

`graph.d` is a depfile, which can be read to see what a project depends on.

## Starting over

A project is configured from the start, rather than by its graph, when:

- it has no `.jig`, or one from another version of jig;
- its graph is missing, as when the build directory has been removed;
- or a command is given a toolchain with `+<toolchain>` other than the one it was configured with (see
  [toolchains](toolchains.md)).

## How

When only the configuration is out of date, and the `jig` which configured it is the one running, jig configures the
project in its own process, which shows `[1/1] CONFIGURE graph`. Otherwise, as when a project builds the `jig` which
configures it, rig runs `jig generate` as an edge, once what it needs is built. A `jig` other than the one which
configured the project reports `other-jig`, as the graph is configured again with the one which configured it.

What configuring asks rustc, its sysroot, host, targets and version, is kept in `.jig`, with a key of what chose the
toolchain and what identifies it: `RUSTUP_TOOLCHAIN` or the toolchain given, `PATH`, `RUSTUP_HOME`, `CARGO_HOME`, the
`rustc` found in `PATH`, rustup's `settings.toml`, the `rust-toolchain` and `rust-toolchain.toml` files above the
project, and the toolchain's identity. While none of it has changed, configuring runs no rustc, and a project of one
crate is configured again in about 20 ms.

What changes the toolchain rustup chooses, as `rustup default` or a new `rust-toolchain.toml`, doesn't itself configure
the project again. The next configuration, for whatever reason, uses the new toolchain.
