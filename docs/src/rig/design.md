# rig's design

rig is jig's build engine. `jig build`, `jig test`, `jig check`, `jig lint`, `jig run` and `jig install` all run it. It
is also a crate of its own, which knows nothing of jig, as [rig as a library](../rig.md) shows.

Its goals are:

1. Fast and parallel builds, on a no-op build as much as a full one.
2. Hashing of contents where it is wanted, which is more correct than comparing timestamps.
3. Use as a standalone crate, with no knowledge of jig.
4. A global build cache, shared between checkouts and projects, and between machines.
5. Distributed builds, on workers of several platforms.

The pages of this chapter describe how it meets them: [the graph](graph.md), [the state and how changes are
found](changes.md), [environment variables](environment.md), [running a build](execution.md), [the cache](cache.md),
[workers](workers.md), and [why rig is as it is](rationale.md).

## Principles

- **The graph is built through an API, not parsed from text.** jig hands rig a graph directly, which rig saves in its
  own binary format, so that a build loads it without evaluating the configuration again. rig reads no text format.
- **Commands are programs with arguments, not shell lines.** A command has a program, its arguments, environment
  variables, a working directory and a file for its stderr. rig runs it directly, so there is no quoting, no escaping
  and no limit on what an argument holds, such as a line break. A command is precise enough to hash for the cache,
  and to send to a worker. One which needs a shell runs one itself, as `sh -c`.
- **An edge is rebuilt when what was recorded differs,** not when an input is newer than an output. rig records the
  stamps of an edge's inputs when it runs, and the edge is out of date when a stamp differs from the one recorded. A
  file whose time goes back, as after `git checkout`, a restored backup or clock skew, is a change.
- **Layered.** The local engine, the cache and the workers are layers over one interface: a command and the hashes of
  its inputs go in, and its outputs come out. Each is tested, and turned on, by itself.
- **Crash-safe without signal handlers.** The state is an append-only log, of which each record is checked when it is
  read, and an edge's record is appended as soon as it finishes. An interrupted build loses only the edges which were
  running.
- **No dependencies.** rig, like jig, depends only on `std`. What it needs beyond it is written in the crate: BLAKE3,
  the depfile and JSON parsers, the jobserver, an HTTP/1.1 client and its own wire protocol. The `unsafe` code and the
  `extern "C"` declarations of the system's C library are in one platform module, `rig/sys.rs`, with
  `rig/sys/unix.rs` and `rig/sys/windows.rs` behind the same interface.

## Architecture

```text
            jig (configuration → graph)              other users of the crate
                       │                                       │
                       ▼                                       ▼
 ┌───────────────────────────── rig ───────────────────────────────┐
 │  graph    : files, edges, commands, aliases; binary format       │
 │  state    : append-only log of each edge's stamps and command    │
 │  plan     : stamps found in parallel, what is out of date, why   │
 │  exec     : critical-path priority, jobs, jobserver, pipelining  │
 │             local ─┬─ cache (local store, remote cache)          │
 │                    └─ workers (over ssh or TCP)                  │
 │  console  : progress in place, or quiet                          │
 └──────────────────────────────────────────────────────────────────┘
```

The crate is in `rig/`, with `rig/lib.rs` as its root. jig's `Jig` builds it as a crate like any other, and the `jig`
crate uses it. It is a library only, and no jig types appear in its API: jig turns crates and custom commands into
edges. `rig/Cargo.toml` lets cargo projects use it from git or a path, though it isn't published on crates.io.

## Formats and their versions

Each of the files rig and jig keep, and the protocol, begins with magic bytes and a version. One of another version
is not read: a graph or database of another version configures the project again, a state log of another version is
started again, and a cache entry of another version is a miss.

| What | Where | Magic | Version |
|---|---|---|---|
| The graph | `<build-dir>/graph` | `RIGG` | 10 |
| The state log | `<build-dir>/.rig/log` | `RIGL` | 4 |
| A cache's manifests and results | `<cache>/mf`, `<cache>/ac` | `RIGC` | 2 |
| The protocol of the remote cache and workers | | `RIGR` | 11 |
| jig's database | `.jig` at the project root | `JIGD` | 30 |

Numbers are little-endian, a length is a `u32`, a string is its length and its UTF-8, and a list is its length and its
items, as `rig::bytes` reads and writes them.
