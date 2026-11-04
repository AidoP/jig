# Replacing ninja with an internal build engine

This is a plan for replacing ninja, which `jig build`, `jig test`, `jig check`, `jig lint`, `jig run` and
`jig install` all run, with a build engine of jig's own. The engine is called `rig`.

Goals:

1. Fast and parallel: no slower than ninja on a no-op build, and faster on a full one.
2. Optional hashing of contents, which is more correct than comparing timestamps.
3. Usable as a standalone crate, with no knowledge of jig.
4. A global build cache, shared between checkouts and projects.
5. Distributed builds, on workers of several platforms.

## How jig uses ninja today

Knowing exactly what ninja does for jig now sets the minimum the engine has to do, and shows the workarounds it can
remove.

- **Writing the graph.** `src/ninja.rs` writes `build.ninja` with five rules (`configure`, `rustc`, `rustdoc`,
  `clippy`, `rustc-check`, plus `custom`) and `phony` aliases (`custom/<name>`, `test/<name>`, `check/<label>`,
  `lint/<name>`, `doc/<name>`, `lint`, `doc`). It uses explicit, implicit and order-only inputs, `default`,
  `deps = gcc` depfiles, `pool = console`, and `generator = 1`.
- **Running it.** `Configured::ninja` (`src/configured.rs`) runs `ninja` in the build directory. That happens in
  `main.rs` (`build`), `test_runner.rs`, `check.rs` (`--quiet check/<label>`), `run.rs`, `lint.rs` and `install.rs`.
- **Regenerating itself.** `Configured::reconfigure` runs `ninja build.ninja` and filters out the line
  `ninja: no work to do.`. The configure edge depends on `build.ninja.d`, which lists every file the configuration
  read, and on `reconfigure.stamp`. jig touches that stamp itself when a path the configuration recorded as missing
  comes into existence.
- **Asking whether something is up to date.** `install.rs` runs `ninja -n <target>` and searches the output for
  `ninja: no work to do.`.
- **Working around ninja's limits.**
  - Every command goes through `sh`, so arguments are shell-quoted and `$` is escaped. The toolchain is set with a
    shell prefix (`toolchain::shell_prefix`), and so are crate environment variables (`$crate-env`).
  - Paths or values containing a line break are refused (`Error::LineBreak`, `has_line_break`), because ninja has no
    escape for them.
  - `install.rs` doesn't run ninja when nothing is stale, because older ninja versions print "no work to do" even
    with `--quiet`.
- **Bootstrapping.** `bootstrap.ninja` builds `build/bootstrap/jig` with a single `rustc` command. CI installs ninja on
  every runner.

jig depends on nothing but `std`, and has no `Cargo.toml`. The engine keeps it that way: it has no dependencies either,
though it may use `unsafe` and `extern "C"` declarations of the system's C library where `std` has no API, kept in one
platform module (`rig/sys.rs`).

## Design principles

- **The graph is built through an API, not parsed from text.** jig hands the engine a graph directly. The engine saves
  it in its own binary format, as jig saves `.jig`, so that `jig build` can load it without evaluating the
  configuration again. rig reads no text format of its own, and no ninja files: a graph is only ever built through
  the API, or loaded from the binary format.
- **Actions are argument vectors, not shell strings.** An action has a program, arguments, environment variables, a
  working directory and optional redirections. Spawning programs directly means no `sh`, no quoting, no `$` escaping
  and no line-break limit. It also makes an action precise enough to hash for the cache and to send to a worker. A
  `Shell` action remains for commands that really need a shell.
- **Rebuild when what was recorded differs, not when an input is newer than an output.** For each edge, the engine
  records the stamps of its inputs when the edge runs. The edge is dirty when any current stamp differs from the
  recorded one. That treats a file whose timestamp goes backwards (a `git checkout`, a restored backup, clock skew) as
  changed, which ninja's "newer than" comparison doesn't. In timestamp mode the stamp is the file's metadata; in hash
  mode it is a hash of the contents.
- **Layered.** The local engine, the cache and distribution are separate layers on one interface: an action and the
  hashes of its inputs go in, and outputs come out. Each layer can be built, tested and turned on by itself.
- **Crash-safe without signal handlers.** State is written as an append-only log, each record checked when read, and a
  record is appended as soon as its edge finishes. An interrupted build loses only the edges that were running, so
  stopping a build doesn't depend on catching `SIGINT`.

## Architecture

```
            jig (configuration → graph)              other users of the crate
                       │                                       │
                       ▼                                       ▼
 ┌───────────────────────────── rig ───────────────────────────────┐
 │  graph    : nodes, edges, actions, pools; binary graph format    │
 │  state    : append-only log: stamps, action hashes, deps, times  │
 │  scan     : parallel stat / hash, dirty set, early cutoff        │
 │  schedule : critical-path priority, pools, jobserver             │
 │  execute  : local executor ─┬─ cache executor ─┬─ remote executor│
 │                             │  (local CAS,     │  (workers over  │
 │                             │   remote cache)  │   ssh / TCP)    │
 │  report   : events → status line / quiet / JSON                  │
 └──────────────────────────────────────────────────────────────────┘
```

### The crate

- It lives in its own directory, `rig/`, with `rig/lib.rs` as its root, and depends only on `std`.
- jig's `Jig` builds it as `crate rig 'rig/lib.rs'`, and the `jig` crate `use`s it, so it is a jig crate like any
  other.
- No jig types appear in its API: no properties, no diagnostics, nothing about rustc. jig turns crates and custom
  commands into edges.
- It is a library only, with no binary of its own.
- `rig/` also has a `Cargo.toml`, with no dependencies and `publish = false`, so that cargo projects can use it as a
  git or path dependency. It isn't published on crates.io, and cargo doesn't build jig itself.

API sketch:

```rust
let mut graph = rig::Graph::new();
let src = graph.file("src/lib.rs");
let out = graph.file("build/liba.rlib");
graph.edge(rig::Edge {
    action: rig::Action::exec("rustc").args([...]).env("RUSTUP_TOOLCHAIN", "stable").cwd(root),
    inputs: vec![src], implicit: vec![], order_only: vec![],
    outputs: vec![out], implicit_outputs: vec![],
    depfile: Some(rig::Depfile::gcc("build/liba.rlib.d")),
    pool: None, flags: rig::Flags::RESTAT | rig::Flags::CACHEABLE,
    ..Default::default()
});
graph.alias("lib/a", [out]);              // in place of `phony`
graph.default([out]);
graph.save("build/graph")?;              // jig generate

let graph = rig::Graph::load("build/graph")?;   // jig build
let report = rig::Engine::new(&graph, rig::Options { jobs, mode: rig::Mode::Hash, .. })
    .observer(&mut ui)
    .build(&["lib/a"])?;
```

`Engine::plan(targets)`, a dry run, returns the dirty edges. That replaces `ninja -n` and the search for
"no work to do" in `install.rs`.

### Graph and graph format

- Paths are interned as `u32` IDs, and edges and nodes are stored as flat arrays. On load, the strings borrow from the
  file's bytes, as `.jig` does, so loading is one read and a validation pass, with no allocation per path.
- The format starts with magic bytes and a version, and ends with a checksum. A version mismatch means "regenerate",
  as for `.jig` now.
- The engine supports the edge features jig uses: explicit, implicit and order-only inputs; implicit outputs; aliases
  (`phony`); defaults; gcc-style depfiles; pools, including an exclusive `console` pool; and generator edges.
  - **restat.** After an edge runs, the engine stats its outputs again. Dependents run only if an output changed.
  - **Missing-path inputs.** The input changes when a path that didn't exist comes into existence. That replaces
    `reconfigure.stamp` and the touch in `Configured::locate`.
- **Generator edges are built first.** Before building anything else, the engine brings the edge that generates the
  graph up to date. If that edge ran, the engine loads the graph again and starts over, as ninja does with
  `build.ninja`. When jig is the generator, it runs in the same process, so no second `jig` is spawned.

### State log

- The log is `build/.rig/log`. Each record has a length and a checksum, and a torn record at the end is ignored.
- One record per edge run holds:
  - the edge's key: its outputs;
  - the action hash: program, arguments, environment, working directory and depfile kind;
  - the stamp of each input, explicit or discovered;
  - the paths discovered from the depfile, which is then deleted, as `deps = gcc` does;
  - the stamps and content hashes of the outputs;
  - the start time and duration.
- The log is rewritten without superseded records when they make up more than half of it.
- A `std::fs::File::lock` on `build/.rig/lock` stops two builds from running in one build directory at once.

### Detecting changes

An edge is dirty if any of these holds:

- it has no record;
- its action hash differs from the record;
- one of its outputs is missing, or isn't what the edge last produced;
- an input's stamp differs from the recorded one;
- an input is produced by a dirty edge, unless early cutoff shows that the input didn't change.

What a stamp is depends on the mode:

- **`timestamp` (the default, and the only mode until phase 3).** The stamp is the mtime and the size, which come
  from the same `stat` call, so it's as cheap as ninja. It catches more changes, since a different size or an older
  mtime counts as a change.
- **`hash`.** The stamp is a BLAKE3 hash of the contents. The metadata stamp above is a cache key for the hash, so an
  unchanged file is never read twice. A file whose mtime falls in the same tick as when it was recorded is hashed
  again: the "racy git" problem.
  - Early cutoff then applies to every edge. If a step regenerates a file whose contents are the same, nothing
    downstream rebuilds.
  - The cache and distributed builds need this mode.
- The mode is chosen in `Jig` with a builtin property such as `set build/change-detection 'hash'`. It can be overridden
  per edge, for example to hash small generated sources while timestamping large binaries.

BLAKE3 is implemented in the crate, with a portable version first and `std::arch` SIMD versions later. It is tested
against the official test vectors. It's chosen over SHA-256 because, without SHA instructions, it is several times
faster and can hash large files in parallel.

Scanning is parallel. On a no-op build, most of the time goes to stat calls, so they're spread over the worker pool,
as hashing is. This is where the engine beats ninja's single thread on large graphs.

### Batched stat, and why there is no io_uring

The stamps a build needs are found together on a thread pool, by `rig::stamps_of`. io_uring's `IORING_OP_STATX` was
implemented and measured as an alternative in step 4, on 60,000 paths, a third of them missing, on 4 cores with Linux
6.18. Warm times are the best of 5 runs; cold times are a single run after dropping the page cache:

| | warm | cold |
|---|---|---|
| one `stat` at a time | 58 ms | 212–255 ms |
| thread pool (`rig::stamps_of`) | 19–24 ms | 79–108 ms |
| io_uring `IORING_OP_STATX` | 97 ms | 214–237 ms |

io_uring was slower than a single thread with a warm cache, and only matched one with a cold cache. The kernel can't
complete `statx` inline, so it hands each request to io_uring's worker threads. Since it gained nothing, and needed
raw system calls, shared memory rings and its own `unsafe` code, it was removed. It could be reconsidered for reading
files to hash them, which, unlike `statx`, can complete asynchronously, but only with a measurement that shows a gain.

### Environment variables

ninja tracks no environment variables. A command inherits the environment that ninja was run with, and changing a
variable rebuilds nothing. jig works around this in several places, and in others it simply can't track them:

- **The configuration.** `env()` is an error by default (`diagnostic/impure`), because the build isn't reconfigured
  when the variable changes. A project can only allow it, and then it is not tracked.
- **Searching `PATH`.** `which` and the program probes depend on the directories they search, but not on `PATH`
  itself. A program found again after `PATH` changes is found only because a directory changed.
- **rustc.** rustc writes `# env-dep:NAME=VALUE` lines into its dep-info for the variables that `env!` and
  `option_env!` read. Cargo rebuilds when those change; ninja ignores the lines.
- **Custom commands and tests.** They read whatever they like from the inherited environment (`CC`, `CFLAGS`, `LANG`,
  ...), and none of it is tracked.
- **Variables jig sets.** A crate's `env` attribute and the toolchain's `RUSTUP_TOOLCHAIN` are tracked only because
  they're written into the command string, so a change to the command rebuilds the edge.

In `rig`, an environment variable is an input of an edge, stamped with a hash of its value, or as unset. It takes part
in change detection like a file:

1. **Declared variables.** These are the variables an action sets: a crate's `env`, the toolchain, and `crate-env`.
   They're part of the action, so they're in the action hash. With argument-vector actions (phase 2), they're no
   longer spliced into the command through `sh`.
2. **Discovered variables.** The depfile parser reads rustc's `env-dep` lines, and records each variable as an input
   of the edge alongside the files it lists. A crate that uses `env!("FOO")` rebuilds when `FOO` changes, as it does
   with cargo.
   - The value stamped is the one rig's commands see: the build's `Options::env`, or else rig's own environment. That
     is not the value in the dep-info, which a command may have set itself. A crate's `env` is set in its command,
     which is already in the action hash; comparing against the dep-info's value would rebuild it every time.
3. **The configuration's variables.**
   - Evaluating the configuration records each variable that `env()` reads, set or not, and `PATH` whenever `which`
     searches it.
   - They are stored in `.jig` beside the missing paths, as names and hashes of their values.
   - `Configured::locate` compares them with the environment, and touches `reconfigure.stamp` when one has changed, as
     it does when a missing path appears. Both engines then regenerate the configuration.
   - `env()` is therefore no longer impure, and `diagnostic/impure` now covers only `run()`.
4. **Variables a command passes through.** `custom` and `test` get an attribute that declares which inherited
   variables the command reads, such as `pass-env 'CC' 'CFLAGS'`. Their values become inputs.
5. **Hermetic environments.** An option, required for cacheable and remote actions, runs an action with only its
   declared, discovered and passed-through variables, plus a fixed minimal set: `PATH`, `HOME` and `TMPDIR`.
   - Every variable that can affect the output is then in the cache key.
   - A worker runs the action with the environment it was sent, never its own.
   - An action that reads an undeclared variable fails the same way locally and remotely, instead of being silently
     non-reproducible.
   - `PATH` is a special case. Rather than hashing it, which would rebuild everything when an unrelated directory is
     added, the engine resolves each action's program when the graph is loaded. It records the resolved path, and in
     hash mode the program's contents. The program, not `PATH`, is the input.

Only a hash of each value is kept, in rig's state log and in `.jig`, never the value, which may be a secret. A dry run
(`jig build -n` with rig) names a variable that changed, such as `environment variable "FOO" has changed`, but not its
value. A variable is never sent to a cache or worker unless the action declares it.

### Scheduling and execution

- **Workers.** A `std::thread` worker pool runs `std::process::Command`. Each child gets its own process group, so the
  whole tree can be stopped when the build fails or is interrupted.
- **Priority.** Ready edges are ordered by the longest path from them to the end of the build, weighted by durations
  from the log. Edges with no history count as 1, so a deep chain of crates isn't started last.
- **Jobs.** The default number of jobs is ninja's: two more than `std::thread::available_parallelism` when there are
  more than two, or one more with two, or two with one, as commands spend time waiting on files and processes.
- **Jobserver.** The engine runs a GNU make jobserver, using the `fifo:` form on Unix, and passes it to children in
  `MAKEFLAGS` and `CARGO_MAKEFLAGS`. rustc then shares tokens for its codegen threads instead of oversubscribing the
  machine, and so do `make` and `cargo` run as custom commands. When `jig` itself runs under a jobserver, it joins that
  one instead.
- **Output.**
  - A program's stdout and stderr are captured together and printed in one piece when it finishes.
  - The `console` pool gets the terminal.
  - The status line works as ninja's does. `--quiet` prints only what programs print.
  - A JSON event stream is available for tools.
  - `jig check` reads events instead of filtering ninja's text.
- **Pipelining.** Cargo starts dependents as soon as rustc has written a crate's `.rmeta`. The engine lets an action
  report that an output is ready before the action finishes, from the `--json=artifacts` notifications on rustc's
  stderr, and starts edges that need only that output. jig would emit `--emit=metadata,link` and pass `.rmeta` to
  dependents that don't link. On deep crate graphs this is the largest gain over ninja on a full build.

### The global cache

The cache stores the results of actions under a key, so an edge that has already run anywhere with the same action
and inputs is fetched instead of run.

- **Store.** By default the cache is `$XDG_CACHE_HOME/jig`, or `~/.cache/jig`, and `JIG_CACHE` overrides it.
  - It is content-addressed: blobs are named by their BLAKE3 hash in sharded directories, and action results are
    stored separately.
  - Entries are written to a temporary file and renamed into place, so concurrent builds need no locks.
  - Outputs are restored by reflink (`ioctl(FICLONE)`) where the filesystem supports it, by hard link where it's
    safe (the output is read-only, which jig's are), and by copy otherwise.
- **Keys.** Dependencies discovered from a depfile are only known after the action runs, so lookup takes two steps,
  as in ccache's manifest mode:
  1. `k1 = H(action hash, toolchain identity, hashes of the declared inputs)` leads to a manifest, which lists the
     sets of discovered inputs that have been seen for `k1`.
  2. For each set, `k2 = H(k1, hashes of those inputs as they are now)` leads to a result: output blobs, captured
     output, and the depfile rewritten with paths relative to the root.
- **Toolchain identity.** This is a hash of `rustc -vV` (version, commit hash and host), the sysroot's manifest and the
  `RUSTUP_TOOLCHAIN` in the action. It's computed once per configuration and recorded in `.jig`.
- **Relocation.** A hit in another checkout needs outputs that don't contain the checkout's path.
  - jig passes rustc `--remap-path-prefix=<root>=.`, and the equivalent for the sysroot.
  - The cache key uses paths relative to the project root.
  - Depfiles are stored relative and rewritten on restore.
  - Actions whose arguments contain absolute paths outside the root and the sysroot aren't cacheable.
- **What is cached.** By default: rustc, rustdoc, clippy and check edges, whose inputs are complete because of their
  depfiles.
  - A `custom` command is cached only with a `cache` attribute, since jig can't know it is pure.
  - Generator, console and test edges are never cached.
  - Cached diagnostics are replayed, so `jig lint` and `jig check` show the same output on a hit.
- **Maintenance.** `jig cache stats`, `jig cache gc [--max-size]` (least recently used first, by a last-used time each
  hit updates) and `jig cache clear`. Blobs are verified against their hash when read, so a corrupt cache causes a
  miss and an eviction, not a bad build.
- **Remote cache.** The remote cache sits behind the same interface as the local store, and the local store sits in
  front of it. Since only `std` is available, which has no TLS, it has two transports:
  - **HTTP/1.1**, with `GET`/`PUT /cas/<hash>` and `/ac/<key>`, compatible with existing simple HTTP caches such as
    bazel-remote. It's for localhost, LAN, or behind a TLS proxy.
  - **`ssh host jig serve --stdio`**: jig's own framed protocol over a child `ssh`'s stdin and stdout. ssh handles
    authentication and encryption, and no new dependency is needed.
  - Pulling from the remote cache is open to all clients. Pushing is configurable and meant for trusted machines,
    such as CI, so that a developer's machine can't poison the cache.

### Distributed builds

Distribution reuses the cache's blob store and keys. A remote execution is a cache miss that runs on another machine.

- **Workers.** `jig worker` runs the same protocol as the cache over `ssh --stdio` or TCP.
  - On connecting, a worker reports its host triple, its rustup toolchains and their installed targets, its job
    count, and any tags.
  - Workers are listed in a user configuration file, `~/.config/jig/workers`, or in `JIG_WORKERS`, not in `Jig`, since
    they belong to a machine rather than a project.
- **Running an action remotely.**
  1. The client asks which input blobs the worker is missing, and uploads only those.
  2. The worker materializes the inputs in a fresh directory under its own root, using hard links from its store, and
     runs the action. Paths are relative and remapped, so the outputs match a local run.
  3. The worker hashes the outputs into its store and returns the result.
  4. The client downloads the outputs. A later version downloads only what is needed locally; see the phases.
- **Complete inputs.** An action must not read anything it doesn't declare.
  - For rustc, the declared inputs are the crate's sources from its last depfile, its `--extern` libraries, its link
    script and its target specification. The toolchain must match by identity, and the worker installs it with rustup
    if it is allowed to.
  - An edge with no depfile history yet, which would be its first build, runs locally. Alternatively, a cheap local
    `rustc --emit=dep-info` pass finds its inputs.
  - If an action fails remotely because a file was missing, it runs again locally, and the edge is marked local-only
    until its inputs change.
- **Multiple platforms.** rustc is a cross-compiler, so most edges can run on any worker that has the right toolchain
  and the target's standard library. Some edges care which platform runs them, though:
  - **Proc-macro crates** are built for the host they run on. Their edges are keyed by the worker's host triple, and a
    crate that uses one runs on a worker with that host, or the proc-macro is built for both.
  - **Linking** with a system linker (`cc`) depends on the platform. A linking edge declares the platform it needs.
    Edges that link with `rust-lld`, as jig's own cross builds do, can run on any platform.
  - **Tests and runners.** The integration with `jig test` sends each test of target `T` to a worker whose host is
    `T`. That's the same thing `test {target}.test` and runners do now, on the machine that can run it. It would let a
    developer run the aarch64 and riscv64 tests that CI's matrix runs, from one machine.
  - An edge's platform needs are part of its action: `any`, `host == <triple>`, or tags. The scheduler matches them
    against what workers report.
- **Scheduling.** Local job slots stay full. An edge goes to a worker when the estimated time to transfer its inputs
  and outputs is less than the time it would wait locally, using durations from the log. Remote job slots are counted
  separately from the local jobserver.
- **Security.** Workers run whatever commands they are sent, so they must only accept clients they trust: ssh keys,
  or for TCP, a shared token on a private network. Results are verified by hash when they arrive.

## Phases

Each phase can be merged and released by itself, and the test suite (`test/main.rs`, `test/perf.rs`) passes at the
end of each one.

### Phase 0: separate jig from ninja (done)

- `src/graph.rs` defines the graph that `jig generate` describes: typed `Rule`s (command, description, console, gcc
  depfile, generator), `Edge`s, aliases and defaults, behind a `Graph` trait.
  - `generate.rs` writes the graph through that trait. `ninja::NinjaFile` implements it, so `build.ninja` is
    byte-for-byte the same as before.
  - Retiring a previous build directory moved to `ninja::retire`.
- `src/builder.rs` defines the `Builder` trait:
  - `build(Request)`, where a request has targets, quiet output, keep-going, output to stdout or stderr, and arguments
    passed through for `jig build`;
  - `regenerate`;
  - `up_to_date(target)`;
  - `graph_up_to_date`.
- `ninja::Ninja` implements `Builder`. `Configured::builder()` returns it, and `Configured::build` turns a failed build
  into `Error::BuildFailed`.
- `jig build`, `test`, `check`, `run`, `lint`, `install` and `build --proxy` all go through the trait. Nothing outside
  `ninja.rs` runs ninja or reads its output.
- There are no behaviour changes.

### Phase 1: the local engine, with ninja's behaviour

#### How the graph is built

jig keeps describing the build exactly as it does now, through the `Graph` trait from phase 0. Phase 1 adds a second
implementation of that trait, so `generate.rs` doesn't change.

- **Two writers from one description.**
  - `RigGraph` implements `Graph` by building a `rig::Graph`, and saves it as `build/graph`.
  - `Tee` passes each call to both `NinjaFile` and `RigGraph`.
  - `jig generate` therefore writes `build.ninja` and `build/graph` from the same calls. Any difference between the
    two engines comes from the engine, not from the description.
- **Templates are expanded when the graph is generated, as ninja does.** Phase 0 rules are still ninja templates
  (`... $flags $in`), and the values of edges are ninja strings (`$$` for `$`). `rig::Graph` has no rules or variables.
  Instead, `rig::ninja::Manifest` evaluates each edge as ninja would:
  - The variables of edges are evaluated where they're defined.
  - A rule's `command`, `description`, `depfile` and `pool` are evaluated for each edge, looking variables up in the
    edge, then the rule, then the top level.
  - `$in` and `$out` are the explicit paths, escaped for the shell as ninja escapes them.

  Each edge becomes an `Action::Shell` command, still run by `sh`. `pool = console`, `deps = gcc`, `depfile`,
  `generator` and `restat` become fields of the edge, and `phony` edges become aliases.
- **`rig::Graph`.**
  - Paths are interned as `u32` IDs.
  - Edges and nodes are flat arrays, and each node records the edge that produces it.
  - An output with two producers is an error when it's added, as it is when ninja loads a file. Cycles are found by
    `validate`, as ninja finds them only when building.
  - The saved format is magic bytes, a version, the paths, the edges and the defaults, then a checksum. A version
    mismatch means "regenerate".
- **rig never parses ninja files.** `Manifest` takes rules, edges and variables as calls, not as text. rig's own
  tests build their graphs the same way, through a small test helper.
- **What stays as it is.**
  - The configure edge stays an ordinary generator edge, with its inputs in `build.ninja.d`.
  - Missing paths keep using `reconfigure.stamp`, which is just a file input to `rig`.
  - Line breaks are still an error, because `build.ninja` is still written.
  - Native inputs for missing paths and environment variables come with the executor.
- **Phase 2** makes a rule's command an `Action` with a program, arguments and environment variables. `RigGraph` then
  passes actions through unchanged, and `NinjaFile` takes on the quoting that `generate.rs` does now.

#### Steps

1. **Done:** the graph, its format, `Manifest`, `RigGraph` and `Tee`, `rig` as a crate that jig uses (and the
   bootstrap builds), and `rig/Cargo.toml` with a CI step that tests it with cargo.
2. **Done:** the state log, the timestamp scan and dry-run plans.
   - **State log.** `rig::state::State` opens `log` in a directory, and holds an exclusive lock on `lock` beside it
     while it's open.
     - Each record is checked against a hash when it's read. A torn record at the end is dropped and overwritten.
     - A log of another version is started again, and the log is rewritten once most of its records have been
       replaced.
     - Records are keyed by the edge's first output. They hold the stamps of its outputs, of its declared inputs (with
       the inputs of aliases in their place), and of the inputs it discovered, plus its action hash, start time and
       duration.
   - **Stamps.** `rig::Stamp` is the mtime in nanoseconds and the size, from one `stat`, or "missing". `Stamps` finds
     each file's stamp once per build.
   - **Plans.** `rig::plan::plan` returns the edges that must run, in dependency order, each with a `Reason`, which is
     what `--explain` will show. It covers:
     - order-only inputs;
     - generator edges, whose command may change without them running;
     - aliases, including always-dirty aliases with no inputs, and aliases taken as inputs;
     - discovered inputs, whose producers are built first, as ninja does with its deps log;
     - missing sources, and cycles;
     - defaults, or ninja's root nodes when there are none.

     A plan is a dry run: it replaces `ninja -n` once the executor records what it runs. Stamps are found one at a
     time for now; batching them is step 4.
   - Like ninja, a file changed twice within one tick of the filesystem's clock, to the same size, isn't noticed. A
     fix, if one is needed, is to stamp again any input whose mtime is in the same tick as the edge's start.
3. **Done:** the scheduler and executor, `rig::exec::build`.
   - **Running.** Edges run in plan order as soon as the edges they depend on, through aliases and discovered inputs,
     have finished. At most `jobs` run at once, and the default is the machine's parallelism.
     - Each command runs with `sh -c` in the build directory, on a thread of its own, with stdin closed.
     - Its stdout and stderr go to one pipe (`std::io::pipe`), and are given to the `Observer` together when it
       finishes.
   - **The console pool.** Its edges run one at a time with the terminal. The output of other edges that finish
     meanwhile is held until the console edge finishes, as ninja does.
   - **Recording.** An edge that succeeds is recorded at once, with its input stamps from when it started and the
     inputs its depfile lists.
     - The depfile is parsed by `rig::depfile`, in gcc's format, skipping rustc's `# env-dep:` comment lines. A missing
       depfile counts as none, and an invalid one fails the edge.
     - The depfile is kept rather than deleted.
     - A failed edge isn't recorded, so it runs again.
   - **Failures.** `keep_going` is ninja's `-k`: the build stops starting edges after that many failures, or never
     with `0`. Edges that depend on a failure don't run, and the `Report` counts them.
   - **Restat.** A restat edge's outputs are stamped before and after it runs. When none of an edge's planned
     dependencies changed their outputs, the edge is checked again with `plan::reason`, and skipped if it's otherwise
     up to date.
   - **Generator edges** run like any other. The caller builds the graph's own file first and reloads the graph if
     that ran, which step 5 does for jig.
   - **Output.** `exec::Console` prints what ninja prints on a terminal that isn't smart: `[finished/total]
     description` as each edge finishes, or as it starts for a console edge, then `FAILED: outputs` and the command
     for a failure, then the edge's output. `quiet` leaves out the status lines.
   - **Left for later.**
     - Children share rig's process group, so Ctrl-C reaches them. Process groups, with forwarding of signals, are
       step 4.
     - A smart terminal's single updating status line is not done yet.
     - Priority by critical path is phase 3; edges are started in plan order for now.
4. **Done:** `rig/sys.rs`, the jobserver and batched stat.
   - **Process groups and signals.** Each command except a console one leads its own process group.
     - While a build runs, `SIGINT`, `SIGTERM` and `SIGHUP` are caught, and the build forwards them to the groups of
       the running commands. It sends `SIGKILL` to any group still running a second later, since a shell's background
       jobs ignore `SIGINT`.
     - Console commands stay in rig's group, so they get the terminal's signals directly.
     - `Options::cancel` stops a build in the same way.
     - Edges stopped by an interrupt are `Outcome::Interrupted` and aren't counted as failures. `Report::interrupted`
       is set.
     - A real `SIGINT` was checked by hand: both commands stopped within 0.5 s, and no `sleep` was left running.
   - **Jobserver** (`rig::jobserver`). By default a build creates a GNU make jobserver: a FIFO in the temporary
     directory with a token for each job beyond the first.
     - Commands get it in `MAKEFLAGS` and `CARGO_MAKEFLAGS` as `--jobserver-auth=fifo:<path>`.
     - Each edge beyond the first running takes a token, so rustc's codegen threads and nested `make` or `cargo`
       share the build's jobs.
     - When rig is run by a FIFO jobserver, it joins that one instead. A jobserver given as file descriptors, by an
       older make, isn't joined.
     - `Options::jobserver` turns it off.
   - **Batched stat.** Before planning, a build collects every file it can reach (targets, the lists of the edges
     that produce them, and recorded discovered inputs) and stamps them together on a thread pool with
     `rig::stamps_of`. io_uring was measured as an alternative and removed; see
     [Batched stat](#batched-stat-and-why-there-is-no-io_uring).
   - The `unsafe` code is in `rig/sys.rs`: `signal`, `kill` and `mkfifo`.
5. **Done:** `JIG_ENGINE=rig`.
   - **Choosing the engine.** `Configured` reads `JIG_ENGINE` (`ninja` by default, or `rig`), and `builder()` returns
     `ninja::Ninja` or `rig_builder::Rig`. An unknown value is an error.
   - **`rig_builder::Rig`** implements the same `Builder` interface:
     - It loads `build/graph`, and keeps its state in `build/.rig`.
     - Each command gets the project's toolchain through `exec::Options::env`.
     - `regenerate` builds `build.ninja`, the generator edge, and jig reads the database again after it, as with
       ninja.
     - `up_to_date` and `graph_up_to_date` are plans that run nothing.
   - **`jig build`'s arguments.** Under rig it takes a subset of ninja's options: `-j`, `-k`, `-n` (a dry run that
     prints each edge with the reason it would run), `-v` (commands rather than descriptions) and `--quiet`. Any
     other option is an error that names it.
   - **Errors and output.** rig doesn't copy ninja's log messages. An unknown target, a missing input or a cycle is
     printed on stderr as the error alone, such as `unknown target "x"`, and the build fails. A build with nothing to
     do prints nothing.
   - **The generator edge is recorded by `jig generate`.** It records the edge in rig's state directly, so the first
     build doesn't regenerate the graph as it would with no record. That is skipped while rig holds the state, as when
     rig itself runs the configure edge, which it records afterwards.
   - **Output directories.** rig now creates the directories of an edge's outputs and depfile before running it, as
     ninja does. Building jig itself showed it was needed.
   - **`configured::ensure`** regenerates when `graph` is missing as well as `build.ninja`.
   - **The suite runs with both engines.** The integration tests no longer rely on ninja's "no work to do" line:
     they check that nothing ran, as no `CLIPPY` line is shown. Under rig they look in rig's log instead of using
     `ninja -t deps`. With `JIG_ENGINE=rig`, all 160 integration tests
     and the unit and perf tests pass, and CI runs the suite a second time with it.
6. **Done:** differential tests, `src/differential.rs`, in jig's unit tests.
   - **What a seed does.** It generates a random graph and writes it through jig's `Graph` trait, as `build.ninja` in
     one directory and rig's graph in another, with the same sources.
     - The graph can have explicit, implicit and order-only inputs, implicit outputs, output subdirectories, and
       aliases.
     - It can have edges that discover headers through a gcc depfile, and restat edges that leave their output alone
       when it wouldn't change.
   - **What it checks.** The seed builds with ninja (`-j4 -k0`) and with rig, then makes 12 random edits, building
     with both after each one.
     - The edits change a source, touch one without changing it, delete an output, or change nothing. Some builds ask
       for one target rather than the defaults.
     - After each build, both engines must have run the same edges, with the same outcome, and every output must have
       the same contents.
   - **Running it.** 16 seeds run by default, 8 at a time. `JIG_DIFFERENTIAL_SEEDS` sets how many, and
     `JIG_DIFFERENTIAL_SEED` runs one, showing each step, what each engine ran, and ninja's output. The test is
     skipped where ninja isn't installed.
   - **It found a bug in rig's restat handling, now fixed with a regression test.** An edge waiting on a restat edge
     that left its output alone was only checked again if every edge it waited on had left its outputs alone. It
     also counted an edge it only depended on through an order-only input, which can't make it run. It is now checked
     again whenever one of them left its outputs alone.
   - **It found a ninja behaviour that rig doesn't copy.** A restat edge with an alias as an input, which left its
     output alone, is run again by ninja on the next build, though nothing it depends on changed. ninja records the
     newest of the edge's inputs, not counting the alias's own inputs. rig runs nothing.
     - The generator doesn't give restat edges alias inputs, so the comparison stays exact.
     - The other cases left out are those where rig rebuilds on purpose and ninja doesn't: a source whose time goes
       back, or an output changed by hand.
   - **Checked.** 400 seeds pass, three runs in a row. Breaking rig on purpose, so that it ignored implicit inputs,
     made the first three default seeds fail.
7. **Done:** environment variables as inputs. See [Environment variables](#environment-variables) for the design.
   - **rig.**
     - `depfile::parse` returns the names on rustc's `# env-dep:` lines beside the inputs.
     - Each record in the state log keeps a hash of each such variable's value, or that it was unset. The log format
       is now version 2.
     - `Stamps::env` stamps a variable as commands see it, through `Stamps::with_env`.
     - A change is `Reason::EnvChanged`.
     - `rig::fnv1a` is public, so that jig hashes values the same way.
   - **jig.**
     - `env()` and `which`'s search of `PATH` record the variable in the project.
     - `.jig` (database version 13) stores the names and hashes, and `Configured::locate` touches the reconfigure
       stamp when one differs, for either engine.
     - `env()` is no longer impure.
   - **Tests.**
     - A unit test in rig rebuilds when a variable listed in a depfile is set, changed or unset.
     - Integration tests regenerate the configuration when a variable `env()` read changes, is unset, or when `PATH`
       changes.
     - Under rig, `jig run` rebuilds a crate whose `env!` variable changed, and nothing else. It fails if rig's
       variable check is disabled.
     - `impure_functions` now uses `run()`, the remaining impure function.
8. **Done:** performance tests comparing rig with ninja, in `test/perf.rs`.
   - **The graph.** `copies(n)` is `n` custom commands, each copying a file of its own into `out/` in the build
     directory, with an order-only link from every tenth to the one before. Its time is that of the engine and of
     spawning processes, not of compilers. Each engine builds its own copy of the project, so neither sees the other's
     outputs or state.
   - **`rig_builds_as_fast_as_ninja`** times a full build and an up-to-date build of 1,000 copies with each engine,
     best of 3. It fails if rig takes more than 1.3 times as long as ninja for either, retrying once first. The test
     is skipped where ninja isn't installed.
   - **`rig_up_to_date_scales`** fails if an up-to-date rig build of 4,000 copies takes more than 10 times as long as
     one of 1,000.
   - **Measured** on 4 cores with Linux 6.18, best of 3:

     | | ninja | rig |
     |---|---|---|
     | full build, 1,000 copies | 0.89–1.08 s | 0.97–1.07 s |
     | full build, 4,000 copies | 3.8–4.2 s | 4.0–4.1 s |
     | up to date, 1,000 copies | 20–22 ms | 12–13 ms |
     | up to date, 4,000 copies | 59–64 ms | 42–50 ms |

     Up-to-date builds are about 1.3 to 1.7 times faster than ninja's, as files are stamped together on threads.
     Full builds are within noise of ninja's, since spawning a process for each edge dominates them. The larger gains
     on full builds are pipelining and priority by critical path, which are later phases.
   - **What the measurements fixed.** Full builds were at first about 1.4 times slower than ninja's:
     - **Fewer jobs.** rig ran as many jobs as cores, while ninja runs two more. rig now defaults to the same number.
     - **A lost job slot.** An edge which started while nothing else ran held the build's job without a token. When
       it finished, the next edge still waited for a token. With 4 jobs, rig mostly ran 3 at once. It now tracks
       which edge holds that job, and a regression test checks it is reused.
     - **A thread for each edge.** That cost about a dozen more system calls per edge than ninja. Edges now run on a
       thread for each job, and each output directory is created once. A full build of 2,000 copies made 459,000
       system calls against ninja's 416,000; it now makes 430,000.

### Phase 2: make the engine the default

#### Steps

1. **Done:** rig is the default, and nothing needs ninja.
   - **The default engine.** `JIG_ENGINE` defaults to `rig`, and `ninja` chooses ninja. jig still writes
     `build.ninja` beside `build/graph`, so ninja can also be run in the build directory on its own.
   - **Bootstrapping without ninja.** `bootstrap.sh` builds `librig.rlib` and then `jig` with rustc alone, into
     `build/bootstrap`, and configures the project. `bootstrap.ninja` does the same with ninja, and is kept.
   - **ninja is optional.** The README lists it as optional, needed only for the ninja engine and for the tests that
     compare the two.
     - The tests that run ninja on `build.ninja` directly, the differential test, and the performance comparison skip
       themselves where ninja isn't installed.
     - One proc-macro test now builds with `jig build` rather than ninja.
   - **CI.**
     - The test matrix and the lint job bootstrap with `bootstrap.sh` and don't install ninja.
     - The test matrix runs every test with rig.
     - A new `ninja` job installs ninja, bootstraps with `bootstrap.ninja`, and runs every test with
       `JIG_ENGINE=ninja`. That includes the differential and comparison tests.
   - **Checked.** Every test passes with no working ninja on `PATH`, and with `JIG_ENGINE=ninja` after bootstrapping
     with ninja.
2. **Done, apart from line breaks:** argument vectors.
   - **`rig::Action::Exec`.** It holds a program, its arguments, environment variables, an optional working directory
     relative to the build directory, and an optional file for stderr. rig runs it directly, without a shell.
     - Its action hash covers every part, each with its length before it.
     - The graph format, now version 2, stores it.
     - A failure, or `-v`, shows it as a quoted command line.
   - **jig gives every edge a program.** That covers configure, rustc and test harnesses, rustdoc, clippy and checks
     (whose stderr goes to the diagnostics file), and custom commands.
     - jig's `Graph` trait carries the program beside the ninja template, as `Edge::exec`. `RigGraph` passes it to
       `Manifest` as the edge's action.
     - `NinjaFile` writes the templates as before, so `build.ninja` is byte for byte the same, and ninja still runs
       through `sh`.
   - **One description, both forms.** rustc's flags are built once, as `Flags`, which holds both the arguments and the
     text `build.ninja` had, quoted for `sh` and with `$` escaped for ninja.
     - The toolchain and a crate's `env` are the program's environment.
     - Lint flags such as `-W clippy::pedantic` are split into their two arguments.
   - **Measured.** A full build of 1,000 copies now takes rig 0.94–1.13 s, against ninja's 1.27–1.50 s. rig starts one
     process for each edge, where ninja starts two: `sh`, then the program.
   - **A graph of another version is configured again.** `.jig`'s version now changes whenever the graph format does,
     so `configured::ensure` regenerates the project, rather than rig failing to load the old graph.
   - **Left for step 3: line breaks.** Lifting the restriction on line breaks waits for step 3. rig's configure edge
     still has `build.ninja` as its output, so `build.ninja` can't yet be left out for a graph which ninja can't
     express.
3. **Configuring natively.** Remove the missing-path stamp in favour of missing-path inputs, and run the configure
   edge in the same process.

### Phase 3: hashing and early cutoff

- Add BLAKE3, the stamp cache in hash mode, and the racy-timestamp rule.
- Add the `build/change-detection` property, and its override per edge.
- Apply early cutoff to every edge in hash mode.
- Use critical-path priority from recorded durations.
- Add the jobserver.

### Phase 4: the local global cache

- Add the content-addressed store, two-step keys and toolchain identity.
- Remap paths in jig's rustc, rustdoc and clippy commands, with tests that a second checkout in another directory gets
  every edge from the cache.
- Add the `cache` attribute for `custom`, and replay cached diagnostics.
- Add the `pass-env` attribute and hermetic environments for cacheable actions.
- Add `jig cache stats|gc|clear`.

### Phase 5: the remote cache

- Add `jig serve`, with the framed protocol over `--stdio` and TCP, and the HTTP/1.1 client.
- Add policies for pulling and pushing, so that CI fills the cache and developers read from it.

### Phase 6: distributed execution

- Add `jig worker`, capability reports, uploading only missing blobs, and running actions in sandboxes.
- Add platform constraints on edges, the proc-macro and link rules, and the fallback to a local run.
- Run tests remotely on workers whose host is the test's target.

### Phase 7: later

- **Pipelining.** Start dependents from `.rmeta`, using early outputs.
- **Building without the bytes.** Leave intermediate outputs on workers, and download only defaults, installed files
  and what tests need.
- **A resident daemon.** It would watch the filesystem, so a no-op build doesn't stat every file.
- **Windows.** Spawning programs, process groups and the jobserver are each kept in one platform module from phase 1,
  so Windows can be added later.

## Risks

- **No dependencies.** Everything is written in-house: BLAKE3, a framed wire protocol, an HTTP/1.1 client, the
  jobserver, the depfile parser, and the system calls (`signal`, `kill`, `mkfifo`, `ioctl`). That's a deliberate
  cost for having no dependencies. The `unsafe` code is confined to `rig/sys.rs`, with a safe interface and tests of
  its own. TLS is avoided by relying on ssh or a proxy instead of implementing it.
- **Matching ninja's behaviour.** jig's users can run ninja in the build directory directly. Keeping `build.ninja`,
  and the differential tests, limits what breaks.
- **Cache correctness depends on complete inputs.** Only rustc and rustdoc, whose depfiles are complete, are cached by
  default. Toolchain identity and remapped paths are tested by comparing a cached result with a fresh run.
- **The first build of an edge can't be distributed** until its inputs are known. That makes distribution most useful
  for CI and for rebuilds after a pull, which is where it pays off most anyway.

## Decisions

1. **Name.** The crate is called `rig`. It has no binary of its own.
2. **`unsafe` and `extern "C"`.** Both are allowed, in `rig/sys.rs`: `mkfifo` for the jobserver, `ioctl(FICLONE)` for
   reflinks, and `signal` and `kill` for process groups. io_uring was tried and removed, as it gained nothing and
   wasn't worth maintaining; see [Batched stat](#batched-stat-and-why-there-is-no-io_uring).
3. **Change detection.** Timestamps, with the mtime and the size from the same `stat` call. Hash mode stays in the
   plan, as an option for phase 3 and the cache, but isn't the default.
4. **ninja.** Generating `build.ninja` remains supported, permanently, as a second engine.
5. **Packaging.** `rig/` has a `Cargo.toml`, so cargo projects can use it from git or a local path. It isn't
   published on crates.io.
