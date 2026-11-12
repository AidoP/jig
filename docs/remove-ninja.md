# Removing ninja

A plan for removing ninja support from jig and rig altogether, so that rig is the only engine, and the description of
a build is rig's own rather than ninja's.

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
2. **Remove the ninja engine from jig.**
   - `JIG_ENGINE` is no longer read. `Engine`, `ninja::Ninja`, `NinjaFile`, `ninja::retire`, `NinjaOnWindows` and
     `GraphOfNinja` go. `Configured` has no engine; `builder()` is rig's. The `Builder` trait, which only rig
     implements then, goes, unless the tests need it as a seam.
   - `reconfigure.stamp` goes, with `Configured::locate`'s checks of missing paths and variables, and the database's
     `missing` and `env_hashes`, as rig's graph depends on them through `graph.d`. The database's version changes.
   - `newline-in-argument` goes, as no argument has to be on a line.
   - `jig build`'s options are rig's, documented as such, rather than "a subset of ninja's".
   - CI's `ninja` job goes, and `bootstrap.sh` configures for rig alone.
   - The tests: the harness generates only rig's graph, and `Output::ninja`, `Project::ninja`, `ninja_in`,
     `raw_ninja`, `ninja_depfile`, `has_ninja` and `rig_engine` go. The three tests of `build.ninja` go, as what they
     tested goes; what else they check, as the configure edge's command and inputs, and paths rebased to the build
     directory, is checked by `jig graph`. The checks of how ninja quotes a command, or which run ninja, go.
   - `test/perf.rs` compares rig with its own budgets alone, and the differential test with its model alone.
3. **Describe builds as rig's, rather than through ninja's rules.**
   - jig builds rig's graph through rig's own API, `Graph::add_edge` and `add_alias`, with each edge's program,
     arguments, environment, working directory, stderr file, description and depfile given directly, rather than
     through rules with `$in`, `$out` and variables, which a `Manifest` evaluates.
   - The `Graph` trait, `Rule`, `Edge`'s `rule` and `variables`, `Tee`, `RigGraph`'s `Manifest` and the rules
     `generate.rs` declares go. `Flags` holds arguments alone, and `ninja_quote`, `crate_env` and
     `toolchain::shell_prefix` go. `shell_quote` stays only where a command line is shown, as `-v` and `jig graph`
     show one.
   - `jig graph`'s output for the integration tests' projects is compared before and after, so that the rework changes
     nothing that is built. This is done before step 5 changes its form, so that the comparison is of like with like.
4. **Remove `rig::ninja` and shell actions from rig.**
   - `rig::testing::Graphs` builds rig's test graphs through the API, with programs rather than shell lines. A test's
     commands, as `cat`, `cp`, `sleep` and writing a depfile, are done by the test harness itself, run again as a
     helper program with its arguments, as a test harness can find its own executable, so that the tests need neither
     `sh` nor coreutils, and run on Windows. The 44 tests ignored on Windows run there.
   - `rig::ninja`, `Manifest`, `Build` and `Error::Manifest` go. The README shows a cargo project using rig how to
     build a graph through the API.
   - `Action::Shell`, `sys::shell` and the graph format's shell actions go, which changes the graph format's version,
     so that a graph from before is generated again. `Action` is a program alone, which may be a shell a command
     names, as `sh -c`.
   - rig's graph keeps, for each edge, its outputs, its early outputs, its inputs, its dependencies and its
     order-only inputs, in place of ninja's explicit and implicit outputs and inputs, which changes the graph format's
     version. Its plan errors for a missing input which no edge produces, as now, but takes a missing dependency as
     out of date, as a discovered one is, so that a crate whose `extern crate` was deleted with what it used is
     built again, and rustc says why, if it still needs it. jig's `Edge` gives each list, and decides which of an
     edge's files are inputs and which dependencies, as the decision above lists them.
     - rig, plan: a missing input which no edge produces is an error, and a missing dependency runs its edge, and
       an edge which no longer needs it is up to date once it has run.
     - Integration: a crate which used another, once both the other and its `extern crate` are removed, is built
       again, without an error from rig.

5. **Give `jig graph` its new form,** as above, with the README, the module's documentation and the integration tests'
   harness and expectations changed to it.
6. **Make the integration tests independent of POSIX shell.** The 18 integration tests ignored on Windows for running
   POSIX shell use a helper program instead, as rig's do: a small test crate of the project, as `jig-test-tool`, which
   copies, writes, prints its arguments and fails with a status, as the tests' custom commands need. Those which test
   running a shell script as a program stay Unix only.
7. **Rewrite the documentation.** The README, `build-engine.md` and the module and function docs describe jig and rig
   without ninja: rig's behaviour by itself, as its console's output, rather than by what ninja does. Phases which
   describe history, as phase 1's "with ninja's behaviour", stay as they were, with a note that ninja was since
   removed. Decision 4 says ninja was removed, and why.

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
- Configuring natively beyond what is done, as `jig generate` without a process for rustc's queries, which ninja's
  removal doesn't change.
