# jig lint

`jig lint` lints the crates of the project with clippy, which are those not from the toolchain, or only those named,
as `<name>`, or `<name>@<target>` if the target is not the host target. A crate is linted as it is checked, with the
levels of the lints set by the `clippy/<lint>` properties where it is declared. Every crate which can be is linted,
even if another fails, and the diagnostics of each are shown, even if nothing needed linting, as they are saved. It
fails if a lint is an error, such as one which is denied, and with `-D` or `--deny-warnings`, if there is a warning,
which is found in what was saved, so the lints are not run again, as `jig lint -D` is in CI. Options may follow the
names, as in `jig lint app -D`. It takes the options of `jig build` which are about building, `-j`, `-k` and `-q`,
where it goes on after any failure unless `-k` says otherwise.

```jig
set clippy/pedantic 'warn'
set clippy/needless_return 'deny'

crate app 'app.rs'
    crate-type 'bin'
```

It also checks the sources of C and C++ of the libraries and executables declared while the `c/tidy` property is set,
with clang-tidy and the checks it gives, as `--checks` takes them, by the same names, from the compile commands jig
writes for clangd. They are built first, so that what they include is there, and then checked every time, as
clang-tidy's diagnostics aren't saved. Its warnings fail `jig lint -D` as clippy's do. Assembly isn't checked.

```jig
set c/tidy '-*,bugprone-*,clang-analyzer-*'

static-library codec
    source 'codec.c'
```

```
./build/bootstrap/jig lint
./build/bootstrap/jig lint -D app
```
