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
  - Outputs are restored by reflink (`ioctl(FICLONE)`) where the filesystem supports it, and by copy otherwise. Hard
    links aren't used, as jig's outputs aren't read-only, so a command that wrote to one in place would change the
    cache's blob.
- **Keys.** Dependencies discovered from a depfile are only known after the action runs, so lookup takes two steps,
  as in ccache's manifest mode:
  1. `k1 = H(action hash, toolchain identity, hashes of the declared inputs)` leads to a manifest, which lists the
     sets of discovered inputs that have been seen for `k1`.
  2. For each set, `k2 = H(k1, hashes of those inputs as they are now)` leads to a result: output blobs, captured
     output, and the depfile rewritten with paths relative to the root.
- **Toolchain identity.** This is a hash of `rustc -vV` (version, commit hash and host) and the channel's manifest which
  rustup writes to the sysroot, or without it, of `bin/rustc`, by its path within the sysroot, so that a toolchain has
  the same identity wherever it is installed, and whichever components it has, as a worker finds it. A
  `RUSTUP_TOOLCHAIN` given by `+<toolchain>` is in the action. It's computed once per configuration and given to each
  cached edge in the graph.
- **Relocation.** A hit in another checkout needs outputs that don't contain the checkout's path.
  - Every path jig gives rustc is relative to the build directory, and rustc's paths are remapped to be relative to
    the project root. The sysroot's sources are already remapped by rustup's builds of the standard library.
  - The cache key uses paths relative to the build directory, and the build directory, where an action names it, is
    replaced by a marker.
  - Depfiles are relative already, so they are restored as they are.
  - Actions whose arguments contain other absolute paths aren't shared between checkouts, as their keys differ.
- **What is cached.** By default: rustc, rustdoc, clippy and check edges, whose inputs are complete because of their
  depfiles.
  - A `custom` command is cached only with a `cache` attribute, since jig can't know it is pure.
  - Generator, console and test edges are never cached.
  - Cached diagnostics are replayed, so `jig lint` and `jig check` show the same output on a hit.
- **Maintenance.** `jig cache stats`, `jig cache gc [--max-size]` (least recently used first, by a last-used time each
  hit updates) and `jig cache clear`. Blobs are verified against their hash when read, so a corrupt cache causes a
  miss and an eviction, not a bad build.
- **Remote cache.** The remote cache sits behind the same interface as the local store, and the local store sits in
  front of it. Since only `std` is available, which has no TLS, it has three transports:
  - **HTTP/1.1**, with `GET`, `HEAD` and `PUT` of the files of a cache's directory, `/cas/<ab>/<cdef...>`,
    `/ac/<key>` and `/mf/<key>`, as any server of files with `PUT` serves them, such as nginx with WebDAV. It's for
    localhost, LAN, or behind a TLS proxy. bazel-remote's HTTP API doesn't fit, as it checks each blob against its
    SHA-256 and each action result against Bazel's format, while rig's blobs are named by BLAKE3 and its results are
    its own.
  - **TCP**, `jig serve --listen`, with jig's own protocol and a shared token, for private networks.
  - **`ssh host jig serve --stdio`**: jig's own framed protocol over a child `ssh`'s stdin and stdout. ssh handles
    authentication and encryption, and no new dependency is needed.
  - Pulling from the remote cache is open to all clients. Pushing is configurable and meant for trusted machines,
    such as CI, so that a developer's machine can't poison the cache.

### Distributed builds

Distribution reuses the cache's blob store and keys. A remote execution is a cache miss that runs on another machine.

- **Workers.** `jig worker` runs the same protocol as the cache over `ssh --stdio` or TCP.
  - On connecting, a worker reports its host triple, its rustup toolchains and their installed targets, its job
    count, and any tags.
  - Workers are listed in the user's configuration, `~/.config/jig/config`, as the remote cache is, or in
    `JIG_WORKERS`, rather than in a project's `Jig`, since they belong to a machine rather than a project.
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
3. **Deferred:** configuring natively, and line breaks.
   - **What it would do.**
     - Replace `reconfigure.stamp` with inputs of rig's generator edge for missing paths.
     - Run the configure edge in jig's own process rather than as a `jig generate` subprocess.
     - Write `build.ninja` only when ninja can express the graph, so that paths with line breaks work under rig.
   - **Why it is deferred.** What it would replace already works for both engines:
     - the stamp, for missing paths and for the configuration's environment variables;
     - the configure subprocess, which runs only when the configuration changes;
     - the line-break check, which rejects what ninja can't write, and is rarely met.

     Doing it natively saves one process when the configuration changes, and allows line breaks in paths. That is
     little for a rework of how both engines regenerate, since rig's generator edge would have to produce `graph`
     rather than `build.ninja`.
   - **When to revisit.** When something else needs rig's generator edge reworked, such as a resident daemon, or
     dropping the ninja engine.

### Phase 3: hashing and early cutoff

1. **Done:** BLAKE3, hashed stamps and early cutoff.
   - **`rig::blake3`.** A portable implementation from the spec: chunks of 1,024 bytes compressed a block at a time,
     merged in a tree by a stack of chaining values.
     - It is checked against 21 of the official test vectors from BLAKE3's `test_vectors.json`, at lengths across the
       block, chunk and tree boundaries up to 100 KiB.
     - Hashing in parts must match hashing at once.
     - It runs at about 470 MB/s on one core; SIMD can come later if hashing ever shows in a profile.
   - **`Stamp::Content`.** It is the mtime, size and hash, and two content stamps are the same when their hashes are,
     by `Stamp::same`.
     - Edges have a `hashed` flag (graph format version 3), and the state log stores content stamps (version 3).
     - A hashed edge's inputs, outputs and discovered inputs are stamped by their contents.
   - **The hash is reused, not read again,** while a file's time and size are those recorded. The racy rule applies
     only to sources, which no edge produces: one modified within a tick of being stamped is hashed again, as it may
     have been edited again within the tick. A tick is a second where the time has no fraction of one, or 10 ms
     otherwise.
     - Generated files aren't subject to the rule, as they are written moments before they are stamped, so that
       every output would otherwise be read again on every no-op build.
   - **Early cutoff.** A hashed edge's outputs are stamped before and after it runs, as a restat edge's are, and an
     edge which would run only because they were rebuilt is checked again, and skipped if they are unchanged.
     - A hashed edge is checked again whatever produced its inputs, since it compares their contents itself.
   - **jig.**
     - The builtin `change-detection` property, `'timestamp'` by default or `'hash'`, applies to every edge.
     - A custom command's `change-detection` attribute replaces it, such as to hash the small file a generator
       writes.
     - It has no effect with ninja. `.jig`'s version changed with the graph's.
   - **Tests.**
     - Stamps reuse hashes, except for a racy source.
     - Under a hashed edge, an unchanged rewrite and a change undone don't rebuild, and an output written unchanged
       isn't modified.
     - A generator which rewrites its output unchanged doesn't rebuild what follows. That test fails without hashing.
     - An integration test checks the property, the attribute and their invalid values.
2. **Done:** priority by critical path.
   - **The priority.** Each planned edge's priority is how long it took when it last ran, from the state log, or 1 ms
     if it hasn't run, plus the largest priority of the edges waiting for it. That is the time of the longest path from
     it to the end of the build. The plan's order is topological, so one pass from its end finds them all.
   - **The ready queue.** Ready edges start highest priority first, then in plan order, so a long chain isn't left
     until last.
   - **Tested.** With one job, a longer chain starts first before anything has run, and of two independent edges, the
     one which took longer last time starts first. The test fails with every priority the same.
3. **The jobserver** was done in phase 1, step 4.

### Phase 4: the local global cache

#### Steps

1. **Done:** the content-addressed store, two-step keys and toolchain identity.
   - **`rig::cache`.** A `Cache` is a directory of three stores, each file written to `tmp` and renamed into place, so
     builds sharing it need no lock.
     - `cas` holds the blobs, named by their BLAKE3 hash in directories of its first two hexadecimal digits: outputs,
       depfiles, and what commands printed.
     - `mf` holds the manifests, by first key.
     - `ac` holds the results, by second key. A result lists each output's path, blob and whether it is executable,
       and the blob of what was printed.
     - A manifest or result ends with the BLAKE3 hash of what precedes it. One that doesn't match is a miss, and is
       removed.
   - **Keys.**
     - The first is of the edge's cache identity, its action, the variables every command is given
       (`Options::env`), its outputs and depfile, and the hashes of its declared inputs.
     - The second is of the first, and of the hashes of one of the manifest's sets of discovered inputs and
       variables, as they are now.
     - Each part has its length before it, and a missing file or unset variable is marked as such.
     - A manifest keeps the latest 16 sets.
   - **Restoring.**
     - A blob is copied, by `FICLONE` where the file system can (in `rig/sys.rs`), to a file beside the output, which is
       checked against its hash, then renamed over the output.
     - A blob that doesn't match is removed, every blob of that result is checked, and the edge runs instead.
   - **In the executor.**
     - A cached edge is looked up when it starts, once its inputs are stamped.
     - A hit is restored on the job's thread in place of running the command. Its depfile is restored with its outputs,
       so it is recorded as if it had run, keeping the duration of its last run for the critical path.
     - A miss which succeeds adds its outputs, depfile and printed output to the cache, unless an output is missing.
     - `Report::cached` counts the hits.
     - A cache which can't be read or written is a miss, never a failure.
   - **Cached edges are hashed.** An edge with a cache identity (`Edge::cache`, graph format version 4) is stamped by
     its contents, as the keys need the hashes.
   - **jig.**
     - When `JIG_CACHE` is set while configuring, the rustc, check and clippy edges are cached, identified by the
       toolchain's identity. rustdoc isn't, as it writes a directory rather than the files its edge lists.
     - The identity is a hash of `rustc -vV`, the toolchain's name, and rustup's
       `lib/rustlib/multirust-channel-manifest.toml` and `lib/rustlib/components` in the sysroot (or, without rustup,
       `bin/rustc`). The configuration depends on those files, so that updating a toolchain in place configures the
       project again and identifies the toolchain afresh.
     - `JIG_CACHE` is read as one of the configuration's variables, so setting or unsetting it configures the project
       again. A relative path is relative to the working directory of the build.
   - **Opt-in at first.** The cache was used only with `JIG_CACHE` until `jig cache gc` could bound its size, in
     step 5.
   - **Measured.** In one checkout, `librig.rlib` takes 4.3 s to compile.
     - Undoing an edit fetches it in 0.05 s.
     - After the build directory is removed, configuring and fetching it takes 0.16 s.
     - A second checkout missed, as rustc was given the absolute paths of sources, until step 2.
   - **Not done yet.**
     - The linker isn't part of the identity, though linked edges are cached.
     - Hits don't update a last-used time, which `gc` will need.
     - Blobs are stored on the build's own thread, copying and hashing each output again, rather than on a job's
       thread. That should be measured on a large build.
   - **Tests.**
     - The store's blobs, manifests and results, with corruption.
     - Keys differing by each part.
     - A second build directory fetching outputs, depfile and printed output, and recording them.
     - Discovered and declared inputs changing and changing back.
     - Another identity or environment missing.
     - A corrupted cache running the edge and being filled again.
     - Executable outputs.
     - An integration test, in which a crate whose outputs are removed is fetched, with its warning, with no `rustc` on
       `PATH`, and unsetting `JIG_CACHE` configures again.
2. **Done:** relative and remapped paths, so that builds are reproducible.
   - **Every path is relative to the build directory.** `Rebase::path` gives a path within the build directory
     relative to it, as before, and any other leading out of it with `..` rather than as an absolute path.
     - The `..` start from the build directory once symbolic links are followed, as the system resolves them, so they
       lead to the same files when the build directory is a link.
     - Paths outside the project are relative too, so that nothing rustc is given depends on where the project is.
     - It applies to every node, as rustc's dep-info names the paths it was given, which must be the nodes the edges
       declare, such as the outputs of custom commands which crates read.
   - **What rustc writes is remapped.** rustc, rustdoc and clippy are given `--remap-path-prefix` three times, in this
     order, as the last that matches applies:
     1. `<build dir>=.`, as the working directory of debug info, which is the project root.
     2. `<build dir to root>=`, usually `..=`, so that `../src/main.rs` is written as `src/main.rs`.
     3. For a root module in the build directory, as a generated one, its first component as itself under the build
        directory, such as `gen.rs=build/gen.rs`. Its modules share it.
   - **What it covers.** Diagnostics, panics, `file!()` and debug info, of which the compilation directory is `.` and
     the file names are relative to the project root, as cargo's are.
     - rustc's dep-info isn't remapped, so ninja and rig find what it lists.
     - The JSON diagnostics `jig check` replays are relative to the project root, which rust-analyzer resolves them
       against.
   - **The build directory in cache keys.** The remapping names the absolute build directory, so rig's first key
     takes the action with the build's directory (`Options::dir`, made absolute) replaced by a marker,
     by `plan::relocated_action_bytes`. The build's own state still keeps the action's hash as it is.
   - **Tests.**
     - rig: a cached program given its build's directory is fetched by a build in another directory. Without the
       replacement it is not.
     - A crate and a binary using a generated crate, with debug info, built in two directories:
       - They are the same bytes, and neither has its directory in it.
       - Diagnostics and panics name `app.rs` and `build/gen.rs`.
     - A second checkout configured with the same `JIG_CACHE` fetches every edge, with no `rustc` on `PATH`.
     - The tests of `build.ninja` expect paths relative to the build directory. A helper leaves out the remapping
       flags, which `rebased_paths` checks.
3. **Done:** the `cache` attribute for `custom`, and replaying diagnostics.
   - **`cache`.** A custom command with `cache` (or `cache true`) is cached when `JIG_CACHE` is set, as rustc's edges
     are. Tests are never cached, so the attribute is an error on one.
   - **Its identity.**
     - It is a hash of the toolchain's identity, which the command is run with.
     - A program without a `/` is found in `PATH` while configuring, and its contents are hashed in too. The
       configuration depends on it, as on the directories `which` searches and on `PATH`, so a program which changes
       configures the project again.
     - A program which is a path is already an input of the edge.
   - **The identity is part of the action hash.** rig's `plan::action_hash` now includes an edge's cache identity, so
     an edge runs again when it changes, as when its program or toolchain changes, rather than only missing in the
     cache. Without that, a program which changed would reconfigure the project and then build nothing.
   - **Relocatable.** A custom command's working directory, the project root, is given to rig as the path from the
     build directory (`..`) rather than an absolute path, so its action, and its key, are the same in another checkout.
     Its arguments are already relative to the project root.
   - **Diagnostics were already replayed.**
     - What rustc and clippy print is restored with their outputs.
     - The diagnostics files of checks and lints are implicit outputs, which are restored too.
   - **Tests.**
     - rig: an edge whose identity changes is planned to run, as its command has changed.
     - An integration test of a cached command and one without `cache`:
       - Once their outputs are removed, only the one without `cache` runs.
       - A second checkout, whose program is a copy in another directory, fetches it.
       - Changing the program configures the project again and runs it.
       - `cache` on a test is an error.
4. **Done:** the `pass-env` attribute and hermetic environments for cached actions.
   - **`rig::Edge::pass_env`.** These are the variables an action reads from those it inherits, stored in the graph
     (format version 5).
     - They are inputs: recorded with the variables discovered from the depfile, so a value that changes is
       `EnvChanged`.
     - Their names are part of the action hash, so passing on another variable is `CommandChanged`.
     - The hashes of their values are part of the first cache key.
   - **Hermetic.** A cached edge (`EdgeRef::hermetic`) runs with its environment cleared, then given:
     - the variables in `Options::locating`, which only locate tools and files, and so aren't inputs. They are `PATH`,
       `HOME` and `TMPDIR` by default, and jig adds `RUSTUP_HOME` and `CARGO_HOME` for rustup's proxies;
     - those it passes on;
     - `Options::env`, its own variables, and the jobserver's `MAKEFLAGS`.

     An edge that isn't cached inherits everything, as before.
   - **Discovered variables aren't passed on.** A variable rustc lists as read by `env!` isn't known before the crate
     first compiles, so a cached crate must declare it, with `env` or `pass-env`. Otherwise it fails to compile rather
     than being cached without it.
   - **`PATH`** isn't an input. The programs found in it are identified instead: rustc and clippy by the toolchain,
     and a cached custom command's program by its contents (step 3).
   - **jig.** `pass-env`, a name or a list of them, is an attribute of `custom` and of `crate`. A crate's applies to its
     rustc, check, clippy and rustdoc edges, and it can be read as `<crate>.pass-env`. ninja passes on every variable
     and tracks none.
   - **Tests are left for phase 6.** They aren't edges and aren't cached, so `pass-env` on a `test` waits for running
     tests on workers, where it will make them hermetic.
   - **Checked.** jig builds itself and runs rig's tests hermetically, with `JIG_CACHE` set.
   - **Tests.**
     - rig, hermeticity: a cached edge sees only the locating and passed variables, and those the build gives. One
       that isn't cached sees every variable.
     - rig, change detection: a passed value that changes runs the edge again and misses in the cache, and changing it
       back is a hit. Passing on another variable changes the command.
     - An integration test with a custom command and a crate's `env!`:
       - The command runs again and the crate compiles again when a passed value changes.
       - Cached, the command doesn't see a variable it doesn't pass on.
       - An invalid name is an error.
5. **Done:** `jig cache stats|gc|clear`, and the cache on by default.
   - **Last use.** A hit touches its result and its first key's manifest (`Cache::used`), so their modification times
     are when they were last used. A result or manifest which is added is new anyway.
   - **`Cache::gc(max)`.**
     - Results are removed, used longest ago first, until the cache's files total no more than `max` bytes.
     - Each blob is removed once no result which is kept refers to it.
     - Manifests are removed if not used since the latest result removed was.
     - A blob which no result refers to, and a file in `tmp`, is removed once it is an hour old. Until then a build
       may be adding the result which refers to it, or writing it.
     - A corrupted result is removed.
     - A build which adds a result while blobs are collected at worst misses later, never restores a wrong output, as
       a result whose blobs are missing is a miss.
   - **`Cache::gc_if_due`.** It collects the cache if the `collected` file in it is at least a given age, marking it
     first so that builds which finish at once don't each collect. A new cache is only marked.
   - **jig.**
     - `src/cache.rs` finds the cache: `JIG_CACHE`, or `$XDG_CACHE_HOME/jig`, or `$HOME/.cache/jig`. A `JIG_CACHE`
       which is empty or `off`, or neither of the others, turns it off.
     - Configuring reads only the variables which decide whether it is on, as configuration variables, so turning it
       on or off configures again.
     - `jig cache stats` shows the directory, and the number of results, manifests and blobs and their size.
     - `jig cache gc [--max-size <size>]` defaults to `JIG_CACHE_SIZE`, or 10 GiB. Sizes take binary multiples: `512`,
       `100K`, `1.5G`, `10GiB`.
     - `jig cache clear` removes everything.
     - After every rig build, the cache is collected down to `JIG_CACHE_SIZE` once a day.
   - **On by default.** Every rustc, check and clippy edge is therefore cached and hermetic by default. A crate whose
     `env!` reads a variable from the environment must pass it on with `pass-env`, or the cache must be turned off.
   - **Tests.**
     - The integration tests run jig with a cache of their own, `jig-test-cache` in the temporary directory, collected
       down to 256 MiB. Every test shares it, as concurrent builds may, which checks that projects in different
       directories share results correctly.
     - The perf tests turn the cache off, so that what they time is built.
     - The depfile tests leave out the toolchain's identifying files, which the cache's test checks.
     - The test of a crate's discovered variables turns the cache off.
     - rig: collection by last use, with shared blobs, back to empty; new unreferenced blobs and `tmp` files kept, old
       ones removed; collecting when due, and clearing.
     - jig: the cache's directory from each variable, and sizes parsed and shown.
     - Integration: `jig cache` with the default and XDG directories and off; invalid sizes and commands; stats after a
       build; collection keeping everything under a large limit and nothing at 0; clearing.

### Phase 5: the remote cache

#### Steps

1. **Done:** `jig serve --stdio`, the framed protocol, and the remote cache behind the local one, over ssh.
   - **`rig::remote`.** A request then its reply, in turn, each a frame: a kind byte, a `u64` length, the payload.
     - After `HELLO`, which checks the magic bytes and the version and says whether the server takes pushes, there
       are `GET_MANIFEST`, `GET_RESULT` and `GET_BLOB`, which are replied to with `FOUND` or `NOT_FOUND`.
     - `MISSING` asks which of a list of blobs the server lacks.
     - `PUT_BLOB`, `PUT_RESULT` and `ADD_TO_MANIFEST` push, and are replied to with `OK` or `ERROR`.
     - Manifests and results travel as they are stored, so the client checks them by the hash that ends them, and
       blobs by their hash, as it does what it reads from disk.
     - A result is only found if the server has all its blobs.
   - **`serve(cache, reader, writer, push)`** answers until the stream ends. It refuses pushes unless `push`, a blob
     that doesn't have its hash, and a result that isn't valid. It touches what it sends, so the server's `gc` keeps
     what clients use.
   - **`Remote`**
     - It runs a command, such as ssh, and speaks to it on its stdin and stdout, connecting when first used. It can
       also use given streams, which the tests do.
     - One connection is shared, behind a mutex, so requests from the build's threads take turns.
     - Its stderr is kept, so that a failure says why, with the server's last line.
     - Any failure, a lost connection or an `ERROR` reply, turns it off for the rest of the build, and
       `Remote::error` says why.
   - **`Cache::with_remote`.** The local cache is in front of the remote one.
     - `manifest` adds the remote's sets of discovered inputs after the local ones.
     - `result` asks the remote when the local cache doesn't have a complete result, and keeps what it gets.
     - A blob that isn't local is fetched when it is restored or read, on the job's thread rather than the build's.
     - `push` sends the blobs the remote is missing, then the result, then the manifest entry, so the remote never
       has a result without its blobs. The executor pushes each result it adds.
   - **jig.**
     - `jig serve --stdio [--push]` serves the local cache, then collects it if due.
     - `JIG_REMOTE_CACHE=ssh://[<user>@]<host>[:<port>][/<jig>]` runs `ssh -o BatchMode=yes [-p <port>] <host> --
       <jig> serve --stdio`, with `--push` when `JIG_REMOTE_CACHE_PUSH=1`. Any other value is a command split at
       spaces.
     - A remote that failed is warned of after the build.
     - The remote isn't part of the configuration, as it changes nothing that is built.
   - **Policy.** Pulling is open to whoever can run the server. Pushing needs both the client to ask
     (`JIG_REMOTE_CACHE_PUSH=1`) and the server to allow it (`--push`). With ssh, `command="jig serve --stdio"` in
     `authorized_keys` makes a key read-only whatever the client asks. ssh handles authentication and encryption.
   - **Tests.**
     - rig, over pipes:
       - Pushing fills the server, and a second push sends no blobs.
       - Another cache finds the manifest and result there, fetches blobs as they are restored, and keeps them.
       - A server that doesn't take pushes refuses them.
       - A server that sends corrupted blobs, results and manifests is a miss.
       - A server that can't be run, or ends, fails with its stderr.
       - Another version is refused.
       - The executor: one build pushes, and another with an empty local cache fetches the edge.
     - jig: the commands `JIG_REMOTE_CACHE` gives.
     - Integration, with `jig serve --stdio --push` run by `env` with its own cache in place of ssh:
       - One checkout pushes, and another, with an empty local cache and no `rustc`, fetches every edge.
       - A remote that fails is warned of, and the build succeeds.
   - **Not done yet,** at first: requests took turns on one connection, and the manifest was asked for on every lookup
     of a cached edge, a round trip each. Step 3 does both.
2. **Done:** the user's configuration, for settings such as the remote cache.
   - **The file.** It is `$XDG_CONFIG_HOME/jig/config`, or `~/.config/jig/config`, or what `JIG_CONFIG` gives, and
     `JIG_CONFIG=off` turns it off.
     - It is a Jig file, processed before the project's `Jig` as if the project began with a `use` of it, so there is
       one syntax. `env()`, `@if` and `path()` work in it, and a project can read and override what it sets.
     - The project depends on it, so a change configures again. While it is missing, it is a missing path, so creating
       it configures again too.
   - **The cache's settings are builtin properties.** They are overridden by the variables which set them before:
     - `cache/dir`, or `JIG_CACHE`, defaulting to `$XDG_CACHE_HOME/jig` or `~/.cache/jig`;
     - `cache/size`, or `JIG_CACHE_SIZE`, defaulting to `'10G'`;
     - `cache/remote`, or `JIG_REMOTE_CACHE`;
     - `cache/push`, or `JIG_REMOTE_CACHE_PUSH`.
   - **Resolved when configuring.** `cache::Settings` is resolved once the configuration is evaluated, reading the
     variables as configuration variables, and recorded in `.jig` (version 19). A build reads it from there rather than
     from the environment.
     - Changing the remote cache now configures the project again, where it was only read by the build before.
     - A cached custom command's identity is found once the configuration is evaluated rather than as it is declared,
       as the cache can be turned on or off after it.
   - **Without a project.** `jig cache` and `jig serve` process the user's configuration alone, with the toolchain's
     builtin properties from rustc if it can be run, or empty, so that a server without Rust can serve a cache.
   - **Tests.**
     - The integration tests run jig with `JIG_CONFIG=off`, so the user's configuration doesn't affect them, and the
       perf tests do too.
     - jig: finding the file from each variable, and resolving the settings with each override, turned off, and invalid.
     - Integration:
       - `jig cache` takes the settings of the configuration, with a directory relative to it.
       - A project reads `cache/push` from it, depends on it, and pushes to the remote cache it gives.
       - Changing it configures again.
       - An error in it is reported at its path.
       - Creating it while it is missing configures again.
3. **Done:** concurrent requests on one connection, and fewer of them.
   - **Request ids (protocol version 2).** Each frame has the id of its request, which its reply has.
     - The client sends from any thread. A thread of the connection's own reads the replies, and gives each to the
       request which waits for it, so one ssh session carries every thread's requests at once.
     - The server reads requests in turn and answers them on four threads, in any order.
     - A connection which fails gives every waiting request no reply, which fails it, and turns the remote off.
   - **Lookups run on the jobs' threads.** The build's thread only finds an edge's first key. The job's thread looks
     the result up and restores it, so edges which wait on the remote wait together, rather than each blocking the
     build in turn.
     - It stamps the discovered inputs itself, by their contents, as the build's thread did. A local hit's inputs are
       read again when it is recorded.
   - **The remote is asked only once the local cache misses.** `Cache::find` tries each of the local manifest's sets
     of inputs against local results first, so a hit in the local cache sends nothing.
     - On a miss, the remote's manifest is asked for, once per first key for the life of the `Remote`. What it found,
       or that it found none, is kept, as a build only adds to a manifest what it pushes, which it has itself.
     - Then every set's second key, the local manifest's and the remote's, goes in one `GET_RESULTS`. The server
       replies with the first it has with all its blobs.
     - The set found is added to the local manifest, so the next lookup is local.
   - **Blobs are fetched together.** A result's missing blobs are asked for at once before it is restored.
   - **Pushes happen in the background.** `Cache::push` queues the result on a thread of the remote's, which sends the
     missing blobs at once, then the result and the manifest. `exec::build` waits for the queue before it returns, so
     a build ends once what it pushes is pushed. Dropping a `Remote` waits too.
   - **Measured.** A remote reached through a proxy which delays each chunk by 5 ms each way, about a 10 ms round
     trip. Fetching 40 independent crates into an empty local cache takes 0.55 s, against 2.0 s with requests in
     turn.
   - **Tests.**
     - Replies given to their requests when the server answers out of order.
     - 64 blobs fetched by eight threads at once.
     - A manifest found, and one not found, each asked for once.
     - A result found by the remote's set of inputs, which the local manifest lacks, in one request, and kept.
     - The tests which push wait for the queue.
   - **Not done yet,** at first: blobs were sent whole in a frame, and held in memory. Step 4 streams them.
4. **Done:** large blobs streamed in parts (protocol version 3).
   - **`MORE` frames.** A blob larger than a part, 1 MiB, is sent as a frame of the kind `MORE` for each part but the
     last, with the request's id, then its usual frame with the last part. It is sent that way both as the reply to
     `GET_BLOB` and as the request `PUT_BLOB`, whose hash leads its last frame.
     - Each part is written under the writer's lock on its own, so other requests and replies go between them rather
       than waiting for the whole blob.
     - The largest frame read is now 16 MiB.
   - **Parts are written as they are received.** `Cache::blob_writer` writes them to a file in `tmp`, hashed as it
     goes, which `BlobWriter::finish` adds once the blob is whole and has its hash, and which is removed otherwise.
     - On the client, a request may have a sink, which the connection's reader thread gives each part to, so the
       reader never waits on a bounded channel, and streams which interleave can't deadlock.
     - On the server, the reader thread writes the parts of each upload, by request id.
   - **Parts are read as they are sent.** `Cache::open_blob` opens the blob, which is read a part at a time, for
     pushes and for the server's replies.
     - The server hashes what it sends. If the blob turns out not to have its hash, it ends the reply with
       `NOT_FOUND` and removes the blob, and the client discards the parts it has.
   - **Measured.** A cached custom command writes 200 MB, which is pushed through `jig serve --stdio --push`, then
     fetched into an empty cache:
     - Pushing used 385 MiB at most in any process, now 32 MiB, and took 17.3 s, now 6.9 s.
     - Fetching used 194 MiB, now 32 MiB, and took 9.1 s, now 7.4 s.
   - **Tests.**
     - A blob of three parts and a bit fetched with small blobs, and blobs of two parts and a bit and of exactly one
       pushed.
     - A blob refused as it doesn't have its hash, leaving nothing in `tmp`.
     - A blob corrupted after its first part ended with `NOT_FOUND`, removed from the server, and not added to the
       client.
     - The tests which fetched blobs into memory now fetch them into a cache.
5. **Done:** a TCP transport for `jig serve`, with a shared token, for private networks (protocol version 4).
   - **The token.** It is proved without being sent.
     - The server's `HELLO` says whether it needs a token, and if it does, carries a random nonce of 32 bytes from
       `/dev/urandom`.
     - The client answers with `AUTH` and the BLAKE3 hash of the token and the nonce. The server compares it in
       constant time.
     - The server answers nothing else until then, and ends the connection with `ERROR` if the proof is wrong.
     - An overheard answer is no use with another nonce. The connection isn't encrypted, so it belongs on a trusted
       network; ssh both authenticates and encrypts.
   - **`rig::remote::listen`** accepts connections and serves each on a thread of its own, running a callback after
     each, which jig uses to collect the cache. A client that fails or is refused ends only its own connection.
     `serve_with_token` serves one stream with a token.
   - **`Remote::tcp(address)`** connects lazily to each address the host resolves to in turn, with a 10 second
     timeout, and `TCP_NODELAY`, as requests are small and waited for. `Remote::with_token` gives the token.
     - The connection keeps a handle to the socket, which it shuts down when it ends. Closing the writer alone left
       the socket open while the reader thread held its clone.
   - **jig.**
     - `jig serve --listen <address> --token-file <path> [--push]` requires a token, and prints the address it is
       bound to, so that port `0` can be used.
     - `cache/remote` takes `tcp://<host>:<port>`.
     - The property `cache/token-file` gives the client's token file. Its path is recorded in `.jig` (version 20),
       and the token is read when the build connects, so the token itself is never recorded.
   - **Tests.**
     - rig: the right token is served, a server without a token serves a client with one, and another token or none
       is refused, with why.
     - rig, over TCP:
       - Two clients at once, one pushing.
       - A client without the token refused while the server goes on.
       - The callback run after each client.
       - An address nothing listens at fails.
     - jig: `tcp://` addresses, with and without a host or port.
     - Integration: `jig serve --listen 127.0.0.1:0`, with the user's configuration naming it and the token file.
       One checkout pushes, another with an empty local cache and no `rustc` fetches every edge, and a client with
       another token is warned of and builds without it.
6. **Done:** the HTTP/1.1 client, for a server of files.
   - **Not bazel-remote.** The plan was for caches such as bazel-remote. But as far as I know, bazel-remote checks a
     blob put to `/cas/` against its SHA-256, and an entry put to `/ac/` against Bazel's `ActionResult`, while rig
     names blobs by BLAKE3 and stores its own results. So the client is for a generic server of files with `PUT`
     instead.
   - **The layout is a cache's directory.** Blobs are at `/cas/<the first two hex digits>/<the rest>`, results at
     `/ac/<key>` and manifests at `/mf/<key>`. A server of files can therefore serve a jig cache's directory as it
     is, and what is pushed is one, which `jig cache stats` reads.
   - **`rig::http::Client`**
     - It speaks `http://` only, as `std` has no TLS; a proxy that speaks TLS can be put in front. `https://` is an
       error which says so.
     - Connections are kept alive and pooled. A request on a pooled connection that fails before its response starts
       is sent again once, on a new one, rewinding a file it sends.
     - Bodies are sent with `Content-Length`, a file read as it is sent. They are read by `Content-Length`, in chunks,
       or until the connection closes, and given to a sink a part at a time. `HEAD`, `204` and `304` have none.
     - `http://user:password@host` authenticates with HTTP's Basic scheme. `cache/token-file` sends
       `Authorization: Bearer`.
   - **`Remote::http`** is a backend of the same `Remote`, so the local cache, lookups, background pushes and
     failure handling are as for the other transports.
     - Manifests are fetched once per key.
     - Results are asked for by every set's key at once, the first in order taken.
     - Blobs are fetched at once, each written to the cache in parts as it is read.
     - A push finds missing blobs by `HEAD`, puts them from their files at once, then the result, then the manifest
       merged with the server's. A push from elsewhere at once may lose one set of inputs or the other, as a miss.
     - Requests from one call go on up to 8 threads, each with a connection of its own.
     - Any status but `2xx` or `404`, or a failure to connect, turns the remote off, saying which request failed.
   - **Tests.**
     - rig, HTTP:
       - URLs parsed, with ports, IPv6 hosts, users, and what isn't one.
       - Base64 against known values.
       - Bodies of 300 KB and from files, with `Content-Length` and chunked, with trailers.
       - `HEAD`, and connections kept alive.
       - Basic and bearer authentication, and refusal.
     - rig, remote:
       - A result with a blob of three parts pushed, with and without chunked responses, kept in a cache's layout,
         and fetched by another cache.
       - Manifests merged.
       - A server which refuses with `401`, or isn't there, turns the remote off.
     - jig: `http://` and `https://` addresses.
     - Integration, with a small server of files:
       - One checkout pushes, and `jig cache stats` reads the served directory as a cache.
       - Another checkout with an empty local cache and no `rustc` fetches every edge.
       - `https://` is an error.

### Phase 6: distributed execution

#### Steps

1. **Done:** `jig worker`, which runs the actions of cached edges a build sends it once its own jobs are full
   (protocol version 5).
   - **`rig::worker`.** A worker serves the remote cache's protocol, and `EXECUTE`, with a `Request`:
     - the identity of the action's toolchain;
     - the action, with the build's directory replaced by a marker, as in its cache key;
     - the variables it is given: the build's, and the values of those it passes on;
     - the names of those it inherits from the worker, `PATH`, `HOME` and the others which locate its tools;
     - the blob of each input, declared and discovered, by its path relative to the build's directory;
     - and the paths of its outputs and depfile.
   - **Inputs.** The client asks which blobs the worker is missing, and sends only those, from their files. A worker
     takes blobs, which are checked against their hashes, though not results or manifests, which only a cache which
     takes pushes does.
   - **The sandbox.** `Executor::execute` runs the action in a new directory under `<cache>/work`. The build's
     directory is as deep in it as the paths leave it with `..`, in directories named `.rig-<n>`, which the paths don't
     name. The inputs are hard links to the worker's blobs, or copies where it can't link, or for one which is
     executable. The marker is replaced by the directory, and the action runs as a cached edge does here. The outputs
     it writes are added to the worker's cache, with what it printed, and the directory is removed.
     - A path which is absolute, or has `..` after a name, is refused, so nothing is written outside the directory.
     - The reply is whether the action succeeded, and its result, as a cache stores one. The client fetches the blobs
       it doesn't have, and restores the outputs as it restores a hit.
     - A toolchain the worker doesn't have is `NOT_FOUND`, and the client doesn't send it that toolchain's edges again
       in the build.
     - Actions run on threads of their own, `--jobs`, so that requests for blobs aren't held up behind them.
   - **The toolchains.** `jig worker` has each toolchain `rustup toolchain list` lists, run with `RUSTUP_TOOLCHAIN`,
     and the `rustc` in `PATH`, by their identities. They are found when the first action is sent, so that a build
     which connects is answered at once.
     - The identity is now of the sysroot's files by their paths within it, and not of the name the toolchain was
       chosen by, as one toolchain has several names, and a name given with `+<toolchain>` is in the action anyway.
   - **Which edges.** `exec::build` sends an edge to a worker when:
     - its jobs are full, and a worker has a job free;
     - it is cached, hermetic and not in the console pool;
     - it has run before, so that what it discovered is known;
     - and every path it has leads from the build's directory.
     The worker's jobs are counted apart from the build's, and don't take the jobserver's tokens.
   - **Fallback.** An edge which the worker refuses, which fails there, or which doesn't write every output, runs
     here, on the thread which waited for the worker. An action which reads a file it wasn't sent fails there, as
     the file isn't there, so a new module, which its last depfile doesn't list, runs here, and is found then.
   - **Connecting.** Workers are connected to on threads of their own once a build has an edge which could run on
     one, and are only sent edges once they are connected, so the build never waits for ssh.
   - **jig.**
     - `jig worker --stdio [--jobs <n>]`, or `jig worker --listen <address> --token-file <path> [--jobs <n>]`, which
       needs a token, as the worker runs what it is sent.
     - The property `workers`, or `JIG_WORKERS`, with the workers separated by `;`. Each is
       `ssh://[<user>@]<host>[:<port>][/<jig>]`, which runs `jig worker --stdio` there, `tcp://<host>:<port>`, with the
       token of `cache/token-file`, or a command. They are resolved when configuring, as the cache's settings are, and
       recorded in `.jig` (version 21).
     - What runs on a worker is added to the local cache, and pushed to the remote cache, as what runs here is.
   - **Tests.**
     - rig, worker:
       - Requests encoded and decoded.
       - How paths leave the build's directory, and those which don't lead from it.
       - An action run with its inputs, a directory down, with the marker replaced and its toolchain's variables. Its
         output is kept, an output it doesn't write is left out, and the directory is removed.
       - A failing action sent back with what it printed.
       - Another toolchain, a path outside, and a missing blob refused.
     - rig, remote: a cache which isn't a worker runs nothing. A worker over pipes reports its jobs, is sent the input,
       runs the action, and its outputs are fetched. Another toolchain is refused, and not asked for again.
     - rig, exec: with one job, the edges which don't fit run on a worker, once they have run here. One which reads a
       file it doesn't declare or discover fails there and runs here. What ran there is recorded and cached. A worker
       without the toolchain runs nothing.
     - jig: `JIG_WORKERS` overrides `workers`, and what isn't a worker, such as `http://`, is an error.
     - Integration: with `-j 1`, `jig worker --stdio` run by `env` with its own cache, in place of ssh.
       - Five independent crates' outputs are removed after a first build.
       - The second build sends the worker their inputs. The libraries it writes are the same, byte for byte, as those
         written here, and the diagnostics are shown.
       - A worker which can't be run builds nothing, and the build goes on.
   - **Not done yet:**
     - An interrupted build doesn't stop what runs on a worker.
     - An edge which runs here after its worker failed it runs beyond the build's jobs.
     - A worker must have the identical toolchain, of the same host, so it's of the client's platform; step 3 relaxes
       that.
     - A worker must have the same components as the client; step 2 relaxes that.
2. **Done:** capability reports, and installing toolchains (protocol version 6).
   - **`GET_CAPABILITIES`.** A worker replies with its `Capabilities`: its host, the tags it was given, and each
     toolchain's identity, rustup name, release, host and the targets it has standard libraries for, found from the
     directories of `lib/rustlib` which have a `lib`. They are found when they are first needed, and again once a
     toolchain is installed.
   - **The identity is of the channel's manifest alone,** rather than also the list of components, so that a worker
     with clippy, or another target, has the same toolchain. A target's standard library which a worker doesn't have
     fails the action there, which then runs here.
   - **Installing.** A request names the toolchain as rustup installs it, which configuring finds in the manifest:
     the version of a stable release, such as `1.97.0`, or the channel and date of a nightly or beta, such as
     `nightly-2026-11-03`. It's recorded in `.jig` (version 22), with the identity it's for, and given to rig as
     `Options::installable`.
     - A worker run with `--install` that doesn't have the identity runs `rustup toolchain install <name> --profile
       minimal --no-self-update`, finds its toolchains again, and runs the action if the identity is among them.
     - Installs take turns, and each name is tried once, so a name which doesn't give the identity, as a channel's
       manifest which was changed since, isn't installed again for each action.
     - A name has only letters, digits, `.` and `-`, as it is given to rustup as an argument.
   - **jig.**
     - `jig worker --tag <tag>`, which may be repeated, and `--install`.
     - `jig workers` asks each worker of `workers` at once, and shows its host, jobs and tags, with each toolchain's
       name, release and targets, or why it can't be reached.
   - **Tests.**
     - rig: capabilities encoded and decoded.
     - rig: a toolchain installed by the name a request gives, and found again, refused without an installer or a
       name, and a name which gives another identity tried once.
     - rig, remote: a worker's capabilities fetched, and a cache which isn't a worker refused.
     - jig: the names of stable, nightly and beta releases, a beta with a number which rustup names otherwise, and a
       date which isn't a name. A toolchain's identity is the same in another directory, and differs by version.
     - Integration: `jig workers` shows the worker's host, jobs and tag, and the release which built here.
   - **Not done yet:** a toolchain is installed with its host's standard library alone, and the targets an edge needs
     aren't known to rig, so step 3 adds them to the platform an edge needs.
3. Platform constraints on edges: `any`, `host == <triple>` or tags, matched against what workers report.
   - Proc-macro crates and edges linked with a system linker need the platform they were built on. Edges which link
     with `rust-lld` can run on any platform.
   - An edge which failed remotely because a file was missing is marked local-only until its inputs change.
4. Scheduling by the estimated time to transfer an edge's inputs and outputs against its wait here, from the durations
   in the log. Interrupts are passed on to workers.
5. Tests run on workers whose host is the test's target.

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
