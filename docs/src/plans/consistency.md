# Consistency and gaps

A plan for the places where jig's commands, statements and settings disagree with each other, and for what a project
would expect of a build system which jig does not have yet. It comes from a survey of the commands, of the attributes
each statement takes, and of the settings, in November 2026. **Phases 1 to 5 are done**, and the rest is not started.

The phases are ordered by how much each is worth against what it costs, and each stands on its own. jig is not
stable yet, so a phase which changes what a project writes, or the variables it reads, replaces the old form rather
than keeping it for a while beside the new one.

## Phase 1: tests

`jig test` built what the tests need with rig, then ran each test itself, one at a time. So a test which hung hung
`jig test`, one test of a crate's harness could not be run alone, and a machine with many cores ran one test at a
time.

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

### 1b: what a test declares (done)

- `timeout <seconds>` on a `test` statement, and on a crate with `test`, with a `test-timeout` property as the
  default of both, which `--timeout` replaces. It is recorded in the database, whose version changes.
- `serial` on a test which must not run beside another, so that the default number of jobs can become the machine's,
  as it is for a build. Until a project marks such tests, `test-jobs` chooses the default.
- `env` and `pass-env` on a `test` statement, as phase 3 gives every statement which runs a command.
- A test runs in a process group of its own, as rig runs a command, so that a timeout, or an interrupt, stops what
  the test started too. rig's process groups are private to it, behind `rig::sys`, so this needs a small public
  interface in rig for running a program with a timeout, which a worker would then use for a test it is sent.

How it was done, with 1c, which made each test which runs here an edge of rig's graph, `test-run/<name>`: its
`timeout` and `serial`, and the `test-timeout` property, are the edge's, in the graph rather than the database, and a
test runs in the process group rig gives every command. Beside the plan:

- There is no `test-jobs`. Tests run in the build's jobs, so `-j` is the jobs of both, and the default is the
  build's, as many as the machine runs at once and two more, rather than one. A test which must not run beside
  others is `serial`, as jig's own performance tests are.
- A `serial` test runs with nothing else running, build or test, as what disturbs a test which times what it runs is
  anything else running.
- `pass-env` is left out of a test, as a test inherits every variable `jig test` has, and is never cached, so which
  ones it reads changes nothing.
- A timeout, and a process group, were at first only those of a test which runs here; 1d gives them to a test on a
  worker too.
- A test runs while rig holds the build's state, so a test which builds the same project with jig cannot, where it
  could when `jig test` ran the tests after the build.
- The results are shown once the build is done, rather than as each test finishes, as the build's console shows the
  tests running, and `finished in` is the time of the build, with the tests, rather than of the tests alone.

### 1c: tests as edges which run always (done)

Each test becomes an edge of rig's graph, which `jig test` builds with what the test needs, so that tests run in the
build's jobs as soon as what each needs is built. A test is generally not pure, as it reads files,
the clock, the network and the machine beyond what it declares, so its edge runs `always`: it runs every time it is
wanted, and is never cached, nor skipped as up to date.

- rig gains `always` edges, planned with the reason `Always` whenever they are wanted, which are neither cached nor sent
  to workers, whatever their `cache` says.
- rig gains a timeout on an edge, which kills the edge's process group, and the outcome `TimedOut`, which is 1b's
  process group.
- rig gains the `Serial` pool, for 1b's `serial`.
- `jig test` gives each test what is decided when it runs, rather than when the project is configured, through
  `Options::given`: the arguments after `--`, the colour of a test harness's results, the search path of the
  project's shared libraries, and `--timeout`. It is only given to an edge which runs always, as it is not part of
  the edge's action, which a cached or recorded edge's must be.
- A failed test does not stop the build, which goes on with `-k 0`, and what each test printed and how it finished
  are collected by an observer, which shows the build's console only that it ran.
- An edge which runs always isn't given the jobserver rig creates, as cargo gives none to a test: rig's own tests,
  which count on their builds running edges at once, waited for tokens the tests around them held.

### 1d: tests on workers as edges (done)

A test sent to a worker was sent by `jig test` itself once the build was done, apart from the build: it was not shown
running, took no timeout, ignored `serial` and the crate's `worker-tag`, and every test of a target went to the first
worker which ran it. It is now a `test-run/<name>` edge too, placed to run only on workers:

- rig's placement gains `runs`, the target a worker's host must run. Such an edge never runs here: it is sent, without
  a toolchain, to a worker whose host runs the target, by `runs_on`, which moved from `jig test` into rig, and which
  has its tags, spread over the workers which run it. A failure there is its outcome, `failed on <worker>`, with what
  it printed, rather than a reason to run it here, and one which no worker runs fails with what each worker runs.
- A request to a worker carries the edge's timeout, which the worker enforces on the command's process group, and the
  reply says whether it timed out. A worker is named as `workers/list` gives it, for the messages.
- A test's harness is given by the absolute path of the build directory, which rig sends as its marker, as a worker's
  build directory is not `build` beside the project root.
- The database keeps only a test's name, tags and whether it is a test harness: its command, variables, inputs and
  target are the edge's.

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

- `pass-env` is left out of a test, which takes only `env`: a test runs with everything `jig test` inherits, and is
  never cached, so which variables it reads means nothing.
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
  and `workers/download`.
- `cache/token-file` is the only one of them without a variable, and gets one.
- The variables are named after their properties, such as `JIG_CACHE_DIR` and `JIG_CACHE_REMOTE`.
- What `jig help` and the properties' own descriptions say of `cache/remote` and `cache/dir` matches the book, which
  has the `tcp://` and `http://` forms and the directory on Windows.
- A size, remote, worker or `download` which is not valid is a diagnostic where the property was set, when it was
  set by the configuration rather than by a variable.

How it was done: `Setting`, in `src/cache.rs`, is a setting's property with its variable, and `SETTINGS` lists them.
`Project::set_property` checks a setting's value as it checks `change-detection`'s, with the error
`invalid-setting`. Beside the plan:

- Every variable follows one rule, `JIG_` and the property's name in capitals with `_` for `/` and `-`, so they are
  `JIG_CACHE_DIR`, `JIG_CACHE_SIZE`, `JIG_CACHE_REMOTE`, `JIG_CACHE_PUSH`, `JIG_CACHE_TOKEN_FILE`,
  `JIG_WORKERS_LIST`, `JIG_WORKERS_ONLY` and `JIG_WORKERS_DOWNLOAD`.
- The old names of the properties and the variables are gone, as jig is not stable yet: `JIG_CACHE`,
  `JIG_REMOTE_CACHE`, `JIG_REMOTE_CACHE_PUSH`, `JIG_WORKERS` and `JIG_DOWNLOAD` are no longer read, and setting
  `workers`, `workers-only` or `download` is the error `undeclared-property`.
- A value is checked where it is set, rather than once the configuration ends, so a value which is replaced later is
  still reported. The errors of a variable's value are those of before, which do not name the variable.
- jig's own tests use the new variables, so that a developer's `JIG_CACHE_DIR` does not replace the tests' cache.

## Phase 6: commands (done)

- `jig clean [<target> ...]` (done): removes what rig built, by the outputs of the graph, rather than the build
  directory, so that what the project did not build is left, and the state forgets them. `rig::clean` removes the
  outputs, depfiles and stderr files of every edge but the generators, or with targets, of the edges which build them
  and what they are built from, and the directories left empty; `State::forget` rewrites the log without their
  records. It uses the graph which was last generated, without configuring the project again.
- `jig doc [--open] [<name> ...]` (done): builds `doc`, or `doc/<name>`, and says where the first page is. The database
  records each documented crate's first page, so `jig doc` shows each of them, and `--open` opens the first.
- `jig fmt [--check]` (done): runs rustfmt on the root of each crate which is edited, with the project's toolchain and
  edition, and `clang-format` on the sources of the C statements, if it is installed. The database records the roots
  and sources in the project, but not in the build directory, and assembly is left out. jig's own sources are not
  formatted by rustfmt, so it is not run on them.
- `jig lint` runs `clang-tidy` on the sources of the C statements with a `c/tidy` property, from the compile
  commands jig already writes for clangd (done). `c/tidy` is the checks, as `--checks` takes them, read where a
  statement is declared, and empty by default. Each statement with it has an alias, `tidy/<name>`, of what it builds,
  which `lint` includes, so that what its sources include is built first; clang-tidy then runs on its sources each
  time, as its diagnostics are not saved, and `-D` gives it `--warnings-as-errors=*`.

## Phase 7: crates (done)

- `edition <year>` on a crate, and an `edition` property, `'2024'` by default, in place of the `--edition=2024` in
  `src/generate.rs` and in `.rust-project.json` (done). An edition is `2015`, `2018`, `2021` or `2024`, as a number
  or a string, checked where it is set, and `jig fmt` formats each crate with its own, recorded with its root. A
  crate's edition is readable by reference, as `app.edition`. The proxy `jig install --proxy` builds keeps its own.
- `flags <arguments>` on a crate, for what rustc takes which is not a codegen option, such as `-l`, `-L`,
  `--check-cfg` and `-Z` on nightly (done). They follow jig's own arguments when the crate is compiled, checked and
  linted, but rustdoc is not given them, as it rejects `-l`.
- `doctest` on a crate with `doc`: a test which runs `rustdoc --test`, named `<name>-doc` (done). It does not need
  `doc`, as rustdoc tests without documenting. Only a library for the host target has one, as rustdoc runs the
  examples it compiles; the library and the crates it uses are given with `--extern`, and `-L dependency=` finds what
  they use. The database marks the test as rustdoc's, so that `jig test` gives it the arguments after `--` in
  `--test-args`. A library which links C is not given its libraries yet.
- `bench` on a crate: its harness built with the options of `profile/release`, run by `jig bench`, which is a
  command of its own, as a benchmark is not a test which passes or fails (done). The harness is the test harness
  with `opt-level` 3, one codegen unit, and no debug information, debug assertions or overflow checks, but not LTO,
  `strip` or `panic=abort`, as it links the crates built for the tests and unwinds; those crates are not built again
  for speed. Only a crate for the host target has one. `jig bench` runs each with `--bench` and the arguments after
  `--`, one at a time, showing what it prints as it does; `#[bench]` needs nightly, and on stable it runs nothing.

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
