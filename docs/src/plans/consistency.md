# Consistency and gaps

A plan for the places where jig's commands, statements and settings disagree with each other, and for what a project
would expect of a build system which jig does not have yet. It comes from a survey of the commands, of the attributes
each statement takes, and of the settings, in November 2026. **Phases 1a, 2, 3, 4 and 5 are done**, and the rest is not started.

The phases are ordered by how much each is worth against what it costs, and each stands on its own. A phase which
changes what a project writes says how what is written today keeps working.

## Phase 1: tests

`jig test` builds what the tests need with rig, then runs each test itself, one at a time. So a test which hangs
hangs `jig test`, one test of a crate's harness cannot be run alone, every test runs again whether or not anything
it reads changed, and a machine with many cores runs one test at a time.

### 1a: jobs, a timeout and arguments (done)

- `jig test -j <jobs>` runs as many tests at once. The results are still shown in the order the tests were chosen
  in, each as its test finishes, so what is shown does not depend on the number of jobs. One job is the default,
  as tests may share what they write, as jig's own performance tests share the machine.
- `jig test --timeout <seconds>` fails a test which runs for longer, and kills it. What it printed until then is
  shown, after `timed out after <seconds>s`.
- `jig test <name> -- <argument> ...` gives the arguments to each test which runs, after its own, such as a filter
  for a crate's test harness: `jig test rig -- plan::`.
- An option `jig test` does not have is an error, rather than the name of a test.

What it leaves: a timeout kills the test's own process, and not what that process started, and only applies to the
tests which run here, not to those sent to workers.

### 1b: what a test declares

- `timeout <seconds>` on a `test` statement, and on a crate with `test`, with a `test-timeout` property as the
  default of both, which `--timeout` replaces. It is recorded in the database, whose version changes.
- `serial` on a test which must not run beside another, so that the default number of jobs can become the machine's,
  as it is for a build. Until a project marks such tests, `test-jobs` chooses the default.
- `env` and `pass-env` on a `test` statement, as phase 3 gives every statement which runs a command.
- A test runs in a process group of its own, as rig runs a command, so that a timeout, or an interrupt, stops what
  the test started too. rig's process groups are private to it, behind `rig::sys`, so this needs a small public
  interface in rig for running a program with a timeout, which a worker would then use for a test it is sent.

### 1c: tests as edges

A test becomes an edge of rig's graph, whose output is a file of what it printed and how it ended, as a check's
diagnostics are a file. This is the larger change, and replaces the test runner's own scheduling, and its own way of
sending a test to a worker.

- A test whose inputs have not changed does not run again, and `jig test` shows what it printed when it last ran.
  `jig test --rerun` runs them regardless. A test reads what its command reads, which rig cannot see, so only a test
  marked `cache`, or a crate's harness, whose inputs are those of its crate and its `depends`, is skipped this way.
- Tests run in rig's jobs, share the jobserver with what they start, and are sent to workers by their placement,
  as any cached edge is, in place of `Workers` in `src/test_runner.rs`.
- A test which fails must not fail the build of the tests after it, so `jig test` builds with `-k 0`, and a failed
  test's edge still writes its output, with the failure recorded in it rather than in its exit status.
- An edge needs a timeout, which is 1b's interface, used by `rig::exec`.

Open questions: whether a skipped test is shown as `ok (cached)`, as a cached build edge is counted, and how
`--timings` counts it.

## Phase 2: the command line (done)

Each command parses its own arguments, and they have drifted apart.

- One parser of options for every command, in `src/main.rs`, which a command gives the options it takes. An option
  a command does not take is an error for every command. Today `test`, `lint` and `run` take one as a name, and
  `jig install --bogus` installs into a directory named `--bogus`.
- Options are taken wherever they are among the arguments, before `--`. Today `jig lint rig -D` fails, as `-D` is
  only taken first.
- `jig --help`, `jig -h` and `jig <command> --help` show the usage, as `jig help` does, and `jig --version` and
  `jig version` show the version, which jig does not record yet, so a release has to give it to the build.
- `-q` for `--quiet`, as `-v` is for `--verbose`.
- `test`, `run`, `lint` and `install` build first, and take the options of `jig build` which are about building:
  `-j`, `-k` and `--quiet`. `jig test -j` is then the jobs of both, and `-v` keeps its meaning for each command.

How it was done: `Args::parse` in `src/main.rs` parses every command by its `Spec`, which lists its options, whether
it builds first, how many operands it takes, and whether it passes arguments on. Beside the plan:

- `jig run` takes its options before the name of the program, and what follows the name is the program's, as it was,
  so that `jig run app -x` still gives `-x` to the program.
- The options with a short name have a long one too: `--jobs`, `--keep-going`, `--dry-run`, `--quiet`, and the
  worker's `--jobs` has `-j`.
- `-j 0` is an error for every command, as it was for `jig test`, where `jig build` took it as one job.
- The version is the `JIG_VERSION` jig is compiled with, which jig's own `Jig` sets from a `jig-version` property,
  empty by default, which the release workflow sets from the tag. Any other build shows `jig unreleased`, as does
  `bootstrap.sh`, which does not set it.
- `jig build --proxy <prefix> <wrapper>`, which the installed wrappers run, is still parsed apart, as it is internal.

## Phase 3: attributes which some statements lack (done)

| Attribute | Has it | Lacks it |
|---|---|---|
| `env` | crate | custom command, test, static library, shared library, executable |
| `pass-env` | crate, custom command, the C statements | test |
| `worker-tag` | crate | custom command, the C statements |
| `change-detection`, `cache` | custom command | crate, the C statements |
| `install`, `install-dir`, `install-mode` | crate, custom command, file, shared library, executable | static library |

- Each attribute is taken by one function, as the install attributes are by `install_attribute`, which every
  statement which runs a command calls, so that they cannot drift again.
- A static library installs its archive in `lib`, and its `public-include` directories' headers in `include`, with
  `install-headers`, as nothing installs a header today.
- `cache false` on a crate or a C statement leaves it out of the cache, as for one whose output depends on what its
  inputs do not say.
- A `change-detection` property which is not `'timestamp'` or `'hash'` is a diagnostic where it is set, as the
  attribute's is, rather than an error once the graph is generated.

How it was done: `Project::running_attribute`, in `src/project/running.rs`, takes the five attributes into a
`Running`, which crates, custom commands, tests and the statements of C call. Beside the plan:

- `pass-env` is left out of a test, which takes only `env`: a test runs here with everything `jig test` inherits,
  and is not cached, so which variables it reads means nothing until tests are edges, in 1c, which should add it.
- The statements of C keep hashing their files by default, whatever the `change-detection` property, as they did,
  and their own `change-detection` replaces that. A cached edge is hashed whatever it says, as before.
- A static library linked as its objects is archived when it is installed, as there is otherwise no archive.
- The headers are those with the extension `.h`, `.hh`, `.hpp`, `.hxx` or `.inc`, found when the project is
  configured, which depends on their directories, so that one which is added configures it again. They are installed
  in `include` whatever `install-dir` says, which is the archive's, and only with `install`.
- The attributes are not yet readable by reference, as `app.pass-env` is for a crate, for the other statements.

## Phase 4: custom commands (done)

rig runs a custom command as it runs rustc, but a `custom` statement cannot say most of what an edge can.

- `depfile <path>`: the depfile the command writes, of the inputs it found, as a C compile has, so that a generator
  which reads what it is not told of is run again when that changes. With `cache`, what it lists is part of the key,
  as rustc's dep-info is.
- `restat`: the outputs are stamped again once the command has run, so that what reads an output it left alone is
  not built again. rig has it, and jig always leaves it off.
- `dir <path>`: the directory the command runs in, the project root by default, as today.
- `description <str>`: what is shown while it runs, in place of `CUSTOM <name>`.
- `console`: the command has the terminal, and runs alone, as the edge which configures the project does.

How it was done: a custom command's `CommandEdge`, in `src/project/customs.rs`, holds them, and its edge in
`src/generate.rs` is given them. Beside the plan:

- The paths of a `depfile` are read by rig relative to the build directory, as every depfile is, while a command runs
  in the project root, so a generator which writes the paths it is given runs in the build directory, with
  `dir build-dir`, which the book shows. Reading them relative to the directory the command ran in would be simpler
  to use, but the edge which configures the project, which runs in the root with an absolute directory, and whose
  depfile jig writes relative to the build directory, would have to change with it, and jig's in-process
  regeneration reads that directory, so it is left for a decision.
- With `dir`, the paths of references and a program which is a path are given relative to the directory, while a
  string is given as it is, as jig cannot tell a path in a string from any other word.
- The depfile is kept with a `depfile` property while the attributes are evaluated, as the outputs are with `output`.
- `console` is not cached, as rig caches no edge with the terminal, and a test takes none of them.

## Phase 5: settings (done)

- The settings of the cache are `cache/dir`, `cache/size`, `cache/remote`, `cache/push` and `cache/token-file`, but
  `workers`, `workers-only` and `download` beside them have no prefix. They become `workers/list`, `workers/only`
  and `workers/download`, and the old names stay as properties which set the new ones, with a note,
  `renamed-property`, for a release, before they are removed.
- `JIG_REMOTE_CACHE_TOKEN_FILE` overrides `cache/token-file`, which is the only one of them without a variable.
- The variables are named after their properties, `JIG_CACHE_DIR` and `JIG_CACHE_REMOTE`, and `JIG_CACHE` and
  `JIG_REMOTE_CACHE` are still read.
- What `jig help` and the properties' own descriptions say of `cache/remote` and `cache/dir` matches the book, which
  has the `tcp://` and `http://` forms and the directory on Windows.
- A size, remote, worker or `download` which is not valid is a diagnostic where the property was set, when it was
  set by the configuration rather than by a variable.

How it was done: `Setting`, in `src/cache.rs`, is a setting's property with its variables, and `SETTINGS` lists them.
`Project::set_property` sets a renamed property by its new name, with the note, and checks a setting's value as it
checks `change-detection`'s, with the error `invalid-setting`. Beside the plan:

- Every variable follows one rule, `JIG_` and the property's name in capitals with `_` for `/` and `-`, so the token
  file's is `JIG_CACHE_TOKEN_FILE`, rather than `JIG_REMOTE_CACHE_TOKEN_FILE`, and the others are `JIG_CACHE_PUSH`,
  `JIG_WORKERS_LIST` and `JIG_WORKERS_DOWNLOAD`. An old variable is read only when the new one is not set, so the
  configuration depends on it only then, and is read without a note, as nothing shows where a variable was set.
- The old names of the properties are not declared. Setting one sets the new property, and reading one reads it, so
  `defined(download)` and `{workers}` still work. A project which declares a property of an old name itself keeps
  it as its own.
- A value is checked where it is set, rather than once the configuration ends, so a value which is replaced later is
  still reported. The errors of a variable's value are those of before, which do not name the variable.
- jig's own tests use the new variables, so that a developer's `JIG_CACHE_DIR` does not replace the tests' cache.

## Phase 6: commands

- `jig clean [<target> ...]`: removes what rig built, by the outputs of the graph, rather than the build directory,
  so that what the project did not build is left, and the state forgets them.
- `jig doc [--open] [<name> ...]`: builds `doc`, or `doc/<name>`, and says where the first page is.
- `jig fmt [--check]`: runs rustfmt on the root of each crate which is edited, with the project's toolchain and
  edition, and `clang-format` on the sources of the C statements, if it is installed.
- `jig lint` runs `clang-tidy` on the sources of the C statements with a `c/tidy` property, from the compile
  commands jig already writes for clangd.

## Phase 7: crates

- `edition <year>` on a crate, and an `edition` property, `'2024'` by default, in place of the `--edition=2024` in
  `src/generate.rs` and in `.rust-project.json`.
- `flags <arguments>` on a crate, for what rustc takes which is not a codegen option, such as `-l`, `-L`,
  `--check-cfg` and `-Z` on nightly.
- `doctest` on a crate with `doc`: a test which runs `rustdoc --test`, named `<name>-doc`.
- `bench` on a crate: its harness built with the options of `profile/release`, run by `jig bench`, which is a
  command of its own, as a benchmark is not a test which passes or fails.

## Larger, each with a plan of its own

- **Crates from elsewhere.** A project's crates are all in its tree. Crates from crates.io or git need a lock file,
  a resolver of features and versions, build scripts, which jig has none of by design, and a place to keep sources.
  The first step is the smallest: `use` of a directory of vendored crates, each declared by a `Jig` file written
  from its `Cargo.toml` by a tool outside the build.
- **MSVC's tools for C.** `cl.exe` and `lib.exe` take other arguments and write other depfiles than gcc and clang,
  as [C and assembly](c-and-assembly.md) leaves for later.
- **A release for Windows.** CI builds and tests jig there, but a release has no `jig.exe`.
- **CRLF.** A `Jig` file with CRLF line endings is not read. The parser's spans are offsets into the file, so the
  line endings are taken where a line ends, rather than removed first.
