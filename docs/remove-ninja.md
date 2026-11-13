# Removing ninja

A plan for removing ninja support from jig and rig altogether, so that rig is the only engine, and the description of
a build is rig's own rather than ninja's. **Done:** every step below has been carried out, and what each found and
changed is recorded with it.

It reverses decision 4 of [the build engine's plan](build-engine.md#decisions), that generating `build.ninja` would
stay supported permanently as a second engine. That decision was made while rig was new, and ninja was the reference
it was checked against. Since then, rig does what ninja did and more: hashing, the cache, workers, pipelining, Windows.
Meanwhile ninja has become a constraint more than an alternative:

- it can't run on Windows, as it runs its commands with `sh`;
- it can't express a line break in an argument;
- the tests read what was configured from `build.ninja`, until `jig graph` replaced it;
- every crate's flags are built twice, once as arguments and once as text quoted for `sh` with `$` escaped;
- rig's graph is itself built through ninja's evaluation of rules and variables, `rig::ninja::Manifest`.

## What ninja touches now

### In jig

- **The engine.** `JIG_ENGINE` and `configured::Engine`, with `Engine::Ninja`, the `ninja::Ninja` builder, which runs
  ninja and passes `jig build`'s arguments to it, and the `NinjaOnWindows` and `GraphOfNinja` errors.
- **`build.ninja` itself.** `src/ninja.rs`'s `NinjaFile`, which writes it, and `ninja::retire`, which leaves a stub
  `build.ninja` in a build directory which `build-dir` has moved from, for a ninja which is regenerating it there.
- **The description of a build.** `src/graph.rs`'s `Graph` trait, with its `Rule`, `Edge` and `Tee`, which
  `generate.rs` writes rules, variables and edges through, to `NinjaFile` and `RigGraph` alike:
  - the `rustc`, `rustdoc`, `clippy`, `rustc-check`, `custom` and `configure` rules, as `sh` command templates with
    `$in`, `$out` and the edge's variables, `crate-name`, `crate-type`, `flags`, `tmpdir`, `outdir`, `diagnostics`,
    `crate-env`, `emit-metadata`, `command-line` and `name`;
  - `Flags`, which holds each crate's flags twice, as arguments and as `build.ninja`'s text, and `ninja_quote`,
    `shell_quote`, `crate_env` and `toolchain::shell_prefix`, which quote them for `sh` and ninja;
  - the configure rule's `cd <root> && <jig> generate <input>` command, beside the program rig runs.
- **Regenerating for ninja.** `reconfigure.stamp`, which `Configured::locate` touches when a path found missing appears
  or a variable the configuration read changes, and the database's `missing` and `env_hashes`, which it checks them by.
  rig's graph depends on both itself, through `graph.d`.
- **Diagnostics and messages.** `newline-in-argument`, which is only an error with ninja, `newline-in-path`'s wording,
  `proxy-build`'s note that ninja before 1.12 says there is nothing to do, and the comments, docs and messages which
  explain jig by ninja: "the ninja target", "ninja runs in the build directory", "`jig build` takes a subset of
  ninja's options".
- **The tests.**
  - The integration tests' harness generates `build.ninja` beside rig's graph, where ninja is, and keeps it as
    `Output::ninja`, with `Project::ninja`, `ninja_in`, `raw_ninja`, `ninja_depfile`, `has_ninja` and `rig_engine`.
  - Three tests are of `build.ninja` itself: `generates_only_the_engines_graph`, `regenerates_itself` and
    `rebased_paths`. Others check how ninja quotes a command, or run ninja on `build.ninja` where it is installed, as
    `custom_command`, `default_targets` and `missing_paths_reconfigure` do.
  - `src/differential.rs`, which builds random graphs with both ninja and rig, through edits, and compares what ran
    and what was written: the oracle rig's incremental behaviour was checked against.
  - `test/perf.rs`'s comparisons of rig's times with ninja's.
- **CI and bootstrapping.** The `ninja` job, which bootstraps and runs every test with `JIG_ENGINE=ninja`, and
  `bootstrap.sh`, which configures for the engine `JIG_ENGINE` chooses.

### In rig

- **`rig::ninja`.** `Manifest`, which evaluates rules, edges and variables as ninja does, and `Build`, and the error
  `Error::Manifest`. It is public, so it is part of rig's API to users of its `Cargo.toml`.
- **`rig::testing`.** `Graphs`, which builds the graphs of rig's own tests through a `Manifest`, from rules whose
  commands are shell lines.
- **Shell actions.** `Action::Shell`, which runs a command line with `sh`, `sys::shell`, which fails on Windows, and
  their place in the graph format. jig gives every edge a program, so only rig's own tests and a `Manifest` without a
  program make shell actions now. 44 of rig's tests are ignored on Windows for them.
- **What is modelled on ninja,** which stays, as it is rig's behaviour rather than ninja's: the console's output,
  `[finished/total] description`, the status lines in place, `restat`, `generator`, the `console` pool, aliases,
  implicit and order-only inputs, and depfiles in the form gcc and rustc write.

## Decided

- **No deprecation period.** Nothing depends on ninja support, so it is removed outright.
- **No compatibility for what is removed.** `JIG_ENGINE` is no longer read, whatever it is set to, rather than being an
  error for `ninja`. Nor does anything else which only ninja needed keep a shim: `newline-in-argument` is no longer a
  diagnostic, so its name is unknown like any other, and a build directory's `build.ninja`, `build.ninja.d` or
  `reconfigure.stamp` from before is left as it is, unread.
- **No shell actions.** rig runs programs only. A command which needs a shell runs one itself, as `sh -c '...'`, as the
  integration tests' custom commands already do.
- **Inputs and dependencies, rather than explicit and implicit inputs.** ninja splits an edge's inputs by whether
  `$in` names them, which means nothing once rig runs each edge's arguments as they are. rig splits them instead by
  what a missing one means:
  - an **input** is a file the edge reads, which must be there: one which is missing, and which no edge produces, is
    an error before anything runs, as `MissingInput` is now. A crate's sources, a custom command's `input`s, its
    program given by a path, a linker script and a target's specification are inputs.
  - a **dependency** is a file the edge depends on, which runs it again when it changes, but which may be missing: one
    which is missing, and which no edge produces, makes the edge out of date, rather than an error, and the edge runs,
    so that whatever reads it, as rustc, says why if it needs it. The crates a crate uses, as an `extern crate` which
    was deleted along with its declaration, and what a crate's or command's `depends` names, are dependencies, as are
    the inputs an edge discovers in its depfile, as they are now.
  - order-only inputs stay as they are, as **`after`**: built first, but neither read nor depended on.
- **`jig graph` stays, in a form of jig's own.** Its ninja-style statements are replaced, as their `|` and `||`
  separators only mean something to ninja.

## `jig graph`'s new form

Each edge is a block, headed by its outputs, which a reader or a tool looks it up by, followed by an indented line for
each of what it has, as `key = value`, and a blank line:

```text
x86_64-unknown-linux-gnu/libutil.rlib x86_64-unknown-linux-gnu/libutil.rmeta
    description = RUSTC x86_64-unknown-linux-gnu/libutil.rlib
    inputs = ../util.rs
    dependencies = x86_64-unknown-linux-gnu/libcore.rmeta
    after = gen.rs
    command = rustc --crate-type=lib --crate-name=util ... ../util.rs
    depfile = x86_64-unknown-linux-gnu/libutil.rlib.d
    early = x86_64-unknown-linux-gnu/libutil.rmeta
    cache = a207ba36...

test/app
    alias = x86_64-unknown-linux-gnu/test/app

default custom/copy x86_64-unknown-linux-gnu/libutil.rlib
```

- **`outputs`,** in the header, are every file the edge writes, as rig has no other kind but the early ones, which
  `early` names again.
- **`inputs`** are the files it reads which must be there, and **`dependencies`** those which may be missing, as
  above, each once, without the duplicates ninja's split made, as `../app.rs | libutil.rlib libutil.rlib` was.
- **`after`** are the order-only inputs: built first, but not depended on, so that a change to one doesn't run the edge.
- **`alias`,** for an alias, the targets it names, in place of a description and command.
- The other keys are as they are now: `command`, `depfile`, `discover`, `early`, `pass-env`, `pool`, `cache`,
  `target`, `tags`, `any-host`, `generator`, `restat` and `hashed`, without `shell`.
- `--json` has the same keys, with `outputs` and the others which are lists as arrays, and the command in parts.

Its documentation, in the README and the module, describes it without reference to ninja, and the integration tests'
harness finds an edge's block by its header, rather than a `build` line.

## Steps

Each step leaves every test passing, on every platform CI runs, and is a commit or a few on their own.

1. **Done: replace the differential test's oracle.** `src/differential.rs` checks rig against `Model`, a simulation of
   a generated graph without files or processes, about 150 lines, which says which edges a build must run, and what
   each writes.
   - **What it models.** Each file has its contents and the logical time it was last written. An edge which is wanted
     runs if it hasn't run, an output is missing or was written since by other than it, or an input, with the targets
     of an alias in its place and the headers it discovered, has another time than when it last ran. Order-only inputs
     are built first, without running it. It runs as its command does: `cat` of its inputs, and its discovered headers,
     a copy for its implicit output, and a restat edge leaves its output, and its time, alone if it wouldn't change.
   - **What it checks.** After each build, rig must have run the edges the model says, and every output must have the
     contents the model has. Where ninja is installed, ninja is held to the model as well, as it was the oracle the
     model replaces, until step 2 removes it.
   - **Checked.** 256 seeds against the model alone, and 64 against the model and ninja, agree. A model which takes
     touching a file as no change, or whose restat edges always write, fails on most of the first 16 seeds.
   - It is ignored on Windows until step 4, as the generated commands are shell lines.
2. **Done: remove the ninja engine from jig.**
   - `JIG_ENGINE` is no longer read, and is ignored if set. `Engine`, `ninja::Ninja`, `NinjaFile`, `ninja::retire`,
     `NinjaOnWindows` and `GraphOfNinja` are gone. `Configured` has no engine, and `builder()` is rig's. The `Builder`
     trait stays as the seam `Timed` wraps.
   - `reconfigure.stamp` is gone, with `Configured::locate`'s checks of missing paths and variables, and the
     database's `missing`, `env` and `env_hashes`, as rig's graph depends on them through `graph.d`, which always lists
     them. The database's version is 27.
   - `newline-in-argument` is gone, as no argument has to be on a line. A path still can't have a line break, as a
     depfile lists it.
   - `jig build`'s options are documented as rig's, and CI's `ninja` job is gone.
   - The tests: the harness generates only rig's graph. The three tests of `build.ninja` are `generates_the_graph`,
     `regenerates_itself` and `rebased_paths`, which check the configure edge's command and inputs, and paths rebased
     to the build directory, with `jig graph`. `test/perf.rs` holds rig to its own budgets, 1.5 s for a full build of
     its project and 100 ms for one which is up to date, and the differential test checks rig against the model alone.
   - **Checked.** Every test passes on Linux, with `JIG_ENGINE=ninja` set as well, and under Wine, all but those which
     need tools the Wine prefix lacks (nightly, clippy, `echo` and `sh`) pass, 21 being ignored on Windows.
3. **Done: describe builds as rig's, rather than through ninja's rules.**
   - jig builds rig's graph through rig's own API, `Graph::add_edge` and `add_alias`, with each edge's program,
     arguments, environment, working directory, stderr file, description and depfile given directly. `RigGraph`
     interns the paths of an `Edge` and adds it; there are no rules, variables or `Manifest`.
   - The `Graph` trait, `Rule`, `Edge`'s `rule` and `variables`, and `src/graph.rs` are gone, with `path_string` now in
     `rig_graph`. `Flags` holds arguments alone, and `ninja_quote`, `crate_env` and `toolchain::shell_prefix` are gone.
     `shell_quote` is left only in `install.rs`, where the wrappers `--proxy` installs are shell scripts.
   - The differential test builds a `rig::Graph` directly, with its commands as shell lines until step 4.
   - **Checked.** `jig graph`, as text and as JSON, is the same before and after for every project the integration
     tests configure, 119 of them. A description no longer quotes its path for `sh`, as ninja's `$out` did, which none
     of them has a reason to. The differential test agrees with the model on 128 seeds, and under Wine the integration
     tests pass as they did after step 2.
4. **Done: remove `rig::ninja` and shell actions from rig.**
   - rig's tests build their graphs through the API, with `rig::testing::Graphs`, whose rules are templates of the
     steps of the test helper, `rig::test_helper`: the test's own executable, run again with `--rig-test-helper`,
     which a hook registered with `register_test_helper!` runs before the test harness's `main`, and which does what
     its steps say, as `cat`, `cp`, `sleep`, `echo` and writing a depfile, so that the tests need neither `sh` nor
     coreutils. The tests which were ignored on Windows run there, but for two, which test a FIFO jobserver and
     process groups with `ps`, which only Unix has. jig's differential test runs the helper too.
   - `rig::ninja`, `Manifest`, `Build` and `Error::Manifest` are gone, and the README shows a project building a graph
     through the API.
   - `Action`, `Action::Shell` and `sys::shell` are gone: an edge's command, and the program which discovers its
     inputs, are an `Exec`, as is what a worker is asked to run. The graph format's version is 10, the worker
     protocol's 11, and jig's database's 29.
   - rig's graph keeps, for each edge, its outputs, of which its early outputs are some, its inputs, its dependencies
     and what it is built after, in place of ninja's explicit and implicit outputs and inputs. A missing input, or a
     missing file an edge is built after, which no edge produces, is an error, as before, while a missing dependency
     runs its edge, as `Reason::DependencyMissing` says if the edge had it, and an edge which has run without it is
     up to date.
   - jig gives a crate's root, its target's specification and its linker script, and a custom command's inputs and
     program, as inputs, and the crates a crate uses, with their libraries where it links, as dependencies. Unlike
     the decision above, a crate's or command's `depends` stays what it is built after, as the README documents it,
     so that what it names does not rebuild it. `jig graph` shows the new lists, its `|` before the dependencies and
     `||` before what an edge is built after, until step 5 replaces its form.
   - **Checked.** rig's tests pass on Linux, with jig and with cargo, and under Wine, where they had been ignored.
     A plan test checks that a missing input and a missing file an edge is built after are errors, and that an edge
     which lost a dependency runs, and is then up to date; an integration test, that a crate whose used crate and
     `extern crate` were removed is built again. Under Wine, the integration tests pass as they did after step 3.

5. **Done: give `jig graph` its new form,** as above, with the README, the module's documentation, the usage and the
   integration tests' harness and expectations changed to it.
   - Each edge is a block headed by its outputs, with `inputs`, `dependencies` and `after` as keys after its
     description, an alias's targets as `alias`, and `default` before the default targets. Paths are quoted for `sh`
     where they need to be, as the words of a command are, rather than escaped with `$`. The JSON has no `kind`, and
     leaves out the lists an edge does not have, as it does its other keys.
   - jig gives each path of an edge's list once, and leaves out of its dependencies the files which are its inputs,
     so that `libutil.rlib` is no longer listed twice, as a crate used and a library linked.
   - The harness finds an edge's block by its first output, and `files_of` gives its lists, which the tests compare
     whole, rather than finding a `build` line.
   - **Checked.** Every test passes on Linux, and under Wine, the integration tests pass as they did after step 4.

6. **Done: make the integration tests independent of POSIX shell.** Rather than a test crate of its own, the
   integration tests' harness registers rig's test helper, so that their commands, and what `run` runs, are the
   harness itself, run again with `--rig-test-helper`, which a test's `Jig` names as `'HELPER'`. A program given by a
   path, found in `PATH`, or a test's runner is a copy of it, `tool.exe` and so on, named as Windows needs on every
   platform. The helper gained `pwd`, `run`, which runs a program, `$<name>` in `echo`, and `\x<hex>` in `printf`.
   - Of the 18 tests which were ignored on Windows, 14 run there. `run_relative_program` and `run_reference` run
     shell scripts as programs, and `proxy_rebuilds_its_jig` and `proxy_installed_by_itself` replace the `jig` which is
     running and check a wrapper's script, which only Unix does, so they stay Unix only, saying why.
   - `which_function` and `environment_variables_reconfigure` find `rustc`, which every machine which runs the tests
     has, rather than `sh`, and join `PATH` as the platform does.
   - **A bug it found:** a file which uses itself overflowed the 1 MiB stack of the main thread on Windows before it
     reached `MAX_USE_DEPTH`, which was chosen for Linux's 8 MiB. jig evaluates the configuration on a thread with a 64
     MiB stack of its own, so that it stops at the same depth on every platform, and `use_force` no longer raises the
     stack with `ulimit`.
   - **Checked.** Every test passes on Linux. Under Wine, 163 integration tests pass, from 150, and 7 are ignored,
     from 21. The 12 which fail need what the Wine prefix lacks: nightly, rustup, clippy and a custom target, and for
     `impure_functions` and `impure_functions_allowed`, an `echo` program. **Since:** those two run the test helper
     too, their diagnostics showing the lines which run it, with its path, so they pass under Wine without `echo`.

7. **Done: rewrite the documentation.** The README, `build-engine.md` and the module and function docs describe jig
   and rig without ninja: rig's behaviour by itself, as its console's output, rather than by what ninja does.
   - `build-engine.md` is titled as rig's, says at its start that ninja was removed, and describes jig's use of ninja
     in the past tense, as what the plan began from. Its design describes rig's edges by their inputs, dependencies and
     what they are built after, and its API sketch is the API as it is. The phases, which are history, stay as they
     were written, under a note that ninja was since removed. The risk of matching ninja's behaviour is now that of
     behaving as expected without it, which the model of the differential tests answers, and decision 4 says that
     ninja was removed, and why.
   - The module and function docs of jig and rig no longer explain what they do by ninja, as `plan.rs` the rules of
     a plan, now with dependencies, `exec::Console` what it shows, and the attributes of crates and custom commands
     what builds them. The test helper's graphs name an alias `alias` rather than `phony`.
   - What still names ninja is history, as the differential test's account of its oracle, and the tests which check
     that a build directory has no `build.ninja`, and that `JIG_ENGINE=ninja` is ignored.

## Risks

- **Losing the oracle.** ninja is the independent implementation rig's incremental builds were checked against. Step 1
  replaces it with a model, which is only as good as it is simple. It is checked against ninja before ninja goes, and
  kept small enough to be read in one sitting.
- **The rework of step 3.** `generate.rs` is most of jig's code which describes builds; rewriting how it describes them
  risks changing what is built. `jig graph`'s output for every integration test's project, compared before and after,
  is the check that it doesn't.
- **rig's API.** `rig::ninja::Manifest` was the easiest way for a cargo project using rig to build a graph. Nothing
  depends on it, but the API which replaces it should be as easy, which step 4's rewrite of rig's own tests shows.

## Not in this plan

- Reading ninja files, as a way to build other projects with rig. rig was never a ninja replacement for other
  projects' `build.ninja`, and isn't made one.
- Configuring natively beyond what is done, which ninja's removal doesn't change. `jig generate` no longer runs
  rustc for its queries while the toolchain is unchanged, as build-engine.md's phase 2 step 3 says.
