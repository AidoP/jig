# The state, and how changes are found

## The state log

The state is `<build-dir>/.rig`. Its `log` is a record for each edge which has run, appended as the edge finishes, by
`rig::state::State`, which holds an exclusive lock on `lock` beside it while it is open, so that two builds don't run
in one build directory at once.

- Each record has its length and a checksum. A torn record at the end is dropped and written over.
- A record is keyed by the edge's first output, and holds the start and duration of its run, the hash of its command,
  and the stamps of its outputs, of its declared inputs, and of the inputs it discovered, with the variables it read.
- The log is rewritten without the records which were replaced once they are more than half of it.
- A log of another version is started again, which runs every edge once.

## Stamps

A stamp is what rig records of a file:

- **missing;**
- **its time and size,** the modification time in nanoseconds and the size, from one `stat`;
- **its contents,** the time and size with a BLAKE3 hash of the contents, for an edge which is hashed or cached;
- **left on a worker,** a hash and whether it is executable, for an output which a worker wrote and the build didn't
  fetch (see [workers](workers.md#building-without-the-bytes)).

Two stamps are the same when their hashes are, if both have one, or else when their times and sizes are. A file's
hash is reused while its time and size are those recorded, so an unchanged file isn't read twice. A source, which no
edge produces, modified within a tick of the file system's clock of when it was hashed, is hashed again, as it may have
changed again within the tick: a second, where times have no fraction of one, or 10 ms. A generated file isn't, as it is
written moments before it is stamped.

Before planning, a build collects every file it can reach, from the targets, the edges which produce them and the
inputs they discovered, and stamps them together on a thread pool, by `rig::stamps_of`.

## What is out of date

An edge is planned to run, with the reason, which `jig build -n` shows, if:

- it has no record;
- its command's hash differs from the record's;
- one of its outputs is missing, or isn't what the edge last wrote;
- an input's stamp, declared or discovered, differs from the one recorded;
- a variable it read or passes on has changed (see [environment variables](environment.md));
- or an input is produced by an edge which runs, unless that edge's outputs turn out to be unchanged.

The last is early cutoff. The outputs of a `restat` or hashed edge are stamped before and after it runs, and an edge
which would run only because they were rebuilt is checked again, and skipped if they are the same. A hashed edge
compares its inputs' contents itself, so early cutoff applies to it whatever produced them.

## Timestamps or hashes

jig's `change-detection` property is `'timestamp'` by default, which stamps a file by its time and size, as cheaply as
a `stat`, or `'hash'`, which stamps it by its contents too, so that a file written again unchanged rebuilds nothing. A
custom command's `change-detection` attribute replaces it for that command. An edge which is cached is always hashed, as
its keys in the cache need the hashes.

BLAKE3 is implemented in the crate, portably, from its specification, and checked against its official test vectors.
It runs at about 470 MB/s on one core.
