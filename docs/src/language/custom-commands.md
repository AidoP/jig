# Custom commands

A `custom` statement is followed by indented attributes:

- `input <path> ...`: files the command reads, as expressions separated by spaces, each a path or a list of them,
  relative to the file containing the statement. It can be repeated, adding to the inputs
- `output <path> ...`: files the command writes, like `input`, but relative to the build directory, `build-dir`.
  Without it, the output is `<build-dir>/<name>`, and each `output` replaces the outputs, which are kept with the
  `output` property, as in `output output 'data'`
- `depends <path> ...`: files which are built before the command runs, like `input`, but which are not inputs, so the
  outputs are not rebuilt when they change. It can be repeated, adding to them
- `command <program> <argument> ...`: the command, as expressions separated by spaces, each of which is a single
  argument, except for a list, each item of which is an argument. As with `run`, a program containing a `/` is a path
  relative to the file, and the outputs are rebuilt when it changes, while any other program is found in `PATH`. A
  program which is a reference is the path of the crate or custom command's output. It is required
- `default <bool>`: run the command when `jig build` is run without targets, which is `false` by default. Without a
  value, it is `true`
- `install <bool>`, `install-dir <str>`, `install-mode <mode>`: install the outputs with `jig install`, as for a
  crate, in `'share'` with the permissions `0o644` by default, which `'+x'` makes executable
- `change-detection <str>`: how rig finds that the command's files have changed, `'timestamp'` or `'hash'`, in place
  of the `change-detection` property, such as to hash the small file a generator writes, so that what reads it is not
  rebuilt when it is written again unchanged
- `cache <bool>`: keep the command's outputs, and what it printed, in rig's cache when it is on, as rustc's
  are, so that it is fetched rather than run when it has run with the same inputs before, in this checkout or another.
  It is for a command whose outputs depend only on its inputs, its arguments and its program, and not on the time, the
  directory it is in, or files which are not inputs. A program found in `PATH` is identified by its contents, and the
  project is configured again when it changes, while one which is a path is an input. A command which is cached is
  stamped by the contents of its files, whatever its `change-detection`. A test cannot be cached
- `pass-env <names>`: the environment variables the command reads from those it inherits, as a name or a list of
  them, such as `pass-env ['CC', 'CFLAGS']`. The command runs again when one of their values changes, and a command
  which is cached inherits only these and those which locate tools, as a cached crate does, so that every variable
  which can change its outputs is part of the key it is cached by
- `env <variables>`: the environment variables the command runs with, beside those it inherits, as a dict of their
  names to their values, as for a crate, such as `env [LC_ALL: 'C']`. It can be repeated, and is part of the command,
  so the command runs again when they change. A test takes it too
- `worker-tag <tags>`: the tags a worker must have for rig to send it the command, when it is cached, as for a crate
- `depfile <path>`: the depfile the command writes, of the inputs it found, in the form gcc writes, relative to the
  build directory, as an output is, and kept with the `depfile` property, as the outputs are with `output`. The command
  runs again when one of the files it lists changes, as a compile of C does when a header changes, and with `cache`,
  they are part of the key it is cached by, as rustc's dep-info is. rig reads the relative paths of a depfile as
  relative to the build directory, so a command which writes the paths it is given runs there, with `dir build-dir`
- `dir <path>`: the directory the command runs in, relative to the file, the project root by default. The paths of
  references, such as `input` and `output`, and a program which is a path, are given to the command relative to it,
  while a string is given as it is
- `description <str>`: what is shown while the command runs, in place of `CUSTOM <name>`, such as
  `description 'GEN tables.rs'`, while `jig build -v` shows the command
- `restat <bool>`: stamp the outputs again once the command has run, so that what reads an output which it left as it
  was, such as a generator which only writes what changed, is not built again. It is `false` by default, and `true`
  without a value
- `console <bool>`: the command has the terminal, rather than its output being captured, and runs alone, as the edge
  which configures the project does, such as for a command which asks for a password. It is not cached. It is `false`
  by default, and `true` without a value

These five, `env`, `pass-env`, `worker-tag`, `change-detection` and `cache`, are taken alike by every statement which
runs a command: a crate, a custom command, and the statements of C, while a test takes only `env`, as it is run by
`jig test` rather than by rig. A command is not cached unless it has `cache`, while a crate and the statements of C are
cached unless they have `cache false`.

A reference is its paths, as they are already resolved, while an absolute path is kept. While the attributes are
evaluated, the `input` and `output` properties are lists of references to the inputs and outputs so far. An output
cannot be, contain or be within the output of a crate, its test harness, or another custom command.

```jig
custom device-tree
    input 'virt.dts'
    output 'virt.dtb'
    command 'dtc' '-O' 'dtb' '-o' output input
```

A generator which finds what it reads, and writes it to a depfile, runs in the build directory, so that the paths it
is given, and writes, are relative to it:

```jig
custom tables
    input 'tables.in'
    output 'tables.rs'
    depfile 'tables.d'
    dir build-dir
    command 'python3' '../gen.py' input output depfile
    description 'GEN tables.rs'
```
