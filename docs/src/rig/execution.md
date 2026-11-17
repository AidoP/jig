# Running a build

`rig::exec::build` brings targets up to date: it plans what must run, runs it in order on the build's jobs, records
each edge as it finishes, and reports what happened to an `Observer`, such as the console.

## Order and priority

An edge runs once the edges it depends on, through aliases and discovered inputs, have finished, or, for what needs
only their early outputs, once those are written. Of the edges which are ready, the one with the longest path from it
to the end of the build starts first: its priority is how long it took when it last ran, from the state, or 1 ms if it
hasn't, plus the largest priority of the edges waiting for it. So a long chain of crates isn't left until last.

## Jobs

A build runs two more jobs than the machine has threads, or three with two threads, or two with one, as commands wait
on files and processes. `jig build -j` sets it. Each job runs its edges on a thread of its own.

rig runs a GNU make jobserver, so that rustc's codegen threads and a nested `make` or `cargo` share the build's jobs.
Commands are given it in `MAKEFLAGS` and `CARGO_MAKEFLAGS`: a FIFO, `--jobserver-auth=fifo:<path>`, on Unix, or a named
semaphore on Windows. Each edge beyond the first takes a token. When rig is run by a jobserver, it joins that one
instead. A jobserver given as file descriptors, by an older make, isn't joined.

## Commands

Each command runs without a shell, in the build directory, or the directory it gives, with stdin closed. Its stdout and
stderr go to one pipe, and what it printed is shown in one piece once it has finished. A command in the console pool
runs alone with the terminal. A command in the serial pool starts once nothing else is running, and nothing else starts
until it has finished.

A command which runs for longer than its edge's timeout is killed, with its process group, and fails with
`Outcome::TimedOut`. An edge which runs `always`, as a test does, is planned to run whenever it is wanted, with the
reason `Always`. `Options::given` gives such an edge arguments, variables and a timeout beside its own, as `jig test`
gives its tests what it was given, which is not part of the edge's action, as such an edge is never cached.

Each command, but a console one, leads a process group of its own, or on Windows, a job object. An interrupted build,
by `SIGINT`, `SIGTERM` or `SIGHUP`, or Ctrl-C, Ctrl-Break or the console closing on Windows, or by
`Options::cancel`, passes the interrupt to the groups of the commands which are running, and kills those still running a
second later. An edge which is interrupted isn't a failure, and isn't recorded.

An edge which fails isn't recorded, so it runs again. The build stops starting edges after `keep_going` failures, or
never with `0`, and the edges which depend on a failure don't run.

## Pipelining

A command may report some of its outputs before it has finished. rustc does so with `--json=artifacts`, once a crate's
metadata is written, so jig compiles a library with its `.rmeta` as an early output, and a library which uses it is
compiled against that. A dependent which needs only an edge's early outputs starts once the edge has reported them,
rather than once it has finished. What links, as a program or a test harness, uses the `.rlib`s.

With `--error-format=json`, rustc writes its diagnostics as JSON too. The job reads its stderr a line at a time, takes
the artifacts it reports, and keeps each diagnostic's `rendered` text, which is what rustc would have printed, so the
JSON isn't shown.

## The console

On a terminal with colour, rig shows a line for each command running at once, with how long it has run, updated in
place, and replaces them with a summary once the build is done. Elsewhere, it prints `[finished/total] description` as
each edge finishes. `--quiet` prints only what commands print, and `-v` shows each command rather than its description.

## Loading once

`jig build` brings the graph up to date first, which loads the graph and the state. While the graph's file is the
same, by its inode, size and modification time, the build which follows takes them as they are rather than loading them
again. A no-op build of 20,000 edges takes about 160 ms, of which stamping the files is about 35 ms. A daemon watching
the file system would save only that, so there is none.
