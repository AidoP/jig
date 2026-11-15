# Diagnostics

Each diagnostic has an identifier, shown after its severity, as in `error[unknown-target]`, except for those of `log`
statements without an `id` attribute, whose identifier is `user`. Its severity is replaced by the value of its
property, `diagnostic/<id>`, where it is reported: `'fatal'`, `'error'`, `'warning'`, `'note'`, or `'allow'` to hide
it. A note or suggestion attached to a diagnostic, such as the list of targets after an unknown target, is hidden with
it. Errors within the first argument of `try` are still reported as warnings, except for `impure`. Diagnostics found
once the files are read, such as unknown targets of crates, have the severities set at the end.

A fatal error stops the configuration once the statement which reported it is done, so the rest of its file, the
files which used it and any repetitions of a `@for` are not processed, nor is the configuration checked or generated.
It suits a configuration which cannot go on, such as for an unsupported device, as later diagnostics would only follow
from it.

```jig
set diagnostic/unknown-target 'warning'
set diagnostic/user 'allow'
```

Hiding or lowering an error does not make the configuration valid, so what is generated may not build.

## Every diagnostic

### Statements and syntax

| Identifier | Reported for |
|---|---|
| `user` | a `log` statement without an `id` |
| `unknown-severity` | a `log` statement's severity, or a `diagnostic/<id>`, which jig doesn't have |
| `unknown-statement` | a line which starts with a word which is no statement |
| `unknown-meta-statement` | a meta-statement, after `@`, which jig doesn't have |
| `expected-statement-after-meta` | a meta-statement without a statement on the line after it |
| `else-without-if` | `@else` or `@else-if` which doesn't follow a conditional statement |
| `else-not-first` | `@else` or `@else-if` which isn't the first meta-statement of its statement |
| `then-without-statement` | `@then` which isn't directly after a statement |
| `expected-identifier` | a name which was expected, and is missing |
| `invalid-identifier` | a character which a name can't have |
| `expected-string` | a string which was expected, and is missing |
| `unterminated-string` | a multi-line string without its closing `'''` |
| `string-indentation` | a line of a multi-line string which isn't indented by 4 spaces more than it starts |
| `invalid-string-template` | a `{}` in a string which isn't a valid expression |
| `invalid-integer` | an integer which can't be read |
| `expected-path` | a path which was expected, and is missing |
| `expected-expression` | an expression which was expected, and is missing |
| `expected-exact` | text which was expected, such as a closing `)` or the end of a line |
| `expected-separator` | an argument or item which isn't followed by `, ` or its closing bracket |
| `expected-dict-entry` | an entry of a dict which isn't `name: value` |

### Properties

| Identifier | Reported for |
|---|---|
| `undeclared-property` | a property which is used, or set, and never declared |
| `redeclare-property` | a property which is declared again, or a builtin one which is declared |
| `read-only-property` | a read-only property which is set when it can't be |
| `unset-property` | a property which is used where it has no value |
| `none-selected` | a `select` of which none of the properties has a value |
| `mismatched-type` | a value of another type than the property's |
| `invalid-type` | a `declare` statement's `type` which is no type |
| `unknown-declaration-attribute` | an attribute which `declare` doesn't have |
| `unknown-namespace` | a namespace, `<name>/`, of no property or target |
| `unknown-reference` | a name which is no property, custom command, target or test |
| `no-location` | a `log` statement's `location` which is builtin, so it has none |

### Values and operators

| Identifier | Reported for |
|---|---|
| `invalid-operand` | a value of a type which an operator or function doesn't take, or which can't be coerced to it |
| `mismatched-operands` | the operands of an operator which have different types |
| `integer-overflow` | arithmetic which overflows |
| `shift-amount` | a shift by less than 0 or more than 63 bits |
| `division-by-zero` | `/` or `%` by zero |
| `repeated-key` | a dict with a name more than once |
| `missing-key` | `get` of a dict without the key, and without a default |
| `index-out-of-range` | `get` of a list without the index, and without a default |
| `empty-separator` | `split` by an empty separator |
| `no-values` | `min` or `max` of no values |
| `invalid-version` | a version, or a requirement, which `semver-compatible` can't read |
| `invalid-mode` | a mode which is neither permissions from `0` to `0o7777`, nor symbolic |
| `unknown-lint-level` | a `clippy/<lint>` which isn't `'allow'`, `'warn'`, `'force-warn'`, `'deny'` or `'forbid'` |

### Functions

| Identifier | Reported for |
|---|---|
| `unknown-function` | a call of a function which jig doesn't have |
| `argument-count` | a call with too many or too few arguments |
| `unknown-keyword-argument` | a keyword argument which the function doesn't take |
| `repeated-keyword-argument` | a keyword argument given more than once |
| `positional-after-env` | an argument of `run` after its variables |
| `invalid-program-name` | a program for `which` which is empty or has a `/` |
| `program-not-found` | a program which `which` doesn't find in `PATH` |
| `run-spawn` | a program which `run` can't start |
| `run-failed` | a program which `run` started, and which failed |
| `run-output` | what a program `run` started printed, which isn't UTF-8 |
| `environment-variable` | a variable `env()` reads which isn't set, or isn't UTF-8 |
| `impure` | a call of `run`, whose value the build doesn't follow |

### Files

| Identifier | Reported for |
|---|---|
| `read-failure` | a file which can't be read, by `use` or `read` |
| `repeated-use` | a file which uses itself, directly or through others |
| `use-depth` | files used more than 2048 deep, each by the one before |
| `builtin-file` | `file` on a `use` of a builtin file, which isn't a path |

### Attributes

| Identifier | Reported for |
|---|---|
| `unknown-crate-attribute` | an attribute which `crate` doesn't have |
| `unknown-attribute` | an attribute which `custom`, `executable`, `file`, `shared-library`, `static-library`, `target` or `test` doesn't have |
| `repeated-attribute` | an attribute given more than once which can't be |
| `missing-attribute` | a required attribute which isn't given, such as a custom command's `command` |
| `unknown-attribute-of` | an attribute, after `.`, which what it is taken of doesn't have |
| `attribute-not-set` | an attribute, after `.`, which isn't set |
| `override-attribute` | an attribute which a `target` with `override` can't have |

### Crates and targets

| Identifier | Reported for |
|---|---|
| `redeclare-crate` | a crate declared again for the same target |
| `redeclare-custom` | a custom command, generated file, target or test declared again |
| `invalid-crate-name` | a character which a crate's name can't have |
| `invalid-crate-type` | a `crate-type` which isn't `'lib'`, `'bin'` or `'proc-macro'` |
| `unknown-crate` | a crate which another uses, and which isn't declared for its target |
| `use-binary` | a binary crate which another uses |
| `use-with-target` | a crate of another target which a crate uses |
| `proc-macro-target` | a procedural macro declared for a target other than the host |
| `unknown-target` | a target which is neither builtin nor declared |
| `invalid-target` | a target's name which isn't valid |
| `builtin-target-name` | a `target` statement with a builtin target's name, without `override` |
| `repeated-target` | a target listed more than once |
| `test-target` | a crate tested for a target whose tests can't run |

### Static libraries

| Identifier | Reported for |
|---|---|
| `unknown-source-language` | a source whose extension is not `.c`, `.s`, `.S` or `.sx` |
| `no-c-compiler` | a static library for a target which has no C compiler or archiver, or an executable without a compiler |
| `redeclare-library` | a library or executable declared again for a target, or with the name of a crate or of another for it |
| `unknown-library` | a static or shared library which a crate, library or executable uses, and which isn't declared for its target |
| `shared-library-target` | a shared library for a target without an operating system |
| `use-executable` | an executable which a crate, library or executable uses, as only a static library can be |
| `library-cycle` | a static library which uses itself, through the libraries it uses |
| `name-shadows-system` | a note, of a static library named after one the standard library links, such as `c` or `m` |

### Outputs and installing

| Identifier | Reported for |
|---|---|
| `overlapping-output` | an output which is, contains, or is within another's output |
| `newline-in-path` | a path with a line break, which a depfile can't have |
| `empty-path` | a path which is empty |
| `install-conflict` | a file which two crates, commands or files install |

### Running jig

| Identifier | Reported for |
|---|---|
| `other-jig` | a `jig` other than the one which configured the project, which configures it again |
| `proxy-build` | a program which a wrapper of `jig install --proxy` builds, as it is out of date |
