# Expressions

An expression is a string, `true`, `false`, an integer, a list, a dict, or the name of a property or a crate.

An attribute is taken with `.` after the name of a property, a custom command, a file, a crate, or a target, as in
`data.default`, `image.input`, `{host-target}/macros.crate-type`, `macros.crate-type`, `my-kernel.runner` or
`{target}.has-std`, where a name computed in `{}` is put in `()` in a string template, as in `'{({target}.has-std)}'`.
The attributes of a property whose value is a reference are those of the crate, custom command or file it refers to.
Those of a property are `default`, `type`, `read-only` and `info`, those of a custom command are `input`, `output`,
`depends` and `command`, as lists, and `default`, `install`, `install-dir` and `install-mode`, those of a file are
`output`, `content`, `mode`, `install`, `install-dir` and `install-mode`, those of a test, which is looked for after the
others, are `input`, `depends` and `command`, those of a crate are `target`, `crate-type`, `path`, `output`, `builtin`,
`default`, `test`, `doc`, `use`, `depends`, `cfg`, `env`, `link-script`, `install`, `install-dir` and `install-mode`,
and those of a target, builtin or declared, are `inherit`, `test`, `runner`, `link-script`, `override`, `has-std`,
`can-run` and its options, such as `code-model`. An attribute which is not set is an error.

`has-std` is whether the toolchain has the standard library of a builtin target, as installed by `rustup target add`,
which is `false` for a custom target. The configuration is generated again when the target's libraries change, so it
follows the standard library being installed.

<!-- jig-test
declare targets
    default [host-target]
-->
```jig
@for target in targets
@if {target}.has-std
crate app 'app.rs'
```

`can-run` is whether the host can run what is built for a target, which it can for the host target. For another, jig
runs a probe, a tiny program for the target's architecture which exits successfully, with the target's runner if it
has one, so it is `true` where the kernel runs the architecture, or an emulator does, such as one registered with
binfmt or given as the runner, or Wine for a Windows target. Probes are for Linux targets of x86_64, x86, the x32 ABI,
AArch64, 32-bit ARM, RISC-V, little-endian 64-bit PowerPC and s390x, and Windows targets of x86_64, x86 and AArch64, so
another target can only be run if it is the host target. A macOS target is probed on macOS by running `/usr/bin/true`
as its architecture with `arch`, so x86_64 runs on Apple silicon with Rosetta, while it cannot run on another host. The probe is run when the project is configured, so an
emulator which is installed later is found once it is configured again.

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

<!-- jig-test
declare pages
    default '4'
declare small
    default 'false'
declare size
declare large
-->
```jig
set size {pages} * 0x1000 - 1
set large size >= 0x10_0000 && !{small}
```

Within a string, an expression in `{}` is replaced with its value, and `{{` and `}}` with a single brace.
The expression cannot start with `{`, which would be `{{`, so a name computed from its start is put in `()`, as in
`'{({host-target}/core)}'`. It can contain strings, which are templates themselves.

<!-- jig-test
declare size
    default 0x4000
-->
```jig
log note 'size is {size} with {size / 0x1000} pages'
log note 'home is {env('HOME')}'
```

A multi-line string starts with `'''` at the end of a line and ends with `'''` at the indentation of that line. The lines
between are indented by 4 spaces more, which is removed, or are blank, and are joined by newlines, without one at the
end. A line starting with `#`, in the first column, is a comment which is skipped. Like other strings, each line is a
template.

```jig
log note '''
    first line
      indented line
'''
```
