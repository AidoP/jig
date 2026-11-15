# Workers

A worker runs the commands of cached edges for a build, once the build's own jobs are full. It reuses the cache's
store and protocol: running a command on a worker is a cache miss which runs elsewhere. How a project uses workers is
in [workers](../workers.md).

## What a build sends

An edge may go to a worker if it is cached and hermetic, isn't in the console pool, and every path it has leads from
the build's directory, by `..` and then names. Its inputs are those it declares and those it discovered when it last
ran. An edge which hasn't run is sent too, if it has a discovery command: the job runs that here, as rustc with
`--emit=dep-info`, and sends the edge with the inputs its depfile lists.

A request, `EXECUTE`, has the identity of the command's toolchain, the command with the build's directory replaced by
a marker, the variables it is given, the names of those it inherits from the worker, the blob of each input by its path,
and the paths of its outputs and depfile. The build first asks which blobs the worker is missing, by `MISSING`, and
sends only those.

## Running there

A worker runs each command in a new directory under `<cache>/work`, as deep as its paths leave the build's directory,
with its inputs as hard links to the worker's blobs, or copies. The marker is replaced by the directory, and the command
runs as a hermetic edge does here. Its outputs are added to the worker's cache, and the reply is whether it succeeded,
and its result, which the build restores as it restores a hit. A path which is absolute, or which leaves the directory,
is refused.

## Toolchains and placement

A worker reports its capabilities, by `GET_CAPABILITIES`, as it connects: its host, its tags, and each toolchain it has,
with its identity, the identity of its release on any host, its rustup name, and the targets it has standard libraries
for. It keeps what identifies each toolchain in `<cache>/toolchains`, so it runs rustc only for one which is new.

An edge goes to a worker which has every tag it needs, and a toolchain of its identity, or of its release if it may
run on any host, with the standard library of its target, if it needs one. jig lets a crate's commands run on any host,
but for a procedural macro, a crate which uses one, and what is linked by the host's linker rather than `rust-lld`. A
worker run with `--install` installs a toolchain, or a target, which it doesn't have, with rustup, by the release's name,
trying each once.

## When to send

An edge goes to the worker on which it would finish soonest, if that is sooner than here:

- **There** is sending its files, of the sizes they had when it last ran, less those the worker has from this build,
  over the link's bandwidth, plus four round trips, plus running it at the worker's speed.
- **Here** is the least time an edge running here has left, by how long it last took, plus running it.

A link's latency is the round trip of `HELLO`. Its bandwidth starts at 50 MB/s, and is measured from transfers of 64 KiB
or more. A worker's speed, against here, is measured from edges which took 100 ms or more here. An edge which hasn't
run is estimated by the median of the cached edges of its toolchain which have. With `workers_only`, an edge a worker
may run waits for one, whatever the build's jobs.

## Fallback

An edge which a worker refuses, which fails there, or which doesn't write every output, is given back to the build,
and runs here once a job is free. One which fails there and succeeds here reads what it didn't declare or discover, so
it is marked, by its first key, in `<cache>/local`, and runs here until the key changes, or for a month.

## Building without the bytes

With `Options::download` `Targets`, the outputs of an edge which runs on a worker are left there, but for the targets
of the build, and its depfile and stderr file. An output left there is stamped by its hash, so the edge which wrote it
is up to date, and what reads it has its key in the cache by that hash. A file left there is fetched once an edge here
reads it, or a worker which doesn't have it is sent an edge which reads it: from the local cache, the remote cache, or a
worker. One which no worker has any longer is lost, and the build runs again, up to three times, to build it again.
Each build asks the workers about the files left on them, which keeps them from their caches' collection.

## Tests

A test of a target the host can't run goes to a worker whose host is its target, or for a test harness linked
statically, to any Linux of its architecture. It is sent its harness and what it depends on, and runs from where the
project root is, with the worker's own tools. What it printed is fetched through the cache. A test isn't cached, so it
runs each time.

## Interrupts

An interrupted build sends `CANCEL` to each worker running its edges, which interrupts their process groups, kills
those still running a second later, and refuses what it hadn't started. A worker does the same when its connection
ends, so nothing runs on for a build which is gone.
