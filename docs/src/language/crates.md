# Crates

A `crate` statement may be followed by indented attributes:

- `target <string>`: the target to compile the crate for, which is a builtin target or one declared by a `target`
  statement, or a list of them to declare the crate for each. Without it, the `target` property at the point of the
  declaration is used. A crate can be declared once for each target
- `use <name>`: a crate for the same target which the crate depends on, passed to rustc with `--extern`. It can be
  repeated, and the crate can be declared later or in another file. It can be a list of them instead, written in `[]` or
  computed in `{}`, such as a property, `use {drivers}`, whose items are names, or crates which have been declared, so
  the crates a crate uses can be chosen by the configuration. A name can also be a [static](static-libraries.md) or
  [shared library](shared-libraries.md) for the same target, which the crate links
- `edition <edition>`: the edition of Rust the crate is compiled with, `2015`, `2018`, `2021` or `2024`, as a number or
  a string, in place of the `edition` property at its declaration, which is `2024` by default. rust-analyzer and
  [jig fmt](../commands/fmt.md) are given it too
- `flags <arguments>`: arguments of rustc which aren't codegen options, as a string or a list of them, such as
  `flags ['-l', 'static=boot', '--check-cfg', 'cfg(board, values("qemu"))']` or `-Z` options on nightly, given after
  those jig gives, when the crate is compiled, checked and linted, but not documented, as rustdoc doesn't take all of
  them. rustc runs in the build directory, so a path in them is relative to it. It can be repeated
- `cfg <option>`: a configuration option, given with `--cfg`, as a name, a list of names, or a dict of names to values
  or lists of them, as `<name>="<value>"`, such as `cfg [feature: ['uart', 'plic']]`. It can be repeated, and is also
  given to rust-analyzer
- `env <variables>`: the environment variables rustc is run with for the crate, which `env!` reads, as a dict of
  their names to their values, such as `env [UART_BASE: uart-base]`, whose names are letters, digits and `_`. It can be
  repeated, and is also given to rust-analyzer. The crate is built again when its `cfg` or `env` change, as they are
  part of its command
- `pass-env <names>`: the environment variables rustc inherits for the crate, which `env!` reads, as a name or a list
  of them, such as `pass-env 'GIT_HASH'`. The crate is built again when one of their values changes, as
  it is when a variable `env!` reads changes, and a crate which is cached, when the cache is on, inherits only
  these and those which locate tools: `PATH`, `HOME`, `TMPDIR`, `RUSTUP_HOME` and `CARGO_HOME`. So a cached crate
  whose `env!` reads any other variable from the environment fails to compile, rather than being cached without it
- `worker-tag <tags>`: the tags a worker must have, as `jig worker --tag` gives them, for rig to send it the crate's
  commands, as a tag or a list of them, such as `worker-tag 'fpga'`. It can be repeated
- `change-detection <str>`: how rig finds that the crate's files have changed, `'timestamp'` or `'hash'`, in place of
  the `change-detection` property, as for a [custom command](custom-commands.md)
- `cache <bool>`: whether rig keeps what is built of the crate in its cache, when it is on, which it does by default,
  so that `cache false` leaves out a crate whose output depends on what its inputs do not say
- `depends <path>`: a file which is built before the crate, its test harness and its test, but which is not an input,
  so the crate is not rebuilt when it changes, such as a program its tests run. It is a path relative to the file, a
  reference, or a list of them, and can be repeated
- `builtin <bool>`: pass the crate with `--extern` to every crate for the same target which is not builtin, like the
  standard library. Builtin crates are not passed to each other, so a builtin crate must `use` the ones it depends on.
  It is `false` by default, and `true` without a value
- `crate-type <type>`: `'lib'`, which is the default, `'bin'` for an executable, `<build-dir>/<target>/<name>`, which
  cannot be used by other crates, or `'proc-macro'` for a procedural macro. A procedural macro is compiled for the
  host target, as rustc runs it, so it is not compiled for the `target` property, and can be used by crates for any
  target. `'staticlib'` and `'cdylib'` are libraries for C, a static library, `<build-dir>/<target>/lib<name>.a`, with
  the crates it uses and the standard library in it, and a shared one, named as a
  [shared library](shared-libraries.md) of C is, which the [static libraries](static-libraries.md), shared libraries
  and [executables](executables.md) of C link by `use`, and which crates cannot use. A static one is built with
  `--print=native-static-libs=<build-dir>/<target>/lib<name>.a.libs`, the libraries of the system it needs, which
  what links it reads as `@<file>`, and of which rustc prints a note as it builds it. A shared one is given its soname,
  `lib<name>.so`, on ELF, so that what links it records it by its name rather than its path in the build directory,
  and `jig run` and `jig test` find it, as they find a shared library of C
- `default <bool>`: build the crate when `jig build` is run without targets, which is `false` by default. Without a
  value, it is `true`
- `codegen/<option> <value>`: a codegen option for the crate, which replaces the property of the option. See [codegen options](codegen.md)
- `link-script <path>`: the linker script of the crate, which replaces that of its target, and is passed to the linker
  in the same way
- `test`: build the crate's tests with `--test` as `<build-dir>/<target>/test/<name>`, and register a test which runs
  them, named after the crate, or as `<name>@<target>` if the target is not the host target. Only crates for a target
  whose `test` is `true` can be tested, which it is by default for the host target and a builtin target which the host
  can run, as found by `can-run`. It is `false` by default, and `true` without a value, so `test {target}.test` tests a
  crate where its tests can run. For a crate declared for several targets, it can instead name the targets its tests
  are built for, as a target or a list of them, such as `test host-target` or `test [target, host-target]`, while its
  tests aren't built for its other targets, nor for any with `test []`
- `tag <tag>`: a tag of the crate's test, or a list of them, by which `jig test` chooses it, such as `tag 'slow'`. A
  tag is not empty and has no whitespace. It can be repeated, adding to them
- `timeout <seconds>` and `serial <bool>`: how long the crate's test may run, and whether it runs with nothing else
  running, as for a [`test` statement](statements.md)
- `doc`: document the crate with rustdoc in `<build-dir>/<target>/doc/<name>`, which `jig build doc/<name>` builds, or
  `doc/<name>@<target>` if the target is not the host target, and `jig build doc` builds with the documentation of
  the other crates. The crates for a target are documented in the same directory, so that they link to each other. It is
  documented with the options it is checked with, and again when its sources change. It is `false` by default, and
  `true` without a value
- `doctest`: run the examples in the crate's documentation as a test, `<name>-doc`, with `rustdoc --test`, against the
  library, and the crates it uses, once they are built, with its edition, `cfg` and `env`, and the `tag`, `timeout` and
  `serial` of its tests. It is `false` by default, and only a library for the host target has one, as rustdoc runs
  the examples it compiles. The arguments `jig test` passes on are given to the examples' harness, as
  `jig test lib-doc -- four` runs those of `four`
- `install <bool>`: install the crate's output with `jig install`, which is `false` by default. Without a value, it is
  `true`
- `install-dir <str>`: the directory the output is installed in, relative to the prefix unless it is absolute, which is
  `'bin'` for a binary and `'lib'` otherwise
- `install-mode <mode>`: the permissions of the installed output, such as `0o644`, which are `0o755` for a binary or
  a procedural macro and `0o644` for a library. A mode is an integer, or a symbolic mode, as taken by chmod, which
  changes the default: clauses separated by `,`, of who they apply to, `u`, `g`, `o` or `a`, or everyone without any,
  and operations, `+`, `-` or `=`, with the permissions `r`, `w`, `x`, `X`, `s` and `t`, or those of `u`, `g` or `o`,
  such as `'u+s'` or `'go-w'`. There is no umask, so `'+x'` is `'a+x'`

<!-- jig-test
declare rust-src
    default '{rustc/sysroot}/lib/rustlib/src/rust'
-->
```jig
crate core '{rust-src}/library/core/src/lib.rs'
    builtin

crate compiler_builtins
    builtin
    use core

crate build_tool
    target host-target
```

A crate built for the host and for a target the host can't run, whose tests are built only for the host:

```jig
crate util 'util.rs'
    target [host-target, 'riscv64gc-unknown-none-elf']
    test host-target
```
