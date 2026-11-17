# The graph

A graph is files, and the edges which produce them, built through `rig::Graph`'s API. jig builds one when it
configures a project, and saves it as `graph` in the build directory.

## Files

Paths are interned as `u32` IDs, and edges and files are stored as flat arrays, each file with the edge which
produces it. An output with two producers is an error when the edge is added, and a cycle is found by `validate`.
Paths are relative to the build directory, where rig runs. jig writes them with `/` between their components on every
platform, so that a target is named the same however it was made.

## Edges

An edge runs a command, or is an alias, which runs nothing and is up to date once its inputs are. An edge has:

- **outputs,** the files it writes, the first of which names it, as its record in the state does;
- **early outputs,** some of its outputs which it writes and reports before it has finished, as rustc does a crate's
  metadata, so that what needs only them starts sooner (see [pipelining](execution.md#pipelining));
- **inputs,** which must be there, or be built;
- **dependencies,** which may be missing, as a crate whose use was removed with it, and which rebuild the edge when
  they change;
- **what it is built after,** without depending on it, as a crate's `depends`;
- **a depfile,** in gcc's form, which its command writes, of the inputs it discovered, and the environment variables
  it read, as rustc's `# env-dep:` lines;
- **a command,** an `Exec`: a program, its arguments, environment variables, a working directory relative to the
  build directory, and a file its stderr is written to;
- **a description,** shown while it runs;
- **its pool,** `Default`; `Console`, whose edges run one at a time with the terminal; or `Serial`, whose edges each
  run with nothing else running;
- **flags:** `generator`, for the edge which generates the graph; `restat`, for an edge whose outputs are stamped again
  once it has run, so that what depends on an output it left alone isn't rebuilt; `hashed`, for one whose files are
  stamped by their contents; and `always`, for one which runs on every build which wants it, as a test does, which is
  never cached or sent to a worker;
- **a timeout,** how long it may run before it is killed and fails;
- **a cache identity,** for an edge whose results the cache keeps, which identifies its toolchain;
- **the variables it passes on,** of those it inherits, which are its inputs (see [environment
  variables](environment.md));
- **its placement:** whether a toolchain of the same release on another host may run it, the target whose standard
  library it needs, the tags a worker must have, and for an edge which runs only on workers, the target a worker's
  host must run (see [workers](workers.md));
- **a discovery command,** which writes its depfile without doing its work, as rustc with `--emit=dep-info`, so that
  an edge which hasn't run can be sent to a worker with the inputs it reads.

## Aliases and defaults

An alias names targets, such as jig's `test/<name>`, `check/<label>`, `lint` and `doc`. The defaults are what a
build without targets builds. An alias without inputs is always out of date.

## The generator edge

The edge which generates the graph is brought up to date before anything else. If it runs, the graph is loaded again
and the build planned afresh. jig's runs `jig generate`, whose depfile, `graph.d`, lists every file the configuration
read, the paths it found missing, and the variables it read, as `# env-dep:` lines. When the generator is the `jig`
which is running, and it is the only edge out of date, jig configures the project in its own process rather than
running a second `jig`, and records the edge as rig would.

## The format

The saved graph is its magic bytes and version, the paths, the edges, aliases among them, and the defaults, then a
checksum, FNV-1a over what precedes it. On load, the paths borrow from the file's bytes, so loading is one read and one
pass, with no allocation for each path. A graph of another version is configured again.
