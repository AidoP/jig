# Statements

- `use <expression>`: start processing a file, which is one of:
  - a path, as a `str`, to the `Jig` file in the directory at the path, unless it has already been processed. The path
    is relative to the file, or to the project root if it starts with `/`. With the `file` attribute, the path is to
    the file itself, of any name. With the `force` attribute, the file is processed even if it has been already, with
    the values the properties have then, and `force <bool>` chooses whether it is. A file which uses itself, directly
    or through other files, raises a warning, unless it is forced, so that it can use itself until it stops, such as
    with `@if`. Files can be used up to 2048 deep, each by the one before, beyond which a `use` is an error
  - a builtin file, which is part of jig, and is processed each time it is used. Each is a read-only builtin property
    with its name, whose value is a reference to it, of type `'ref'`, which is its name where a string is needed. So
    `profile/` is a dict of the profiles, and a profile can be chosen as `use profile/{name}` or
    `use get(profile/, name)`. The builtin files are:
    - `profile/<name>`: set the codegen options of a builtin profile, `release` or `debug`. Like any other `set`, the
      options apply to the crates declared after it, and can be set again after it. See [codegen options and profiles](codegen.md)
    - `msrv`: report a fatal error, `msrv`, if rustc is older than the minimum supported Rust version, the
      `rust-version` property, which must be set before it. A version is compared as by `semver-compatible`, ignoring
      pre-releases, so a nightly toolchain meets the version of its release
- `declare <name>`: define a property that can be used for substitution
- `set <name> <value>`: assign a value to a property
- `crate <name> [<path>]`: introduce a Rust library crate named `<name>`, with its root at `<path>`, or at
  `<name>/lib.rs` relative to the file without one
- `log <severity> <expression>`: report the value of an expression as a `fatal` error, an `error`, a `warning`, a `note`
  or a `suggestion`, where a fatal error stops the configuration, as [diagnostics](diagnostics.md) describes. With the `quiet` attribute, only
  the message is shown, without where it was logged. With `id <str>`, the diagnostic has that identifier, which is shown
  with it and whose property, `diagnostic/<id>`, overrides its severity, as for other diagnostics, and is declared by
  the first `log` with the identifier. With `location <name>`, it is shown at another statement, rather than where it
  was logged: the one which gave a property its value, or declared it, or which declared a crate, custom command,
  target or test, whose name may be computed, such as `location {target}` in a `@for`
- `custom <name>`: a custom build command, which rig runs when its inputs change. Its outputs can be built with
  `jig build custom/<name>`
- `file <name>`: a file generated from a template string, which is written when the project is configured. See [generated files](files.md)
- `target <name>`: a custom target, which crates can be compiled for. See [targets](targets.md)
- `test <name>`: a test, which passes if its command succeeds. Like `custom`, it has `input`, `depends` and `command`
  attributes, but no `output` or `default`, and a `tag` attribute, a tag or a list of them, by which `jig test` chooses
  it, as [`jig test`](../commands/test.md) describes. Its inputs and dependencies are built with `jig build test/<name>`, or those of every test with
  `jig build test`

```
set rust-version '1.95.0'
use msrv

use 'local.jig'
    file

log note 'configured'
    quiet

log warning 'the `legacy` feature is deprecated'
    id 'deprecated'

@if uart-base % 0x1000 != 0
log error 'the UART base must be aligned to 4 KiB'
    location uart-base
```

A crate is a read-only property, `<target>/<name>`, for each target it is declared for, whose value is a reference to
it, of type `'ref'`. The target is part of the name, so a target from a property is computed with `{}`, as in
`{host-target}/core` or `{get(targets, 0)}/core`, while `host-target/core` is an error, as there is no target named
`host-target`. A crate can also be referred to by its name alone, as `core`, for the `target` property where it is
referred to, unless there is a property with the name. A custom command is a read-only property with its name, whose
value is a reference to it.

Where a string or path is needed, a reference is the path of the crate's output, or the paths of the custom command's
outputs, separated by spaces. These are relative to the project root, and are not resolved again relative to the file
which refers to them.

```
custom image
    input riscv64gc-unknown-none-elf/kernel
    output 'kernel.bin'
    command 'objcopy' '-O' 'binary' input output
```

A `<name>` is identifiers joined by `/`, where an identifier is ASCII letters, digits, `-` and `_`, not starting with a
digit. Crate and target names cannot contain `/`, and crate names cannot contain `-` either, as rustc does not accept
it. Names declared by `declare` cannot contain `/` either, as those are left to crates and codegen options. Any part of
a name can be computed with an expression in `{}`, as long as the whole name is valid. A computed part may also contain
`.`, as some targets do, such as `thumbv8m.main-none-eabihf`, which is referred to as
`{'thumbv8m.main-none-eabihf'}/core`, as a `.` written in a name takes an attribute, while crate names cannot.

```
crate compiler_builtins
set {'heap-{arch}'} 0x1000
set heap-{arch} 0x1000
log note {target}/compiler_builtins
```

In an expression, a name followed by `/` is a namespace, which is a dict of the properties named `<name>/<rest>` which
are set where it is evaluated, by `<rest>`, which may contain `/` itself. So `codegen/` is the codegen options which are
set, `diagnostic/` the severities which are overridden, and `<target>/` the crates for a target, by their names. As in
`6 / 2`, division has spaces around it. A namespace must have a property declared in it, or be a target, so that a
misspelt one is an error rather than empty.

```
log note keys({host-target}/)
@if get(codegen/, 'opt-level', 0) == 3
log note 'optimised'
```


A line starting with `#` is a comment. Indented by 4 spaces, a comment can be among the attributes of a statement, to
comment out one of them.

```
crate app
    # use util
    default
```
