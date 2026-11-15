# The cache

The cache keeps the results of edges by a key of their command and inputs, so an edge which has run anywhere with the
same command and inputs is fetched rather than run. How a project uses it, and how it is set up, is in
[caching](../caching.md).

## The store

A cache is a directory, by default `$XDG_CACHE_HOME/jig`, `%LOCALAPPDATA%\jig\cache` on Windows, or `~/.cache/jig`:

- `cas` holds the blobs, by their BLAKE3 hashes, in directories of their first two hexadecimal digits: outputs,
  depfiles, and what commands printed;
- `mf` holds the manifests, by first key;
- `ac` holds the results, by second key: each output's path, blob, and whether it is executable, and the blob of what
  was printed;
- `tmp` holds what is being written, which is renamed into place once whole, so builds which share the cache need no
  lock;
- `local` holds the marks of edges which run only here (see [workers](workers.md#fallback));
- `collected` is the mark of when it was last collected.

A manifest or result ends with the BLAKE3 hash of what precedes it, and a blob is checked against its hash as it is
read, so a corrupted entry is a miss, and is removed, rather than a wrong build.

## Keys

What an edge discovers is only known once it has run, so it is found in two steps, as ccache's manifest mode does:

1. The first key is of the edge's cache identity, its command, the build's variables, its outputs and depfile, the
   values of the variables it passes on, and the hashes of its declared inputs. It leads to a manifest, which lists the
   sets of discovered inputs and variables that edges of this key have read, the latest 16 of them.
2. For each set, the second key is of the first, and of the hashes of those inputs and variables as they are now. It
   leads to a result.

Each part has its length before it, and a missing file or an unset variable is marked as such.

## The toolchain's identity

An edge's cache identity identifies its toolchain: a hash of `rustc -vV`, its version, commit and host, and of the
channel's manifest which rustup writes into the sysroot, `lib/rustlib/multirust-channel-manifest.toml`, or without it,
of `bin/rustc`. It is the same wherever the toolchain is installed, and whichever components it has. The configuration
depends on that file, so a toolchain updated in place configures the project again. A cached custom command's identity
also has its program's contents, and what links has its linker's.

## Relocation

A result is shared between checkouts in different directories, so what is built mustn't depend on where the project
is. Every path jig gives rustc is relative to the build directory, and what rustc writes, other than its dep-info, is
remapped to be relative to the project root, with `--remap-path-prefix`. The first key has the build's directory,
where a command names it, replaced by a marker.

## Restoring

A hit is restored on the job's thread rather than running the command: each blob is copied, by `FICLONE` where the
file system can, to a file beside the output, checked against its hash, and renamed over the output. Its depfile is
restored with it, so it is recorded as if it had run, with the duration of its last run. What it printed is replayed.
A miss which succeeds adds its outputs, depfile and what it printed. A cache which can't be read or written is a miss,
never a failure.

## Collection

A hit touches its result and manifest, so their times are when they were last used. `Cache::gc` removes results, used
longest ago first, until the cache is no larger than a size, then each blob no result refers to, and the manifests not
used since. A blob no result refers to, or a file in `tmp`, is kept for an hour, as a build may be adding it. A build
collects the cache once a day, by the `collected` mark.

## The remote cache

The local cache can be in front of a remote one, which it asks only once it misses. A result fetched is kept locally,
and what is built is pushed to the remote, in the background, if it takes pushes. A remote which fails is turned off
for the rest of the build.

It has three transports, as `std` has no TLS:

- **ssh,** `ssh host jig serve --stdio`, speaking rig's protocol over the child's stdin and stdout. ssh authenticates
  and encrypts, and `command="jig serve --stdio"` in `authorized_keys` makes a key read-only.
- **TCP,** `jig serve --listen`, with rig's protocol and a shared token, for private networks. The client proves it
  has the token by its BLAKE3 hash with a random nonce the server sends, so the token isn't sent, but the connection
  isn't encrypted.
- **HTTP/1.1,** to a server of files with `GET`, `HEAD` and `PUT`, laid out as a cache's directory is, at `/cas/`,
  `/ac/` and `/mf/`, such as nginx with WebDAV. bazel-remote doesn't fit, as it checks blobs by SHA-256 and results as
  Bazel's.

## The protocol

A request and its reply are frames: a kind, the request's id, a `u64` length and the payload. Requests from every
thread go on one connection at once, and a thread of the connection's own gives each reply to the request it answers.
After `HELLO`, which checks the version, says whether the server takes pushes, and carries the nonce of a token, the
requests are `GET_MANIFEST`, `GET_RESULTS` (by every set's key at once, of which the first the server has whole is
returned), `GET_BLOB`, `MISSING` (which of a list of blobs the server lacks), `PUT_BLOB`, `PUT_RESULT` and
`ADD_TO_MANIFEST`, and for workers, `GET_CAPABILITIES`, `EXECUTE` and `CANCEL`. A blob larger than 1 MiB is sent in
parts, as `MORE` frames, which are written to the cache as they arrive.
