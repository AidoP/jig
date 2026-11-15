# Files jig keeps

## At the project root

- **`.jig`**, the database. `jig generate` writes it, and every other command reads it, to find the project and what
  it needs without evaluating the configuration again: the root file it was configured from, its toolchain, its build
  directory, the tests and their tags, the programs `jig run` runs, the files `jig install` installs, the flychecks and
  lints, how rig caches and which workers it uses, and what configuring asked rustc, with a key of what chose the
  toolchain. A command finds the project as the closest directory to the working directory, or it itself, with a
  `.jig`. One from another version of jig configures the project again, from what it was configured with.
- **`.rust-project.json`**, which describes the crates to rust-analyzer (see [`jig check`](commands/check.md)).

Neither is meant to be committed.

## In the build directory

The build directory is `build` by default, or the `build-dir` property. Every path rig is given is relative to it.

- **`graph`**, rig's graph of the project, and **`graph.d`**, its depfile, of what the configuration read, which
  configures the project again when it changes (see [when a project is configured again](reconfiguring.md)).
- **`.rig/log`**, the state of the build: a record of each edge which has run, by which rig finds what is out of date,
  and **`.rig/lock`**, which stops two builds from running in the build directory at once (see [the
  state](rig/changes.md)).
- **`<target>/`**, what is built for each target:
  - `lib<name>.rlib` and `lib<name>.rmeta` for a library, and its output, by its name, for a binary;
  - `test/<name>`, a crate's test harness;
  - `check/` and `lint/`, the diagnostics of checking and linting each crate, which `jig check` and `jig lint` show;
  - `doc/`, the documentation of the crates with `doc`;
  - `unwind/`, the libraries built again without `panic=abort` for what must unwind;
  - and beside each, the depfile rustc writes, `<output>.d`.
- **`targets/<name>.json`**, the specification of each custom target.
- **`tmp/`**, rustc's temporary files, in a directory for each crate it compiles.
- The outputs of custom commands and `file` statements, where they give them.

Removing the build directory removes all of it, after which the next command configures the project again, and
builds what it needs, fetching from the cache what it can.

## Elsewhere

- **The cache,** by default `$XDG_CACHE_HOME/jig`, `%LOCALAPPDATA%\jig\cache` on Windows, or `~/.cache/jig`, shared by
  every project and checkout of a user (see [caching](caching.md) and [the cache's design](rig/cache.md)).
- **The user's configuration,** by default `$XDG_CONFIG_HOME/jig/config`, `%APPDATA%\jig\config` on Windows, or
  `~/.config/jig/config` (see [the user's configuration](user-configuration.md)).
