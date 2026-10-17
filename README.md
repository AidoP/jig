# Jig

A build system for Rust.

**note:** This project is almost entirely vibe-coded with Claude.


## Building

The build system needs to be bootstrapped. To do so, use the `bootstrap.ninja` ninja file.

```
ninja -f bootstrap.ninja
```

Then run the bootstrapped `jig` to install.

```
/build/bootstrap/jig install [prefix]
```

## Usage

This builds `build/bootstrap/jig` and generates `build/build.ninja`. From then on, the project is rebuilt with `jig build`,
which runs ninja in the build directory, and tested with `jig test`.

```
./build/bootstrap/jig build
./build/bootstrap/jig test
```

`jig generate` evaluates `Jig`, writes `build.ninja` to the build directory and records the build directory in
`.jig`, at the project root. `jig build`, `jig check` and `jig test` find `.jig` in the working directory or its
parents, so they can be run from anywhere in the project. `jig build` passes its arguments to ninja, such as the
targets to build, which are relative to the build directory, or the names `custom/<name>`, `check/<label>`, `test` and
`test/<name>`. `build.ninja` regenerates itself when the configuration, or `jig`, changes.

`jig install [<prefix>]` builds the outputs of the crates and custom commands with the `install` attribute, which ninja
builds as `install`, then copies each into its `install-dir` within the prefix, replacing any file there, with its
`install-mode` as its permissions, then prints how many files it installed. The prefix is relative to the working
directory, and is the `install-prefix` property, relative to the project root, without one.

With `--proxy`, a program, which is a binary crate or the output of a custom command with an executable `install-mode`,
is installed as a wrapper which runs `jig build --proxy <prefix> <wrapper>`, then runs the program with the wrapper's
arguments, so that it is rebuilt when it is out of date. Any other file is installed as a symbolic link to its output.
The wrapper runs the `jig` which installed it, writing the build's output to stderr.

`jig build --proxy` brings the configuration up to date, finds the program installed at the wrapper in the prefix, and
builds it with ninja's `--quiet`, which needs ninja 1.11 or later. If the configuration or the program is out of date,
it first reports the `proxy-build` warning, before any diagnostics from configuring. Its severity is that of
`diagnostic/proxy-build` at the end of the configuration when it was last generated, where `'error'` fails without
configuring, building or running the program, and `'allow'` hides it. Then, if the wrapper would be
installed differently, such as when the program's output, its `install-mode` or `jig` has changed, the wrapper is
reinstalled and run again in place of the program. A wrapper whose program the project no longer installs fails.

```
./build/bootstrap/jig install
./build/bootstrap/jig install stage
./build/bootstrap/jig install --proxy ~/.local
```

Ninja runs in the build directory, so paths in `build.ninja` within the build directory are relative to it, and others
are absolute. Custom commands, tests and `run` run from the project root, so their paths are relative to it as usual.


### Statements

- `use <path>`: start processing the `Jig` file in the directory at `<path>`, unless it has already been processed.
  With the `file` attribute, `<path>` is the file itself, of any name. A file which uses itself, directly or through
  other files, raises a warning
- `declare <name>`: define a property that can be used for substitution
- `set <name> <value>`: assign a value to a property
- `crate <name> [<path>]`: introduce a Rust library crate named `<name>`, with its root at `<path>`, or at
  `<name>/lib.rs` relative to the file without one
- `log <severity> <expression>`: report the value of an expression as an `error`, `warning`, `note` or `suggestion`.
  With the `quiet` attribute, only the message is shown, without where it was logged
- `custom <name>`: a custom build command, which ninja runs when its inputs change. Its outputs can be built with
  `jig build custom/<name>`
- `target <name>`: a custom target, which crates can be compiled for. See below
- `test <name>`: a test, which passes if its command succeeds. Like `custom`, it has `input` and `command` attributes,
  but no `output` or `default`. Its inputs are built with `jig build test/<name>`, or those of every test with
  `jig build test`

```
use 'local.jig'
    file

log note 'configured'
    quiet
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
a name can be computed with an expression in `{}`, as long as the whole name is valid.

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


### Meta-statements

A meta-statement is a line starting with `@` which applies to the statement on the next line. There cannot be a blank
line or a comment between them, but several meta-statements can apply to the same statement.

- `@if <expression>`: skip the statement, including its attributes, unless the expression is `true`. The expression
  must be a `bool`, or coerced to one with `{}`. Once a statement is skipped, the conditions of later meta-statements
  are not evaluated

```
@if {device-tree}
@if target == 'riscv64gc-unknown-none-elf'
use 'dtb'
```

- `@else`: process the statement only if no statement has been processed in the chain of conditional statements before
  it. A chain starts with a statement with `@if` and is continued by each statement with `@else`, so `@else` followed
  by `@if` is an else-if. Blank lines and comments may separate the statements of a chain, but another statement ends
  it. `@else` must be the first meta-statement of its statement
- `@else-if <expression>`: the same as `@else` followed by `@if <expression>`

```
@if level == 1
log note 'one'
@else-if level == 2
log note 'two'
@else
log note 'many'
```

- `@for <name> in <value> ...`: process the meta-statements and statement after it once for each of the values, which
  are expressions separated by spaces, where each item of a list is a value. For each, the property `<name>` has the
  value, within the file and until the end of the statement. Without any values, the statement is skipped

```
@for target in targets
@if target != host-target
crate kernel 'kernel.rs'
    target target
```

- `@then`: chain the statement after it to the statement before it, which it must be directly after, so that the
  statements are processed or skipped together, and repeated together by `@for`. The chained statement can have its
  own meta-statements, which apply to it and the rest of the chain

```
@for a in 1 2
set total (total + a)
@then
log note 'added {a}, the total is {total}'
```

- `@with <name> as <value>`: process the meta-statements and statement after it, and the statements chained to it with
  `@then`, with the property `<name>` set to the value, as by `set`, then restore its previous value, unless they set
  it. Unlike `@for`, the property is set everywhere, including in files used by the statements. As with `set`, it can
  be declared later, which checks the value. Conditions after it continue the chain of those before it, or start one

```
@with target as 'riscv64gc-unknown-none-elf'
crate kernel 'kernel.rs'
@then
crate boot 'boot.rs'
```


### Expressions

An expression is a string, `true`, `false`, an integer, a list, a dict, or the name of a property or a crate.

An attribute is taken with `.` after the name of a property, a custom command, a crate, or a declared target, as in
`data.default`, `image.input`, `{host-target}/macros.crate-type`, `macros.crate-type` or `my-kernel.runner`. The
attributes of a property whose value is a reference are those of the crate or custom command it refers to. Those of a
property are `default`, `type`, `read-only` and `info`, those of a custom command are `input`, `output` and `command`,
as lists, and `default`, `install`, `install-dir` and `install-mode`, those of a test, which is looked for after the
others, are `input` and `command`, those of a crate are `target`, `crate-type`, `path`, `output`, `builtin`, `default`,
`test`, `use`, `install`, `install-dir` and `install-mode`, and those of a target are `inherit`, `test`, `runner`,
`override` and its options, such as `code-model`. An attribute which is not set is an error.

A list is written as expressions in `[]`, separated by `, `, such as `['-O', 'dtb']`, and its items can have any type.
Lists can be joined with `+` and compared with `==` and `!=`. As a string, such as in a string template, the items of
a list are separated by spaces, while each item is a separate argument of a command, such as for `run`.

A dict is written as entries in `[]`, as `name: value` separated by `, `, such as `[opt-level: 2, debug: true]`, where
`[:]` is an empty dict. A name is an identifier, or an expression in `{}`, as for a property. The entries are kept in
the order of their names. Dicts can be merged with `+`, where the entries on the right replace those on the left, and
compared with `==` and `!=`. As a string, the entries are written as `name: value` separated by `, `. Entries are read
with `get`, `contains`, `len`, `keys` and `values`.

Integers and integer properties can be combined with `+`, `-`, `*`, `/` and `%`, the bitwise `&`, `|` and `^`, and the
shifts `<<` and `>>`, with a single space on either side of the operator, and grouped with `(` and `)`. Both operands
must have the same type, `unsigned` or `signed`. A shift must be by `0` to `63` bits, and the bits shifted out are
lost, while `>>` keeps the sign of a `signed` integer.

Integers can be compared with `<`, `<=`, `>` and `>=`, and any two values of the same type with `==` and `!=`.
A `bool` can be negated with a leading `!` and combined with `&&` and `||`, while a leading `!` is the complement of
an integer, except in `{}`, which it takes as a `bool`. As in Rust, from lowest to highest, the precedence is `||`,
`&&`, comparisons, `|`, `^`, `&`, `<<` and `>>`, `+` and `-`, then `*`, `/` and `%`.

An expression written in `{}` is coerced to the type the operator requires, or to the type of the other operand.
This converts between `unsigned` and `signed`, parses a `str` as an integer or as `true` or `false`, treats a `bool`
as `0` or `1`, and treats an integer as `true` unless it is zero. Integer literals are only coerced between
`unsigned` and `signed`.

```
set size {pages} * 0x1000 - 1
set large size >= 0x10_0000 && !{small}
```

Functions are called with their arguments in `()`, separated by `, `:

- `upper(str)` and `lower(str)`: convert a `str` to upper or lower case
- `starts-with(str, prefix)` and `ends-with(str, suffix)`: whether a `str` starts or ends with another, as a `bool`
- `select(name, ...)`: the value of the first of the named properties which has a value. Each property must be
  declared, or set before it is declared, so that a misspelt name is an error rather than ignored
- `env(name)`: the value of an environment variable, which must be set. The build is not reconfigured when it changes
- `len(value)`: the number of items in a list or entries in a dict, or of bytes in a `str`, which is UTF-8, as an
  `unsigned`
- `contains(list, value)`: whether a list has an item equal to a value, or a dict has an entry for a key, as a `bool`
- `keys(dict)` and `values(dict)`: the names or values of the entries of a dict, as a list, in the order of the names
- `get(list, index, default)`: the item of a list at an index, counting from `0`, or the value of a dict for a key, or
  the default if there is none, which is an error without a default
- `min(value, ...)` and `max(value, ...)`: the smallest or largest of integers, where each item of a list is a value
- `join(list, separator)`: the items of a list, converted to strings as in a string template, separated by a `str`
- `split(str, separator)`: the parts of a `str` between each non-empty separator, as a list, including empty parts
- `path(str)`: a path relative to the directory of the file containing the expression, as a `str`. An absolute path
  is not made relative. `.` and `..` are resolved without following symbolic links
- `read(path)`: the contents of a file, without trailing newlines, where the path is relative to the file containing
  the expression. The build is reconfigured when the file changes, or is created if it was missing
- `exists(path)`: whether a file or directory exists, as a `bool`, where the path is relative to the file containing
  the expression. The build is reconfigured when an existing path changes, or when a missing one is created, which
  `jig build`, `jig check` and `jig test` check before running ninja
- `try(expression, default)`: the value of the expression, or the default if evaluating it fails. Its errors are reported
  as warnings instead

```
set prefix try(env('CROSS_COMPILE'), '')
```

- `which(name)`: the path of the first executable file with the given name in the directories of the `PATH` environment
  variable. The build is reconfigured when one of the directories searched before it changes
- `run(program, argument, ..., NAME: value, ...)`: run a program and return its output, without trailing newlines.
  As in `sh`, a program containing a `/` is a path, relative to the file containing the expression, while any other
  program is found in `PATH`. A program which is a reference is the path of the crate or custom command's output, which
  is not resolved again. It inherits the environment, with any variables given as `NAME: value` after the
  arguments, where `NAME` is a name like those of properties, so it can be computed with `{}`. The configuration fails
  if the program exits unsuccessfully. The build is reconfigured when a program given as a path changes, but not when
  its output would change otherwise

```
set board select(board, default-board)
@if starts-with(target, 'riscv')
use 'arch/riscv'
```

Within a string, an expression in `{}` is replaced with its value, and `{{` and `}}` with a single brace.
The expression cannot start with `{`, which would be `{{`, so a name computed from its start is put in `()`, as in
`'{({host-target}/core)}'`. It can contain strings, which are templates themselves.

```
log note 'size is {size} with {size / 0x1000} pages'
log note 'home is {env('HOME')}'
```

A multi-line string starts with `'''` at the end of a line and ends with `'''` at the indentation of that line. The lines
between are indented by 4 spaces more, which is removed, or are blank, and are joined by newlines, without one at the
end. A line starting with `#`, in the first column, is a comment which is skipped. Like other strings, each line is a
template.

```
log note '''
    first line
      indented line
'''
```

A `declare` statement may be followed by indented attributes:

- `type <type>`: the type of the property, one of `'str'`, `'bool'`, `'unsigned'`, `'signed'`, `'list'`, `'dict'`
  or `'ref'`
- `default <value>`: the initial value of the property
- `info <string>`: a description of the property, included in diagnostics about it
- `read-only`: the property can only be assigned once, by its default or by a single `set` before or after the declaration

A `crate` statement may be followed by indented attributes:

- `target <string>`: the target to compile the crate for, which is a builtin target or one declared by a `target`
  statement, or a list of them to declare the crate for each. Without it, the `target` property at the point of the
  declaration is used. A crate can be declared once for each target
- `use <name>`: a crate for the same target which the crate depends on, passed to rustc with `--extern`. It can be
  repeated, and the crate can be declared later or in another file
- `builtin`: pass the crate with `--extern` to every crate for the same target which is not builtin, like the standard
  library. Builtin crates are not passed to each other, so a builtin crate must `use` the ones it depends on
- `crate-type <type>`: `'lib'`, which is the default, `'bin'` for an executable, `<build-dir>/<target>/<name>`, which
  cannot be used by other crates, or `'proc-macro'` for a procedural macro. A procedural macro is compiled for the
  host target, as rustc runs it, so it is not compiled for the `target` property, and can be used by crates for any
  target
- `default`: build the crate when `jig build` is run without targets
- `codegen/<option> <value>`: a codegen option for the crate, which replaces the property of the option. See below
- `test`: build the crate's tests with `--test` as `<build-dir>/<target>/test/<name>`, and register a test which runs
  them, named after the crate, or as `<name>@<target>` if the target is not the host target. Only crates for the host
  target, or a custom target with `test`, can be tested
- `install <bool>`: install the crate's output with `jig install`, which is `false` by default. Without a value, it is
  `true`
- `install-dir <str>`: the directory the output is installed in, relative to the prefix unless it is absolute, which is
  `'bin'` for a binary and `'lib'` otherwise
- `install-mode <unsigned>`: the permissions of the installed output, such as `0o644`, which are `0o755` for a binary or
  a procedural macro and `0o644` for a library

```
crate core '{rust-src}/library/core/src/lib.rs'
    builtin

crate compiler_builtins
    builtin
    use core

crate build_tool
    target host-target
```

The stable codegen options of rustc are builtin properties named `codegen/<option>`, such as `codegen/opt-level`, which
have no value until they are set. A crate is compiled, and checked, with `-C <option>=<value>` for each of them which is
set where it is declared, as with the `target` property, unless the crate has an attribute for the option,
`codegen/<option> <value>`, which replaces it. A value is converted to a string as in a string template.
For `target-feature` and `remark`, a list is joined by `,`, for `llvm-args` and `passes` by spaces, and for `link-arg`
each item is a separate option. A path, such as for `linker`, is relative to the project root, unless it is a reference.
`codegen/` is a dict of the options which are set, by name, as a crate declared there would be given them, such as
`[opt-level: 's']`, as described for namespaces.

```
set codegen/opt-level 's'
set codegen/debug-assertions true
crate kernel 'kernel.rs'
crate boot 'boot.rs'
    codegen/opt-level 'z'
```

A `custom` statement is followed by indented attributes:

- `input <path> ...`: files the command reads, as expressions separated by spaces, each a path or a list of them,
  relative to the file containing the statement. It can be repeated, adding to the inputs
- `output <path> ...`: files the command writes, like `input`, but relative to the build directory, `build-dir`.
  Without it, the output is `<build-dir>/<name>`, and each `output` replaces the outputs, which are kept with the
  `output` property, as in `output output 'data'`
- `command <program> <argument> ...`: the command, as expressions separated by spaces, each of which is a single
  argument, except for a list, each item of which is an argument. As with `run`, a program containing a `/` is a path
  relative to the file, and the outputs are rebuilt when it changes, while any other program is found in `PATH`. A
  program which is a reference is the path of the crate or custom command's output. It is required
- `default`: run the command when `jig build` is run without targets
- `install <bool>`, `install-dir <str>`, `install-mode <unsigned>`: install the outputs with `jig install`, as for a
  crate, in `'share'` with the permissions `0o644` by default

A reference is its paths, as they are already resolved, while an absolute path is kept. While the attributes are
evaluated, the `input` and `output` properties are lists of references to the inputs and outputs so far. An output
cannot be, contain or be within the output of a crate, its test harness, or another custom command.

```
custom device-tree
    input 'virt.dts'
    output 'virt.dtb'
    command 'dtc' '-O' 'dtb' '-o' output input
```

A `target` statement is followed by indented attributes, which describe the target with a JSON target specification,
`<build-dir>/targets/<name>.json`. Custom targets need a nightly toolchain.

- `inherit <target>`: the builtin target whose specification it is based on
- `override`: set the attributes of the builtin target with the name, rather than declaring a target. Only `test` and
  `runner` can be used with it, as the builtin target keeps its specification
- `env`, `os`, `arch`, `linker <str>`: replace the field of the specification
- `code-model`, `relocation-model`, `target-cpu`, `linker-flavor`, `panic <str>`: replace the field of the
  specification for the codegen option
- `target-feature <features>`: the features of the target, as a `str` separated by `,`, or a list of them
- `no-redzone`: disable the red zone
- `link-script <str>`: the linker script, which is usually a multi-line string, where braces must be doubled
- `test <bool>`: whether crates for the target can be tested, which is `false` by default. Without a value, it is `true`
- `runner <program>`: the program which runs a test harness for the target, given as its last argument, or a list of the
  program and its first arguments. As with `run`, a program containing a `/` is a path relative to the file. Without
  it, a test harness is run directly

```
target riscv64gc-unknown-none-elf
    override
    test
    runner ['qemu-system-riscv64', '-machine', 'virt', '-nographic', '-kernel']

target bluemetal-riscv64
    inherit 'riscv64gc-unknown-none-elf'
    os 'bluemetal'
    code-model 'medium'
    link-script '''
        ENTRY(_start)
        SECTIONS {{
            . = 0x80000000;
        }}
    '''
```

### Builtin properties

- `sysroot`: the sysroot of the Rust toolchain, as printed by `rustc --print=sysroot`
- `build-dir`: the directory of what is built, relative to the project root, `build` by default. It is read-only
  once it is used, such as by a crate or the outputs of a custom command, so it is set before them. When it changes,
  `build.ninja` in the previous build directory is replaced with a stub, which ninja finds is up to date, and
  `jig build`, `jig check`, `jig test` and `jig install` regenerate `build.ninja` before running ninja, so that they run
  it in the new build directory. The rest of the previous build directory is kept
- `target`: the target triple for crates without a `target` attribute, initially the host
- `install-prefix`: the directory `jig install` installs in when it is not given one, relative to the project root,
  `/usr/local` by default
- `host-target`: the target triple of the host, as printed by `rustc --print=host-tuple`, which is read-only
- `codegen/<option>`: a codegen option of the crates declared while it is set, as described for `crate`, while
  `codegen/` is a dict of those which are set
- `diagnostic/<id>`: the severity of a diagnostic, as described below

### Diagnostics

Each diagnostic has an identifier, shown after its severity, as in `error[unknown-target]`, except for those of `log`
statements, whose identifier is `user`. Its severity is replaced by the value of its property, `diagnostic/<id>`, where
it is reported: `'error'`, `'warning'`, `'note'`, or `'allow'` to hide it. A note or suggestion attached to a
diagnostic, such as the list of targets after an unknown target, is hidden with it. Errors within the first argument
of `try` are still reported as warnings. Diagnostics found once the files are read, such as unknown targets of crates,
have the severities set at the end.

```
set diagnostic/unknown-target 'warning'
set diagnostic/user 'allow'
```

Hiding or lowering an error does not make the configuration valid, so what is generated may not build.
