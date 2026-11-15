# Functions

Functions are called with their arguments in `()`, separated by `, `:

- `upper(str)` and `lower(str)`: convert a `str` to upper or lower case
- `starts-with(str, prefix)` and `ends-with(str, suffix)`: whether a `str` starts or ends with another, as a `bool`
- `select(name, ...)`: the value of the first of the named properties which has a value, whose names are as for a
  property, which may be computed and have a `/`, such as `codegen/{target}/opt-level`, or a local, such as the
  variable of a `@for`. A property which is declared later has no value yet, while one which is never declared, nor
  set, is an error, so that a misspelt name is not ignored. A crate for a target, `<target>/<name>`, which has not been
  declared has no value, as it may be declared later
- `defined(name)`: whether a property has a value where the call is, as a `bool`, such as one which may be set in
  `Jig.local`, whose name is as for `select`
- `env(name)`: the value of an environment variable, which must be set. The build is reconfigured when it changes, or
  is set or unset, which `jig build`, `jig check` and `jig test` check before building. A hash of the value is
  recorded, rather than the value
- `len(value)`: the number of items in a list or entries in a dict, or of bytes in a `str`, which is UTF-8, as an
  `unsigned`
- `contains(list, value)`: whether a list has an item equal to a value, or a dict has an entry for a key, as a `bool`
- `keys(dict)` and `values(dict)`: the names or values of the entries of a dict, as a list, in the order of the names
- `get(list, index, default)`: the item of a list at an index, counting from `0`, or the value of a dict for a key, or
  the default if there is none, which is an error without a default
- `min(value, ...)` and `max(value, ...)`: the smallest or largest of integers, where each item of a list is a value
- `unique(value, ...)`: the values without those equal to an earlier one, as a list, where each item of a list is a
  value
- `sort(value, ...)`: the values in order, as a list, where each item of a list is a value. They must all be `str`,
  which are ordered by their bytes, `bool`, where `false` is first, or integers, which are compared as `signed` if any
  is
- `join(list, separator)`: the items of a list, converted to strings as in a string template, separated by a `str`
- `split(str, separator)`: the parts of a `str` between each non-empty separator, as a list, including empty parts
- `semver-compatible(version, requirement)`: whether a semantic version, `<major>.<minor>.<patch>`, meets a requirement,
  as cargo's default requirement for a dependency, as a `bool`. The version is at least the requirement, and before
  the next version which changes the requirement's leftmost number which is not `0`, or the last number it gives, so
  `1.2` allows up to `2.0.0`, `0.2.3` up to `0.3.0`, and `0.0.3` only itself. A pre-release, such as `1.2.3-beta.1`, only
  meets a requirement for a pre-release of the same numbers, and build metadata, after `+`, is ignored. With
  `ignore-pre-release: true`, pre-releases are ignored, so that a version is compared by its numbers alone, as a
  nightly version of rustc has the features of its release

```jig
@if !semver-compatible(rustc/version, '1.95', ignore-pre-release: true)
log error 'Rust 1.95 or later is needed'
```
- `path(str)`: a path relative to the directory of the file containing the expression, as a `str`. An absolute path
  is not made relative. `.` and `..` are resolved without following symbolic links
- `read(path)`: the contents of a file, without trailing newlines, where the path is relative to the file containing
  the expression. The build is reconfigured when the file changes, or is created if it was missing
- `exists(path)`: whether a file or directory exists, as a `bool`, where the path is relative to the file containing
  the expression. The build is reconfigured when an existing path changes, or when a missing one is created, which
  `jig build`, `jig check` and `jig test` check before building
- `try(expression, default)`: the value of the expression, or the default if evaluating it fails. Its errors are reported
  as warnings instead, except that an impure call is still an error, as evaluating it did not fail

<!-- jig-test
declare prefix
-->
```jig
set prefix try(env('CROSS_COMPILE'), '')
```

- `which(name)`: the path of the first executable file with the given name in the directories of the `PATH` environment
  variable. The build is reconfigured when `PATH` changes, or one of the directories searched before it changes
- `run(program, argument, ..., NAME: value, ...)`: run a program and return its output, without trailing newlines.
  As in `sh`, a program containing a `/` is a path, relative to the file containing the expression, while any other
  program is found in `PATH`. A program which is a reference is the path of the crate or custom command's output, which
  is not resolved again. It inherits the environment, with any variables given as `NAME: value` after the
  arguments, where `NAME` is a name like those of properties, so it can be computed with `{}`. The configuration fails
  if the program exits unsuccessfully. The build is reconfigured when a program given as a path changes, but not when
  its output would change otherwise, so it is impure, as described below

<!-- jig-test
files: arch/riscv/Jig
declare default-board
    default 'virt'
declare board
-->
```jig
set board select(board, default-board)
@if starts-with(target, 'riscv')
use 'arch/riscv'
```

A call to `run` is impure, as the build is not reconfigured when its value changes, which is the `impure` error, so
that the build does not silently depend on what it does not track. It is allowed by setting `diagnostic/impure`, as
with `@with` for a single statement, which is suggested before the statement, or before the meta-statement containing
the call. A call in an `@else-if` has no suggestion, as `@with` cannot come before it.

```
@with diagnostic/impure as 'allow'
set version run('git', 'describe')
```
