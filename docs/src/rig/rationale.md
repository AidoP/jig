# Why rig is as it is

## Decisions

- **Its own engine, rather than ninja.** jig ran ninja at first. ninja's graph is text, so every command was a shell
  line, quoted and escaped, an argument couldn't hold a line break, and ninja tracked no environment variables, cached
  nothing and ran nothing elsewhere. rig replaced it in phases, compared against it, and ninja was then removed, without
  a deprecation period, as keeping it meant keeping two engines' behaviour alike. A differential test now checks rig
  against a model of what a build must do, which was itself checked against ninja.
- **No dependencies.** Everything is written in the crate, at the cost of maintaining it: BLAKE3, the protocol, the
  HTTP client, the jobserver, and the system calls. TLS is left to ssh, or a proxy, rather than written.
- **`unsafe` in one place.** `rig/sys.rs`, and its platform modules, hold the system calls: process groups and signals
  or job objects and console events, `mkfifo` or a named semaphore for the jobserver, `ioctl(FICLONE)` for reflinks, and
  random bytes.
- **Timestamps by default.** A stamp of time and size, from one `stat`, catches more than ninja's comparison did, as a
  time going back is a change. Hashing is an option, and the default for what is cached.
- **Packaging.** `rig/` has a `Cargo.toml` for cargo projects, but isn't published on crates.io.

## Decided against

- **io_uring for stamping.** `IORING_OP_STATX` was measured against a thread pool on 60,000 paths, a third of them
  missing, on 4 cores with Linux 6.18:

  | | warm | cold |
  |---|---|---|
  | one `stat` at a time | 58 ms | 212–255 ms |
  | a thread pool (`rig::stamps_of`) | 19–24 ms | 79–108 ms |
  | io_uring `IORING_OP_STATX` | 97 ms | 214–237 ms |

  The kernel can't complete `statx` inline, so it hands each request to io_uring's own threads, and gained nothing for
  its `unsafe` code. Reading files to hash them could be reconsidered, with a measurement which shows a gain.
- **A daemon.** A process watching the file system would make a no-op build of 20,000 edges about 10 ms rather than 160
  ms, but would outlive builds, and must notice when its watch overflows. Projects that large are unlikely, so it was
  decided against once loading and planning were made faster.
- **bazel-remote as the remote cache.** It checks blobs by SHA-256 and results as Bazel's, while rig's are by BLAKE3
  and its own, so the HTTP transport speaks to any server of files instead.

## Risks

- **A cache is only as correct as an edge's inputs are complete.** Only rustc, rustdoc and clippy, whose depfiles are
  complete, and custom commands with `cache`, are cached. Hermetic environments make a variable which isn't declared
  fail rather than be left out of a key, and the toolchain's identity and remapped paths are tested by comparing a
  cached result with a fresh run.
- **What rustc writes is assumed not to depend on the host** for what may run on any host, rather than tested, as the
  tests have one host.
- **An edge which reads what it doesn't declare** fails on a worker, and is marked to run here, rather than being
  found before it is sent.
