# Environment variables

A variable is an input of an edge, as a file is, stamped by a hash of its value, or as unset. Only the hash is kept,
in rig's state and in `.jig`, never the value, which may be a secret, so `jig build -n` says that a variable changed,
but not what it was.

The variables an edge has are:

- **Its own.** The variables its command sets, as a crate's `env` and the toolchain's `RUSTUP_TOOLCHAIN`, are part of
  the command, so a change to one changes its hash.
- **Discovered.** The depfile's `# env-dep:` lines, which rustc writes for what `env!` and `option_env!` read, are
  recorded with the inputs the depfile lists, so a crate which reads a variable is rebuilt when it changes. The value
  stamped is the one the build's commands see, not the one in the depfile, which the command may have set itself.
- **Passed on.** A command's `pass-env` names the variables it reads from those it inherits. Their values are inputs,
  and their names are part of the command.
- **The configuration's.** `graph.d` lists, as `# env-dep:` lines, the variables the configuration read with `env()`,
  `PATH` when `which` searched it, and those which set the cache, workers and install prefix, so that a change to one
  configures the project again.

## Hermetic edges

An edge which is cached runs with its environment cleared, then given only:

- the variables which locate tools and files, `rig::exec::Options::locating`: `PATH`, `HOME` and `TMPDIR`, and on
  Windows `SystemRoot`, `SystemDrive`, `windir`, `TEMP`, `TMP`, `USERPROFILE`, `PATHEXT` and `ComSpec`, to which jig
  adds `RUSTUP_HOME`, `CARGO_HOME` and `RUSTUP_TOOLCHAIN`, for rustup's proxies;
- the variables it passes on;
- the build's own variables, `Options::env`, and the jobserver's.

So every variable which can change what it writes is in its key in the cache, and it runs the same on a worker, which
is sent the same variables, as here. One which reads a variable it doesn't declare fails here as it would there,
rather than being cached without it. A variable rustc discovers isn't passed on, as it is only known once the crate
has compiled, so a cached crate which reads one must pass it on. An edge which isn't cached inherits every variable.

`PATH` isn't an input, as adding an unrelated directory to it would rebuild everything. The programs found in it are
identified instead: rustc and clippy by the toolchain, a cached custom command's program by its contents, and a linker
by its file and what it prints for `--version`.
