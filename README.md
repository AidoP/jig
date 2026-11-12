# Jig

A build system for Rust.

**note:** This project is almost entirely vibe-coded with Claude.


## Installing

Each release on GitHub has a statically linked `jig` for x86_64, aarch64 and riscv64 Linux, and a `jig` for macOS on
Apple silicon, which are built and tested by `.github/workflows/release.yml` when a tag starting with `v` is pushed. A
tag with a suffix, such as `v1.0.0-rc.1`, is a prerelease.

`jig` is built for each of the `targets` property's targets whose standard library is installed, such as the musl
targets added with `rustup target add`. The `release-target` property, which is the host target by default, is the one
built by default, installed, and run by the integration tests. A release sets it to a musl target in `Jig.local`:

```
set release-target 'x86_64-unknown-linux-musl'
```

`jig` and rig are being ported to Windows, which is developed on Linux, building for `x86_64-pc-windows-gnu` with
MinGW, and running the tests with Wine, as CI does, with this in `Jig.local`, after `rustup target add
x86_64-pc-windows-gnu` and installing `wine64` and `gcc-mingw-w64-x86-64`:

```
set targets [host-target, 'x86_64-pc-windows-gnu']
target x86_64-pc-windows-gnu
    override
    linker 'x86_64-w64-mingw32-gcc'
    runner '/usr/lib/wine/wine64'
```

Then `jig test rig@x86_64-pc-windows-gnu jig@x86_64-pc-windows-gnu` runs their tests for Windows, and `jig lint` lints
them for Windows too.

## Building

Building `jig` needs Rust 1.97 or later. The tests need a nightly toolchain as well, for those of custom targets.

The `perf` test times `jig` on large generated projects, failing if one takes far longer than expected, or grows much
faster than its size where it should be linear. It is slow, so it is tagged `perf`, and
`jig test` leaves it out unless it is named, or run with `jig test --tag perf` or `jig test --all`, as CI does. Its
time budgets are multiplied by the `JIG_PERF_SCALE` environment variable, such as `JIG_PERF_SCALE=4 jig test perf` on
a slow machine.

The unit tests, run by `jig test jig`, include micro-benchmarks of parts of `jig`, such as reading a configuration and
finding the crates passed to each crate, which fail if one grows much faster than its input. Their times are shown by
running the test harness with `--nocapture`, as in `build/<target>/test/jig benches --nocapture`.

`rig/` is rig, the build engine which builds projects, as `docs/build-engine.md` plans. It is a crate which `jig`
uses, and has a `Cargo.toml` so that cargo projects can use it as a git or path dependency. `jig generate` writes rig's
graph to `graph` in the build directory, and rig builds the project, keeping the state of the build in `.rig` in the
build directory.

A graph is built through rig's API, of files and the edges which produce them, each by running a program, without a
shell, which a command which needs one runs itself, as `sh -c`. A project which uses rig builds one, and has rig bring
it up to date:

```rust
use std::path::Path;

fn main() -> Result<(), rig::Error> {
    // `hello.o` is compiled from `hello.c`, with the headers it includes found in the depfile `cc` writes
    let mut graph = rig::Graph::new();
    let (source, object) = (graph.file("hello.c"), graph.file("hello.o"));
    let cc = rig::Exec {
        program: "cc".into(),
        args: ["-c", "hello.c", "-o", "hello.o", "-MD"].map(String::from).to_vec(),
        ..rig::Exec::default()
    };
    graph.add_edge(rig::Edge {
        kind: rig::EdgeKind::Command(cc),
        description: "CC hello.o".into(),
        outputs: vec![object],
        inputs: vec![source],
        depfile: Some("hello.d".into()),
        ..rig::Edge::default()
    })?;
    graph.add_default(&[object]);
    // what ran is recorded in `.rig`, so that the next build runs only what is out of date
    let dir = Path::new(".");
    let mut state = rig::state::State::open(&dir.join(".rig"))?;
    let options = rig::exec::Options::new(dir);
    let mut console = rig::exec::Console::new(std::io::stdout(), false, false, false);
    let report = rig::exec::build(&graph, &mut state, &options, &[], &mut console)?;
    std::process::exit(if report.succeeded() { 0 } else { 1 });
}
```

A graph can be saved with `Graph::save`, and loaded again with `Graph::load`, so that what describes the build need not
run again. rig's tests run the [test helper](rig/test_helper.rs), their own executable run again, as the program of
their edges, so that they need neither a shell nor coreutils, as can a project's.

The build system needs to be bootstrapped, with `bootstrap.sh`, which builds `jig` with rustc alone, and configures the
project.

```
sh bootstrap.sh
```

Then run the bootstrapped `jig` to install.

```
build/bootstrap/jig install [prefix]
```

## Usage

This builds `build/bootstrap/jig` and configures the project. From then on, the project is rebuilt with `jig build`, and
tested with `jig test`.

```
./build/bootstrap/jig build
./build/bootstrap/jig test
```

`jig generate` evaluates `Jig`, writes rig's `graph` to the build directory and records the build
directory in `.jig`, at the project root. `jig build`, `jig check`, `jig test`, `jig run`, `jig lint` and `jig install`
find `.jig` in the working directory or its parents, so they can be run from anywhere in the project. They configure the
project first if it is not, from the `Jig` in the closest of those directories with one, or again from what it was
configured with if its build directory has been removed or `.jig` is from another version of jig. `jig build` takes the
targets to build, which are relative to the build directory, or the names `custom/<name>`, `check/<label>`, `test`,
`test/<name>`, `doc`, `doc/<name>`, `lint` and `lint/<name>`. Without targets, it builds the crates and custom commands
with `default`, or if none has it, every crate, test harness, documentation and custom command, but not the checks,
which are for rust-analyzer, nor the lints. It takes options before them: `-j <jobs>`, `-k <failures>`, `-n` for a
dry run, which shows why each edge would run, `-v` to show commands rather than descriptions, and `--quiet`.
rustc and clippy are run in colour, as their output is captured, and the colour is left out where the build's output
is not a terminal, so that what is built does not depend on where its output goes. `jig --color <when>`, before the
command, chooses otherwise, as `auto`, `always` or `never`, for the build, `jig lint` and jig's own diagnostics, such as
`jig --color=always build | less -R`.
With colour, where the build's output is a terminal, rig shows the build in place:
a line for each of the commands which run at once, by the order it started in, with how long it has run, such as
`[ 2/15] RUSTC libexample.rlib (3m)`, or `[  /15] -` once its command has finished and no other has started in its
place. What has finished is not kept, but what it printed is, above the lines, and once the build is done, the lines
are replaced by a summary, such as `[15/15] completed in 3m 14s`, or `[12/15] stopped after 2.0s` if it stopped first.
`jig --timings <command>` shows on stderr, once the command has finished, how long configuring, building, running a
program and running tests took, each in total, and the ten commands of the build which took longest, from
when each started until it finished, such as `jig --timings test`.
The graph regenerates itself when the configuration, or `jig`, changes. It does so with the `jig` which configured
the project, so running another one reports the `other-jig` warning, which names both, until `jig generate` configures
the project with the one which is running. Its severity is that of `diagnostic/other-jig`
when it was configured, where `'error'` stops the command before building.

`jig run [<name>] [--] [<argument> ...]` builds a binary crate and runs it with the arguments, in the working
directory, by the `runner` of its target if it has one, which it is given to before the arguments, and exits as it
does. What is built is reported on stderr, so that stdout is the program's. A crate is named as a test is, as
`<name>@<target>` if its target is not the host target, though its name alone is enough if only one target has a binary
crate with it, or only one of those is `default`, and without a name, the project's only binary crate is run, or the
only one which is `default`. Arguments which start with `-` follow `--`,
as in `jig run -- --help`.

`jig test [-v] [--all] [--tag <tag> ...] [<name> ...]` builds what the tests need and runs them in turn, showing the
output of those which fail, or of each with `-v`. Without names or tags, it runs the tests without a `tag`, and those
with a tag in the `test-tags` property, which is empty by default, so that slow tests can be left out. Tests can be
named, whatever their tags, or chosen by tag with `--tag`, which can be repeated, and `--all` runs every test. Named
tests run in the order they are named, followed by the others, in the order they were declared. A name or tag which no
test has is an error.

`jig lint` lints the crates of the project with clippy, which are those not from the toolchain, or only those named,
as `<name>`, or `<name>@<target>` if the target is not the host target. A crate is linted as it is checked, with the
levels of the lints set by the `clippy/<lint>` properties where it is declared. Every crate which can be is linted,
even if another fails, and the diagnostics of each are shown, even if nothing needed linting, as they are saved. It
fails if a lint is an error, such as one which is denied, and with `-D` or `--deny-warnings`, if there is a warning,
which is found in what was saved, so the lints are not run again, as `jig lint -D` is in CI.

A rustup toolchain is selected with `+<toolchain>` before the command, as with cargo, such as `jig +nightly build`. The
project is configured with it, so that rustc, the custom commands, the tests and `run` use it, until the project is
configured with another. `jig +<toolchain> generate` configures it with the
toolchain, and `jig generate` without one leaves the toolchain to rustup, while the other commands configure it again
if they are given a different toolchain. Without rustup, a toolchain cannot be selected.

```
./build/bootstrap/jig +nightly build
```

The results of rustc and clippy, and of custom commands with `cache`, are kept in a cache, and fetched from it
rather than run again. The cache is the `cache/dir` property, which is `$XDG_CACHE_HOME/jig`, or on Windows,
`%LOCALAPPDATA%\jig\cache`, or `~/.cache/jig`, by default, and `'off'` turns it off, usually set in the user's
configuration. The `JIG_CACHE` environment variable overrides it, relative to the working directory, while a `JIG_CACHE`
which is empty or `off` turns it off. They are read as the project is configured. A crate which is built again with what
it was built with before, such as once a change to it is undone, or another branch is checked out and back, or once the
build directory is removed, is restored from the cache, with what rustc printed. The cache is found by the contents of
each crate's sources, the crates it uses and the variables it read, and by the identity of the toolchain, from `rustc
-vV` and the files rustup writes when it installs or updates it, so that the project is configured again when the
toolchain is updated in place. Turning the cache on or off configures the project again too. The cache is shared between
checkouts of a project in different directories, as what rustc is given and writes does not depend on where the project
is. A crate or command which is cached runs in a hermetic environment, inheriting only the variables it passes on, with
`pass-env`, and those which locate tools.

`jig cache stats` shows where the cache is and what it holds, `jig cache gc [--max-size <size>]` removes what was used
longest ago until it is no larger than a size, such as `512M` or `10G`, and `jig cache clear` removes everything in it.
After a build, once a day, the cache is collected as by `jig cache gc`, down to the `cache/size` property, `'10G'` by
default, which the `JIG_CACHE_SIZE` environment variable overrides. `jig cache` takes the settings of the user's
configuration, rather than a project's.

```
./build/bootstrap/jig cache stats
JIG_CACHE_SIZE=2G ./build/bootstrap/jig cache gc
JIG_CACHE=off ./build/bootstrap/jig build
```

The cache can be in front of a remote cache on another machine, given by the `cache/remote` property, or the
`JIG_REMOTE_CACHE` environment variable which overrides it, as `ssh://[<user>@]<host>[:<port>][/<jig>]`, which runs `jig serve --stdio` there by ssh, in batch mode, so that it
fails rather than asking for a password, with the `jig` given after the host, or the one in `PATH` there. What is not
in the local cache is looked for in the remote one, and fetched from it into the local one. With `cache/push` set to
`true`, or `JIG_REMOTE_CACHE_PUSH=1`, what is built is pushed to the remote cache too, if it takes pushes, as `jig serve --stdio
--push` does, so that a machine which is trusted, such as CI, fills it, and others only read from it. ssh
authenticates and encrypts the connection, so a key in `authorized_keys` can be limited to reading the cache with
`command="jig serve --stdio"`, or allowed to push with `--push`. A remote cache which fails, as it cannot be reached,
is warned of, and the build goes on with the local cache. `JIG_REMOTE_CACHE` can also be a command, split at spaces,
which serves the cache on its stdin and stdout, such as `ssh -i key cache jig serve --stdio`.

A remote cache can also be a server of files over HTTP, `http://[<user>:<password>@]<host>[:<port>][/<path>]`, which
serves `GET` and `HEAD`, and `PUT` to push to, such as nginx with WebDAV. The files are laid out as a cache's directory
is, so a server of files can serve a jig cache's directory as it is. A user and password in the URL are sent by HTTP's
Basic scheme, and `cache/token-file` as a bearer token. Only `http://` is spoken, so a cache which is not on the
machine or a trusted network is put behind a proxy which speaks TLS to it.

On a private network, a remote cache can instead be served over TCP, by `jig serve --listen <address> --token-file
<path> [--push]`, and reached as `tcp://<host>:<port>`, with the `cache/token-file` property giving the file of the
same token. A client proves it has the token by the hash of it and a random nonce the server sends, so the token is
not sent, but the connection is not encrypted, so it is for networks which are trusted, while ssh both authenticates
and encrypts. The token is read from its file when the cache is connected to, so it is not recorded with the
project. As the remote cache is
part of how a project is configured, changing it configures the project again.

```
JIG_REMOTE_CACHE=ssh://cache.example ./build/bootstrap/jig build
JIG_REMOTE_CACHE=ssh://ci@cache.example JIG_REMOTE_CACHE_PUSH=1 ./build/bootstrap/jig build
```

### Workers

Once a build's own jobs are full, the cached commands it would run, such as rustc's, can run on other machines,
workers, given by the `workers` property, usually in the user's configuration, or the `JIG_WORKERS` environment
variable, with the workers separated by `;`, which overrides it. A worker is `ssh://[<user>@]<host>[:<port>][/<jig>]`,
which runs `jig worker --stdio` there by ssh, `tcp://<host>:<port>`, which `jig worker --listen <address> --token-file
<path>` serves, proved to by the token of `cache/token-file`, or a command which serves one on its stdin and stdout.
A worker runs as many commands at once as `--jobs`, or as the machine runs threads, and reports the tags of each
`--tag <tag>`. `jig workers` shows each worker, with its host, jobs and tags, how long its first answer took, and the
toolchains it has, with the targets they have standard libraries for.

A command runs on a worker once it has run here, so that the files its dep-info lists are known. Those and the files it
declares are sent to the worker, once each, as the worker keeps them in its cache, where the command runs in a directory
of its own with only them, beside the toolchain, which the worker must have: one with the same `rustc -vV`, installed by
rustup from the same release, whichever components it has. A worker run with `--install` installs a toolchain it does
not have with rustup, by the release's version, such as `1.97.0`, or its channel and date, such as `nightly-2026-11-03`,
with its minimal profile, trying each once. As the paths rustc is given and writes are relative, what it writes there is
what it would write here, which is fetched into the cache, and pushed to the remote cache as what is built here is. A
command which fails there, or reads a file it was not sent, runs here again, in one of the build's own jobs, as do those
of a toolchain the worker does not have. One which fails there and succeeds here runs here for a month, or until it or
its declared inputs change. A command goes to a worker only if sending its files, other than those the worker has from
the build, and fetching what it writes, by the latency and bandwidth measured of the worker's link, would take less time
than waiting for a job here, by how long the commands which run here took before. An interrupted build interrupts what
its workers run for it, as does one whose connection to a worker ends. A build which used workers ends by saying how
many of its commands ran on them. A worker runs whatever it is sent, so it must only be reachable by those who are
trusted.

With the `workers-only` property `true`, or `JIG_WORKERS_ONLY=1`, which overrides it, what a worker may build is not
built here, however many jobs the build has, but waits for a worker, as for a machine which should be left alone. What
must be built here still is: what no worker builds, as none has its toolchain or target, or none can be reached, and
what is not cached, such as custom commands without `cache`. What has not been built before, such as a fresh checkout's
crates, goes to workers too, once rustc finds the files it reads without compiling it, here first. A command which fails
on a worker is run here again, as without it.

With the `download` property `'targets'`, or `JIG_DOWNLOAD=targets`, which overrides it, what workers build is left on
them, rather than fetched, but for what was asked for: what is built by default, the files `jig install` installs, the
tests `jig test` runs, or the targets `jig build` is given. What is left there is fetched once something built here
reads it, or a worker which does not have it is sent something which does, and when it is asked for later, without
being built again. One which no worker has any longer, as its cache was cleared, is built again. With `'all'`, as by
default, everything a worker builds is fetched.

A worker must have the standard library of a crate's target, if the target is builtin, and its rustup installs it with
the toolchain, with `--install`. A crate's commands are sent only to workers with every tag its `worker-tag` attribute
gives. Where what rustc writes does not depend on the host, as for a library, or a binary linked by `rust-lld`, a
worker on another host with the same release of the toolchain builds it, such as an x86-64 Linux machine for an ARM
Mac. What depends on the host stays on workers with the same host: a procedural macro, a crate which uses one, as
rustc loads it, and what is linked by the host's linker, as binaries and tests are by default.

```
# ~/.config/jig/config
set workers ['ssh://build.example', 'ssh://ci@other.example/opt/bin/jig']
```

Tests run on workers too, where the host cannot run them. With workers, a builtin target which the host cannot run, and
which has no `test` attribute, is tested, so that `test {target}.test` tests its crates. `jig test` sends each of their
tests to a worker whose host is its target, or for a test linked statically, any Linux of its architecture, such as an
aarch64 or riscv64 machine, with its test harness and what it `depends` on, as a test which reads a file at run time
must depend on it. It runs there, from where the project root is, with the worker's own tools, while the other tests run
here, and its result is shown as if it had run here. A target whose `test` is `'workers'` has its tests run on workers
even where the host can run them.

### The user's configuration

What belongs to the user or the machine rather than a project, such as rig's cache, is set in the user's configuration,
`$XDG_CONFIG_HOME/jig/config`, or on Windows, `%APPDATA%\jig\config`, or `~/.config/jig/config`, or the file the
`JIG_CONFIG` environment variable gives, while a `JIG_CONFIG` which is empty or `off` turns it off. It is a file of the
same statements as `Jig`, which is processed before a project's own, as if the project began with a `use` of it, so that
a project can read what it sets, and set it again. Paths in it are relative to it, as in any file. A project depends on
it, so it is configured again when it changes, or once it is created. `jig cache` and `jig serve`, which have no
project, process it alone, where the builtin properties of the toolchain are empty if rustc cannot be run.

```
# ~/.config/jig/config
set cache/remote 'ssh://cache.example'
set cache/size '20G'
@if env('CI') == 'true'
set cache/push true
```

`jig install [<prefix>]` builds the outputs of the crates and custom commands with the `install` attribute, as the
target `install`, then copies each into its `install-dir` within the prefix, replacing any file there, with its
`install-mode` as its permissions, then prints how many files it installed. The prefix is relative to the working
directory, and is the `install-prefix` property, relative to the project root, without one.

With `--proxy`, a program, which is a binary crate or the output of a custom command with an executable `install-mode`,
is installed as a wrapper which runs `jig build --proxy <prefix> <wrapper>`, then runs the program with the wrapper's
arguments, so that it is rebuilt when it is out of date. Any other file is installed as a symbolic link to its output.
The wrapper runs the `jig` which installed it, writing the build's output to stderr. It is a shell script on Unix,
and on Windows, a program which does as the script does, compiled once for the project, with its toolchain, in its build
directory, with what it wraps after its end. A wrapper which is running when it is reinstalled is renamed, as Windows
does not remove a program which is running, to `<wrapper>.old0` and so on, which the next reinstall removes.

`jig build --proxy` brings the configuration up to date, finds the program installed at the wrapper in the prefix, and
builds it quietly. If the configuration or the program is out of date,
it first reports the `proxy-build` warning, before any diagnostics from configuring. Its severity is that of
`diagnostic/proxy-build` at the end of the configuration when it was last generated, where `'error'` fails without
configuring, building or running the program, and `'allow'` hides it. Then, if the wrapper would be
installed differently, such as when the program's output, its `install-mode` or `jig` has changed, the wrapper is
reinstalled and run again in place of the program. A wrapper whose program the project no longer installs fails.

```
./build/bootstrap/jig install
./build/bootstrap/jig install stage
./build/bootstrap/jig install --proxy ~/.local
```

rig builds in the build directory, so every path of its graph is relative to it, with those outside it leading out
of it with `..`, from where it is once symbolic links are followed. rustc, rustdoc and clippy run there too, and are
given those paths, but the paths they write other than in dep-info, as in diagnostics, panics, `file!()` and debug
info, are remapped to be relative to the project root, as `src/main.rs`, or `build/gen.rs` for a crate generated in the
build directory, with the project root as the working directory of debug info, so that what they write does not depend
on where the project is, and a build in another directory is the same. Custom commands, tests and `run` run from the
project root, so their paths are relative to it as usual.
A path, such as an input, an output, a crate or a linker script, cannot contain a line break, as a depfile cannot have
one, which is the error `newline-in-path`. rig runs a command with its arguments as they are, so an argument, such as an
`env` value or a codegen option, can contain one. A path cannot be empty either, as it would be the directory it is
relative to, which is the error `empty-path`.

`jig graph` shows rig's graph of the project, once it is up to date, for reading what it builds and how, as each of its
commands, or with outputs, only those which produce them. Each is a block headed by its outputs, followed by a line for
each of what it has, as `<key> = <value>`: its `description`, its `inputs`, the files it reads, which must be there, its
`dependencies`, those it depends on, which may be missing, as a crate it uses, what it is built `after`, without
depending on it, or for an alias, the targets it is an `alias` for, then its `command` line, as `-v` shows it, its
`depfile`, the `discover` command which writes the depfile without building, the `early` outputs it reports as it writes
them, the variables it passes on with `pass-env`, the `console` pool, the identity it is `cache`d by, the `target`,
`tags` and `any-host` of the workers which may run it, and whether it is a `generator`, `restat` or `hashed`. The
default targets follow, after `default`. Its paths are relative to the build directory, with `/` between their
components on every platform, and quoted for `sh` where they need to be. `--json` shows the same as JSON, an object of
`edges` and `defaults`, with each command's program, arguments, variables, directory and file for stderr apart, for
tools. It is a stable interface: a later jig may add keys, but doesn't change or remove those it has.

```text
x86_64-unknown-linux-gnu/libutil.rlib x86_64-unknown-linux-gnu/libutil.rmeta
    description = RUSTC x86_64-unknown-linux-gnu/libutil.rlib
    inputs = ../util.rs
    dependencies = x86_64-unknown-linux-gnu/libcore.rmeta
    command = rustc --crate-type=lib --crate-name=util ... ../util.rs
    depfile = x86_64-unknown-linux-gnu/libutil.rlib.d
    early = x86_64-unknown-linux-gnu/libutil.rmeta

test/app
    alias = x86_64-unknown-linux-gnu/test/app
```

```
./build/bootstrap/jig graph x86_64-unknown-linux-gnu/jig
./build/bootstrap/jig graph --json
```


### Statements

- `use <expression>`: start processing a file, which is one of:
  - a path, as a `str`, to the `Jig` file in the directory at the path, unless it has already been processed. The path
    is relative to the file, or to the project root if it starts with `/`. With the `file` attribute, the path is to
    the file itself, of any name. With the `force` attribute, the file is processed even if it has been already, with
    the values the properties have then, and `force <bool>` chooses whether it is. A file which uses itself, directly
    or through other files, raises a warning, unless it is forced, so that it can use itself until it stops, such as
    with `@if`. Files can be used up to 2048 deep, each by the one before, beyond which a `use` is an error
  - a builtin file, which is part of jig, and is processed each time it is used. Each is a read-only builtin property
    with its name, whose value is a reference to it, of type `'ref'`, which is its name where a string is needed. So
    `profile/` is a dict of the profiles, and a profile can be chosen as `use profile/{name}` or
    `use get(profile/, name)`. The builtin files are:
    - `profile/<name>`: set the codegen options of a builtin profile, `release` or `debug`. Like any other `set`, the
      options apply to the crates declared after it, and can be set again after it. See below
    - `msrv`: report a fatal error, `msrv`, if rustc is older than the minimum supported Rust version, the
      `rust-version` property, which must be set before it. A version is compared as by `semver-compatible`, ignoring
      pre-releases, so a nightly toolchain meets the version of its release
- `declare <name>`: define a property that can be used for substitution
- `set <name> <value>`: assign a value to a property
- `crate <name> [<path>]`: introduce a Rust library crate named `<name>`, with its root at `<path>`, or at
  `<name>/lib.rs` relative to the file without one
- `log <severity> <expression>`: report the value of an expression as a `fatal` error, an `error`, a `warning`, a `note`
  or a `suggestion`, where a fatal error stops the configuration, as described below. With the `quiet` attribute, only
  the message is shown, without where it was logged. With `id <str>`, the diagnostic has that identifier, which is shown
  with it and whose property, `diagnostic/<id>`, overrides its severity, as for other diagnostics, and is declared by
  the first `log` with the identifier. With `location <name>`, it is shown at another statement, rather than where it
  was logged: the one which gave a property its value, or declared it, or which declared a crate, custom command,
  target or test, whose name may be computed, such as `location {target}` in a `@for`
- `custom <name>`: a custom build command, which rig runs when its inputs change. Its outputs can be built with
  `jig build custom/<name>`
- `file <name>`: a file generated from a template string, which is written when the project is configured. See below
- `target <name>`: a custom target, which crates can be compiled for. See below
- `test <name>`: a test, which passes if its command succeeds. Like `custom`, it has `input`, `depends` and `command`
  attributes, but no `output` or `default`, and a `tag` attribute, a tag or a list of them, by which `jig test` chooses
  it, as below. Its inputs and dependencies are built with `jig build test/<name>`, or those of every test with
  `jig build test`

```
set rust-version '1.95.0'
use msrv

use 'local.jig'
    file

log note 'configured'
    quiet

log warning 'the `legacy` feature is deprecated'
    id 'deprecated'

@if uart-base % 0x1000 != 0
log error 'the UART base must be aligned to 4 KiB'
    location uart-base
```

A crate is a read-only property, `<target>/<name>`, for each target it is declared for, whose value is a reference to
it, of type `'ref'`. The target is part of the name, so a target from a property is computed with `{}`, as in
`{host-target}/core` or `{get(targets, 0)}/core`, while `host-target/core` is an error, as there is no target named
`host-target`. A crate can also be referred to by its name alone, as `core`, for the `target` property where it is
referred to, unless there is a property with the name. A custom command is a read-only property with its name, whose
value is a reference to it.

Where a string or path is needed, a reference is the path of the crate's output, or the paths of the custom command's
outputs, separated by spaces. These are relative to the project root, and are not resolved again relative to the file
which refers to them.

```
custom image
    input riscv64gc-unknown-none-elf/kernel
    output 'kernel.bin'
    command 'objcopy' '-O' 'binary' input output
```

A `<name>` is identifiers joined by `/`, where an identifier is ASCII letters, digits, `-` and `_`, not starting with a
digit. Crate and target names cannot contain `/`, and crate names cannot contain `-` either, as rustc does not accept
it. Names declared by `declare` cannot contain `/` either, as those are left to crates and codegen options. Any part of
a name can be computed with an expression in `{}`, as long as the whole name is valid. A computed part may also contain
`.`, as some targets do, such as `thumbv8m.main-none-eabihf`, which is referred to as
`{'thumbv8m.main-none-eabihf'}/core`, as a `.` written in a name takes an attribute, while crate names cannot.

```
crate compiler_builtins
set {'heap-{arch}'} 0x1000
set heap-{arch} 0x1000
log note {target}/compiler_builtins
```

In an expression, a name followed by `/` is a namespace, which is a dict of the properties named `<name>/<rest>` which
are set where it is evaluated, by `<rest>`, which may contain `/` itself. So `codegen/` is the codegen options which are
set, `diagnostic/` the severities which are overridden, and `<target>/` the crates for a target, by their names. As in
`6 / 2`, division has spaces around it. A namespace must have a property declared in it, or be a target, so that a
misspelt one is an error rather than empty.

```
log note keys({host-target}/)
@if get(codegen/, 'opt-level', 0) == 3
log note 'optimised'
```


A line starting with `#` is a comment. Indented by 4 spaces, a comment can be among the attributes of a statement, to
comment out one of them.

```
crate app
    # use util
    default
```


### Meta-statements

A meta-statement is a line starting with `@` which applies to the statement on the next line. There cannot be a blank
line or a comment between them, but several meta-statements can apply to the same statement.

- `@if <expression>`: skip the statement, including its attributes, unless the expression is `true`. The expression
  must be a `bool`, or coerced to one with `{}`. Once a statement is skipped, the conditions of later meta-statements
  are not evaluated

```
@if {device-tree}
@if target == 'riscv64gc-unknown-none-elf'
use 'dtb'
```

- `@else`: process the statement only if no statement has been processed in the chain of conditional statements before
  it. A chain starts with a statement with `@if` and is continued by each statement with `@else`, so `@else` followed
  by `@if` is an else-if. Blank lines and comments may separate the statements of a chain, but another statement ends
  it. `@else` must be the first meta-statement of its statement
- `@else-if <expression>`: the same as `@else` followed by `@if <expression>`

```
@if level == 1
log note 'one'
@else-if level == 2
log note 'two'
@else
log note 'many'
```

- `@for <name> in <value> ...`: process the meta-statements and statement after it once for each of the values, which
  are expressions separated by spaces, where each item of a list is a value. For each, the property `<name>` has the
  value, within the file and until the end of the statement. Without any values, the statement is skipped

```
@for target in targets
@if target != host-target
crate kernel 'kernel.rs'
    target target
```

- `@then`: chain the statement after it to the statement before it, which it must be directly after, so that the
  statements are processed or skipped together, and repeated together by `@for`. The chained statement can have its
  own meta-statements, which apply to it and the rest of the chain

```
@for a in 1 2
set total (total + a)
@then
log note 'added {a}, the total is {total}'
```

- `@with <name> as <value>`: process the meta-statements and statement after it, and the statements chained to it with
  `@then`, with the property `<name>` set to the value, as by `set`, then restore its previous value, unless they set
  it. Unlike `@for`, the property is set everywhere, including in files used by the statements. As with `set`, it can
  be declared later, which checks the value. Conditions after it continue the chain of those before it, or start one

```
@with target as 'riscv64gc-unknown-none-elf'
crate kernel 'kernel.rs'
@then
crate boot 'boot.rs'
```


### Expressions

An expression is a string, `true`, `false`, an integer, a list, a dict, or the name of a property or a crate.

An attribute is taken with `.` after the name of a property, a custom command, a file, a crate, or a target, as in
`data.default`, `image.input`, `{host-target}/macros.crate-type`, `macros.crate-type`, `my-kernel.runner` or
`{target}.has-std`, where a name computed in `{}` is put in `()` in a string template, as in `'{({target}.has-std)}'`.
The attributes of a property whose value is a reference are those of the crate, custom command or file it refers to.
Those of a property are `default`, `type`, `read-only` and `info`, those of a custom command are `input`, `output`,
`depends` and `command`, as lists, and `default`, `install`, `install-dir` and `install-mode`, those of a file are
`output`, `content`, `mode`, `install`, `install-dir` and `install-mode`, those of a test, which is looked for after the
others, are `input`, `depends` and `command`, those of a crate are `target`, `crate-type`, `path`, `output`, `builtin`,
`default`, `test`, `doc`, `use`, `depends`, `cfg`, `env`, `link-script`, `install`, `install-dir` and `install-mode`,
and those of a target, builtin or declared, are `inherit`, `test`, `runner`, `link-script`, `override`, `has-std`,
`can-run` and its options, such as `code-model`. An attribute which is not set is an error.

`has-std` is whether the toolchain has the standard library of a builtin target, as installed by `rustup target add`,
which is `false` for a custom target. The configuration is generated again when the target's libraries change, so it
follows the standard library being installed.

```
@for target in targets
@if {target}.has-std
crate app 'app.rs'
```

`can-run` is whether the host can run what is built for a target, which it can for the host target. For another, jig
runs a probe, a tiny program for the target's architecture which exits successfully, with the target's runner if it
has one, so it is `true` where the kernel runs the architecture, or an emulator does, such as one registered with
binfmt or given as the runner, or Wine for a Windows target. Probes are for Linux targets of x86_64, x86, the x32 ABI,
AArch64, 32-bit ARM, RISC-V, little-endian 64-bit PowerPC and s390x, and Windows targets of x86_64, x86 and AArch64, so
another target can only be run if it is the host target. A macOS target is probed on macOS by running `/usr/bin/true`
as its architecture with `arch`, so x86_64 runs on Apple silicon with Rosetta, while it cannot run on another host. The probe is run when the project is configured, so an
emulator which is installed later is found once it is configured again.

A list is written as expressions in `[]`, separated by `, `, such as `['-O', 'dtb']`, and its items can have any type.
Lists can be joined with `+` and compared with `==` and `!=`. As a string, such as in a string template, the items of
a list are separated by spaces, while each item is a separate argument of a command, such as for `run`.

A dict is written as entries in `[]`, as `name: value` separated by `, `, such as `[opt-level: 2, debug: true]`, where
`[:]` is an empty dict. A name is an identifier, or an expression in `{}`, as for a property. The entries are kept in
the order of their names. Dicts can be merged with `+`, where the entries on the right replace those on the left, and
compared with `==` and `!=`. As a string, the entries are written as `name: value` separated by `, `. Entries are read
with `get`, `contains`, `len`, `keys` and `values`.

Integers and integer properties can be combined with `+`, `-`, `*`, `/` and `%`, the bitwise `&`, `|` and `^`, and the
shifts `<<` and `>>`, with a single space on either side of the operator, and grouped with `(` and `)`. Both operands
must have the same type, `unsigned` or `signed`. A shift must be by `0` to `63` bits, and the bits shifted out are
lost, while `>>` keeps the sign of a `signed` integer.

Integers can be compared with `<`, `<=`, `>` and `>=`, and any two values of the same type with `==` and `!=`.
A `bool` can be negated with a leading `!` and combined with `&&` and `||`, while a leading `!` is the complement of
an integer, except in `{}`, which it takes as a `bool`. As in Rust, from lowest to highest, the precedence is `||`,
`&&`, comparisons, `|`, `^`, `&`, `<<` and `>>`, `+` and `-`, then `*`, `/` and `%`.

An expression written in `{}` is coerced to the type the operator requires, or to the type of the other operand.
This converts between `unsigned` and `signed`, parses a `str` as an integer or as `true` or `false`, treats a `bool`
as `0` or `1`, and treats an integer as `true` unless it is zero. Integer literals are only coerced between
`unsigned` and `signed`.

```
set size {pages} * 0x1000 - 1
set large size >= 0x10_0000 && !{small}
```

Functions are called with their arguments in `()`, separated by `, `:

- `upper(str)` and `lower(str)`: convert a `str` to upper or lower case
- `starts-with(str, prefix)` and `ends-with(str, suffix)`: whether a `str` starts or ends with another, as a `bool`
- `select(name, ...)`: the value of the first of the named properties which has a value, whose names are as for a
  property, which may be computed and have a `/`, such as `codegen/{target}/opt-level`, or a local, such as the
  variable of a `@for`. A property which is declared later has no value yet, while one which is never declared, nor
  set, is an error, so that a misspelt name is not ignored. A crate for a target, `<target>/<name>`, which has not been
  declared has no value, as it may be declared later
- `defined(name)`: whether a property has a value where the call is, as a `bool`, such as one which may be set in
  `Jig.local`, whose name is as for `select`
- `env(name)`: the value of an environment variable, which must be set. The build is reconfigured when it changes, or
  is set or unset, which `jig build`, `jig check` and `jig test` check before building. A hash of the value is
  recorded, rather than the value
- `len(value)`: the number of items in a list or entries in a dict, or of bytes in a `str`, which is UTF-8, as an
  `unsigned`
- `contains(list, value)`: whether a list has an item equal to a value, or a dict has an entry for a key, as a `bool`
- `keys(dict)` and `values(dict)`: the names or values of the entries of a dict, as a list, in the order of the names
- `get(list, index, default)`: the item of a list at an index, counting from `0`, or the value of a dict for a key, or
  the default if there is none, which is an error without a default
- `min(value, ...)` and `max(value, ...)`: the smallest or largest of integers, where each item of a list is a value
- `unique(value, ...)`: the values without those equal to an earlier one, as a list, where each item of a list is a
  value
- `sort(value, ...)`: the values in order, as a list, where each item of a list is a value. They must all be `str`,
  which are ordered by their bytes, `bool`, where `false` is first, or integers, which are compared as `signed` if any
  is
- `join(list, separator)`: the items of a list, converted to strings as in a string template, separated by a `str`
- `split(str, separator)`: the parts of a `str` between each non-empty separator, as a list, including empty parts
- `semver-compatible(version, requirement)`: whether a semantic version, `<major>.<minor>.<patch>`, meets a requirement,
  as cargo's default requirement for a dependency, as a `bool`. The version is at least the requirement, and before
  the next version which changes the requirement's leftmost number which is not `0`, or the last number it gives, so
  `1.2` allows up to `2.0.0`, `0.2.3` up to `0.3.0`, and `0.0.3` only itself. A pre-release, such as `1.2.3-beta.1`, only
  meets a requirement for a pre-release of the same numbers, and build metadata, after `+`, is ignored. With
  `ignore-pre-release: true`, pre-releases are ignored, so that a version is compared by its numbers alone, as a
  nightly version of rustc has the features of its release

```
@if !semver-compatible(rustc/version, '1.95', ignore-pre-release: true)
log error 'Rust 1.95 or later is needed'
```
- `path(str)`: a path relative to the directory of the file containing the expression, as a `str`. An absolute path
  is not made relative. `.` and `..` are resolved without following symbolic links
- `read(path)`: the contents of a file, without trailing newlines, where the path is relative to the file containing
  the expression. The build is reconfigured when the file changes, or is created if it was missing
- `exists(path)`: whether a file or directory exists, as a `bool`, where the path is relative to the file containing
  the expression. The build is reconfigured when an existing path changes, or when a missing one is created, which
  `jig build`, `jig check` and `jig test` check before building
- `try(expression, default)`: the value of the expression, or the default if evaluating it fails. Its errors are reported
  as warnings instead, except that an impure call is still an error, as evaluating it did not fail

```
set prefix try(env('CROSS_COMPILE'), '')
```

- `which(name)`: the path of the first executable file with the given name in the directories of the `PATH` environment
  variable. The build is reconfigured when `PATH` changes, or one of the directories searched before it changes
- `run(program, argument, ..., NAME: value, ...)`: run a program and return its output, without trailing newlines.
  As in `sh`, a program containing a `/` is a path, relative to the file containing the expression, while any other
  program is found in `PATH`. A program which is a reference is the path of the crate or custom command's output, which
  is not resolved again. It inherits the environment, with any variables given as `NAME: value` after the
  arguments, where `NAME` is a name like those of properties, so it can be computed with `{}`. The configuration fails
  if the program exits unsuccessfully. The build is reconfigured when a program given as a path changes, but not when
  its output would change otherwise, so it is impure, as described below

```
set board select(board, default-board)
@if starts-with(target, 'riscv')
use 'arch/riscv'
```

A call to `run` is impure, as the build is not reconfigured when its value changes, which is the `impure` error, so
that the build does not silently depend on what it does not track. It is allowed by setting `diagnostic/impure`, as
with `@with` for a single statement, which is suggested before the statement, or before the meta-statement containing
the call. A call in an `@else-if` has no suggestion, as `@with` cannot come before it.

```
@with diagnostic/impure as 'allow'
set version run('git', 'describe')
```

Within a string, an expression in `{}` is replaced with its value, and `{{` and `}}` with a single brace.
The expression cannot start with `{`, which would be `{{`, so a name computed from its start is put in `()`, as in
`'{({host-target}/core)}'`. It can contain strings, which are templates themselves.

```
log note 'size is {size} with {size / 0x1000} pages'
log note 'home is {env('HOME')}'
```

A multi-line string starts with `'''` at the end of a line and ends with `'''` at the indentation of that line. The lines
between are indented by 4 spaces more, which is removed, or are blank, and are joined by newlines, without one at the
end. A line starting with `#`, in the first column, is a comment which is skipped. Like other strings, each line is a
template.

```
log note '''
    first line
      indented line
'''
```

A `declare` statement may be followed by indented attributes:

- `type <type>`: the type of the property, one of `'str'`, `'bool'`, `'unsigned'`, `'signed'`, `'list'`, `'dict'`
  or `'ref'`
- `default <value>`: the initial value of the property, unless it is set before the declaration, such as by a file
  used first for local configuration, which replaces the default
- `info <string>`: a description of the property, included in diagnostics about it
- `read-only`: the property can only be assigned once, by a single `set` before the declaration, which replaces its
  default, or otherwise by its default or a single `set` after the declaration

A `crate` statement may be followed by indented attributes:

- `target <string>`: the target to compile the crate for, which is a builtin target or one declared by a `target`
  statement, or a list of them to declare the crate for each. Without it, the `target` property at the point of the
  declaration is used. A crate can be declared once for each target
- `use <name>`: a crate for the same target which the crate depends on, passed to rustc with `--extern`. It can be
  repeated, and the crate can be declared later or in another file. It can be a list of them instead, written in `[]` or
  computed in `{}`, such as a property, `use {drivers}`, whose items are names, or crates which have been declared, so
  the crates a crate uses can be chosen by the configuration
- `cfg <option>`: a configuration option, given with `--cfg`, as a name, a list of names, or a dict of names to values
  or lists of them, as `<name>="<value>"`, such as `cfg [feature: ['uart', 'plic']]`. It can be repeated, and is also
  given to rust-analyzer
- `env <variables>`: the environment variables rustc is run with for the crate, which `env!` reads, as a dict of
  their names to their values, such as `env [UART_BASE: uart-base]`, whose names are letters, digits and `_`. It can be
  repeated, and is also given to rust-analyzer. The crate is built again when its `cfg` or `env` change, as they are
  part of its command
- `pass-env <names>`: the environment variables rustc inherits for the crate, which `env!` reads, as a name or a list
  of them, such as `pass-env 'GIT_HASH'`. The crate is built again when one of their values changes, as
  it is when a variable `env!` reads changes, and a crate which is cached, when the cache is on, inherits only
  these and those which locate tools: `PATH`, `HOME`, `TMPDIR`, `RUSTUP_HOME` and `CARGO_HOME`. So a cached crate
  whose `env!` reads any other variable from the environment fails to compile, rather than being cached without it
- `worker-tag <tags>`: the tags a worker must have, as `jig worker --tag` gives them, for rig to send it the crate's
  commands, as a tag or a list of them, such as `worker-tag 'fpga'`. It can be repeated
- `depends <path>`: a file which is built before the crate, its test harness and its test, but which is not an input,
  so the crate is not rebuilt when it changes, such as a program its tests run. It is a path relative to the file, a
  reference, or a list of them, and can be repeated
- `builtin <bool>`: pass the crate with `--extern` to every crate for the same target which is not builtin, like the
  standard library. Builtin crates are not passed to each other, so a builtin crate must `use` the ones it depends on.
  It is `false` by default, and `true` without a value
- `crate-type <type>`: `'lib'`, which is the default, `'bin'` for an executable, `<build-dir>/<target>/<name>`, which
  cannot be used by other crates, or `'proc-macro'` for a procedural macro. A procedural macro is compiled for the
  host target, as rustc runs it, so it is not compiled for the `target` property, and can be used by crates for any
  target
- `default <bool>`: build the crate when `jig build` is run without targets, which is `false` by default. Without a
  value, it is `true`
- `codegen/<option> <value>`: a codegen option for the crate, which replaces the property of the option. See below
- `link-script <path>`: the linker script of the crate, which replaces that of its target, and is passed to the linker
  in the same way
- `test`: build the crate's tests with `--test` as `<build-dir>/<target>/test/<name>`, and register a test which runs
  them, named after the crate, or as `<name>@<target>` if the target is not the host target. Only crates for a target
  whose `test` is `true` can be tested, which it is by default for the host target and a builtin target which the host
  can run, as found by `can-run`. It is `false` by default, and `true` without a value, so `test {target}.test` tests a
  crate where its tests can run
- `tag <tag>`: a tag of the crate's test, or a list of them, by which `jig test` chooses it, such as `tag 'slow'`. A
  tag is not empty and has no whitespace. It can be repeated, adding to them
- `doc`: document the crate with rustdoc in `<build-dir>/<target>/doc/<name>`, which `jig build doc/<name>` builds, or
  `doc/<name>@<target>` if the target is not the host target, and `jig build doc` builds with the documentation of
  the other crates. The crates for a target are documented in the same directory, so that they link to each other. It is
  documented with the options it is checked with, and again when its sources change. It is `false` by default, and
  `true` without a value
- `install <bool>`: install the crate's output with `jig install`, which is `false` by default. Without a value, it is
  `true`
- `install-dir <str>`: the directory the output is installed in, relative to the prefix unless it is absolute, which is
  `'bin'` for a binary and `'lib'` otherwise
- `install-mode <mode>`: the permissions of the installed output, such as `0o644`, which are `0o755` for a binary or
  a procedural macro and `0o644` for a library. A mode is an integer, or a symbolic mode, as taken by chmod, which
  changes the default: clauses separated by `,`, of who they apply to, `u`, `g`, `o` or `a`, or everyone without any,
  and operations, `+`, `-` or `=`, with the permissions `r`, `w`, `x`, `X`, `s` and `t`, or those of `u`, `g` or `o`,
  such as `'u+s'` or `'go-w'`. There is no umask, so `'+x'` is `'a+x'`

```
crate core '{rust-src}/library/core/src/lib.rs'
    builtin

crate compiler_builtins
    builtin
    use core

crate build_tool
    target host-target
```

The stable codegen options of rustc are builtin properties named `codegen/<option>`, such as `codegen/opt-level`, which
have no value until they are set. A crate is compiled, and checked, with `-C <option>=<value>` for each of them which is
set where it is declared, as with the `target` property, unless the crate has an attribute for the option,
`codegen/<option> <value>`, which replaces it. A value is converted to a string as in a string template. For
`target-feature` and `remark`, a list is joined by `,`, for `llvm-args` and `passes` by spaces, and for `link-arg` each
item is a separate option. A path, such as for `linker`, is relative to the project root, unless it is a reference.
`codegen/` is a dict of the options which are set, by name, as a crate declared there would be given them, such as
`[opt-level: 's']`, as described for namespaces.

So that the options can be set for the whole project, as with cargo, some are not given to every crate:

- A procedural macro is not given `codegen/lto`, which rustc refuses for one, or `codegen/panic`, as it must unwind,
  unless it has the attribute itself.
- A procedural macro is for the host, where rustc loads it, so it is not given the options which describe the machine
  the crates are for, or how they are linked for it, by `codegen/<option>`: `code-model`, `default-linker-libraries`,
  `link-arg`, `link-self-contained`, `linker`, `linker-flavor`, `no-redzone`, `relocation-model`, `target-cpu` and
  `target-feature`. It is given them by `codegen/<host>/<option>`, where the host is `host-target`, or its own
  attribute. So one of them can be set for a whole project for a target without an operating system, such as
  `codegen/relocation-model 'static'`, which a procedural macro could not be linked with.
- A test harness is never given `panic`, as it must unwind on stable rustc, to catch each test's panic, nor are the
  checks and lints of a crate with tests, which are of its harness.
- A library built with `panic=abort` cannot be linked into what unwinds, a test harness or a procedural macro, nor can
  a library which uses one, as `panic` changes the hash of a crate. So each of those libraries which a test harness or
  a procedural macro links is built again without `panic`, as `<target>/unwind/lib<name>.rlib`, which they link
  instead, as cargo does, while binaries link the libraries which abort. Nothing is built again without `panic=abort`.

Each target has a property for each option too, `codegen/<target>/<option>`, such as
`codegen/x86_64-unknown-linux-gnu/opt-level`, which replaces `codegen/<option>` for the crates for that target, while a
crate's own attribute replaces both. They are declared when they are first used, for a builtin target or one declared
by a `target` statement, which may come after they are set. `codegen/<target>/` is a dict of those which are set for
the target, which are also in `codegen/`, by `<target>/<option>`.

```
set codegen/opt-level 's'
set codegen/debug-assertions true
set codegen/{host-target}/opt-level 3
crate kernel 'kernel.rs'
crate boot 'boot.rs'
    codegen/opt-level 'z'
```

The builtin profiles set the codegen options which are usually wanted together, as cargo's profiles do:

- `profile/release`: optimised aggressively, with `opt-level` 3, fat LTO, as `lto` `'fat'`, a single codegen unit, as
  `codegen-units` 1, `strip` `'symbols'`, which takes the symbols and debug information from what is linked, and
  `panic` `'abort'`, with `debuginfo` 0, and `debug-assertions` and `overflow-checks` off. It builds slowly, as each
  binary is optimised with all of the crates it links, in one codegen unit, and the libraries which tests link are built
  again to unwind, as described for codegen options
- `profile/debug`: `opt-level` 0, `debuginfo` 2, `debug-assertions` and `overflow-checks` on, and `lto` `'off'`,
  `codegen-units` 256, `strip` `'none'` and `panic` `'unwind'`, so that it undoes the release profile

```
use profile/release
set codegen/debuginfo 1
crate app 'main.rs'
    crate-type 'bin'
```

A `custom` statement is followed by indented attributes:

- `input <path> ...`: files the command reads, as expressions separated by spaces, each a path or a list of them,
  relative to the file containing the statement. It can be repeated, adding to the inputs
- `output <path> ...`: files the command writes, like `input`, but relative to the build directory, `build-dir`.
  Without it, the output is `<build-dir>/<name>`, and each `output` replaces the outputs, which are kept with the
  `output` property, as in `output output 'data'`
- `depends <path> ...`: files which are built before the command runs, like `input`, but which are not inputs, so the
  outputs are not rebuilt when they change. It can be repeated, adding to them
- `command <program> <argument> ...`: the command, as expressions separated by spaces, each of which is a single
  argument, except for a list, each item of which is an argument. As with `run`, a program containing a `/` is a path
  relative to the file, and the outputs are rebuilt when it changes, while any other program is found in `PATH`. A
  program which is a reference is the path of the crate or custom command's output. It is required
- `default <bool>`: run the command when `jig build` is run without targets, which is `false` by default. Without a
  value, it is `true`
- `install <bool>`, `install-dir <str>`, `install-mode <mode>`: install the outputs with `jig install`, as for a
  crate, in `'share'` with the permissions `0o644` by default, which `'+x'` makes executable
- `change-detection <str>`: how rig finds that the command's files have changed, `'timestamp'` or `'hash'`, in place
  of the `change-detection` property, such as to hash the small file a generator writes, so that what reads it is not
  rebuilt when it is written again unchanged
- `cache <bool>`: keep the command's outputs, and what it printed, in rig's cache when it is on, as rustc's
  are, so that it is fetched rather than run when it has run with the same inputs before, in this checkout or another.
  It is for a command whose outputs depend only on its inputs, its arguments and its program, and not on the time, the
  directory it is in, or files which are not inputs. A program found in `PATH` is identified by its contents, and the
  project is configured again when it changes, while one which is a path is an input. A command which is cached is
  stamped by the contents of its files, whatever its `change-detection`. A test cannot be cached
- `pass-env <names>`: the environment variables the command reads from those it inherits, as a name or a list of
  them, such as `pass-env ['CC', 'CFLAGS']`. The command runs again when one of their values changes, and a command
  which is cached inherits only these and those which locate tools, as a cached crate does, so that every variable
  which can change its outputs is part of the key it is cached by

A reference is its paths, as they are already resolved, while an absolute path is kept. While the attributes are
evaluated, the `input` and `output` properties are lists of references to the inputs and outputs so far. An output
cannot be, contain or be within the output of a crate, its test harness, or another custom command.

```
custom device-tree
    input 'virt.dts'
    output 'virt.dtb'
    command 'dtc' '-O' 'dtb' '-o' output input
```

A `file` statement is followed by indented attributes, which give the contents of the file, as it is written when the
project is configured, rather than by rig. It is only written when its contents change, so that what uses it is only
built again then, and is written again if it is removed. Like a custom command, it is a property with its name, whose
value is a reference to it, so it can be an input, or a `link-script`. Its output cannot overlap another output.

- `content <str>`: the contents, which are usually a multi-line string, where braces must be doubled. The file ends with
  a newline, which the last line of a multi-line string does not have, unless it is empty
- `output <path>`: the file, relative to the build directory, `<build-dir>/<name>` by default
- `mode <mode>`: the permissions of the file in the build directory, as for `install-mode`, such as `'+x'` for a
  script, which are `0o644` by default, and are set again when the project is configured if they have been changed
- `install`, `install-dir` and `install-mode`: as for a custom command, where the file is installed in `share` by
  default, with its `mode`, which a symbolic `install-mode` changes. A file whose installed permissions are executable
  is a program for `jig install --proxy`

```
file kernel-link
    output 'kernel.x'
    content '''
        ENTRY(_start)
        SECTIONS {{
            . = {kernel-base};
        }}
    '''

crate kernel 'kernel/main.rs'
    crate-type 'bin'
    link-script kernel-link
```

A `target` statement is followed by indented attributes, which describe the target with a JSON target specification,
`<build-dir>/targets/<name>.json`. Custom targets need a nightly toolchain.

- `inherit <target>`: the builtin target whose specification it is based on
- `override`: set the attributes of the builtin target with the name, rather than declaring a target. Only `test`,
  `runner`, `link-script`, `linker` and `linker-flavor` can be used with it, as the builtin target keeps its specification. The linker
  and its flavor are passed to rustc as codegen options of each crate for the target, unless the crate has its own
  `codegen/linker` or `codegen/linker-flavor`, and a linker which is a path is relative to the file, as a runner is.
  `rust-lld`, which rustup installs with the toolchain, cross-links a musl target, such as
  `aarch64-unknown-linux-musl` on `x86_64-unknown-linux-gnu`, as it has its own C runtime, while a glibc target also
  needs that target's C libraries
- `env`, `os`, `arch`, `linker <str>`: replace the field of the specification
- `code-model`, `relocation-model`, `target-cpu`, `linker-flavor`, `panic <str>`: replace the field of the
  specification for the codegen option
- `target-feature <features>`: the features of the target, as a `str` separated by `,`, or a list of them
- `no-redzone`: disable the red zone
- `link-script <path>`: the linker script of the crates for the target, as a path relative to the file, or a reference
  to a crate or custom command with one output, such as one which generates it. It is passed to the linker with
  `-C link-arg=-T<path>`, rather than written to the specification, when a binary, procedural macro or test harness is
  built, which depends on it, unless the crate has its own `link-script`
- `test <bool>`: whether crates for the target can be tested, which by default is its `can-run` for a builtin target,
  or, if there are workers, `true`, as their tests run on a worker which runs the target, and `false` for a custom
  target. Without a value, it is `true`, and with `'workers'`, the tests of its crates run on workers, even where the
  host can run them
- `runner <program>`: the program which runs a test harness for the target, given as its last argument, or a list of the
  program and its first arguments, as for the `command` of a custom command. As with `run`, a program containing a `/`
  is a path relative to the file, while a reference, such as to a crate or a custom command, is its output, which is
  built before the tests. Without it, a test harness is run directly

```
target riscv64gc-unknown-none-elf
    override
    test
    runner ['qemu-system-riscv64', '-machine', 'virt', '-nographic', '-kernel']

target aarch64-unknown-linux-musl
    override
    linker 'rust-lld'

target bluemetal-riscv64
    inherit 'riscv64gc-unknown-none-elf'
    os 'bluemetal'
    code-model 'medium'
    link-script 'link.x'
```

### Builtin properties

- `rustc/sysroot`: the sysroot of the Rust toolchain, as printed by `rustc --print=sysroot`, which is read-only
- `rustc/targets`: the builtin targets of rustc, as a list of their names, as printed by `rustc --print=target-list`,
  which is read-only. It does not include targets declared by `target` statements
- `rustc/version`: the version of rustc, from the `release` line of `rustc -vV`, such as `1.95.0`, or `1.96.0-nightly`
  for a nightly toolchain, which is read-only. As it is semantic, it can be compared with `semver-compatible`, where a
  nightly version is a pre-release, unless it is given `ignore-pre-release: true`
- `build-dir`: the directory of what is built, relative to the project root, `build` by default. It is read-only
  once it is used, such as by a crate or the outputs of a custom command, so it is set before them. When it changes,
  `jig build`, `jig check`, `jig test` and `jig install` regenerate the graph in the new build directory, and build
  there. The previous build directory is kept
- `target`: the target triple for crates without a `target` attribute, initially the host
- `install-prefix`: the directory `jig install` installs in when it is not given one, relative to the project root,
  `/usr/local` by default, or on Windows, where a user's own programs go, `%LOCALAPPDATA%\Programs`, which needs no
  administrator
- `test-tags`: the tags of the tests `jig test` runs without names or tags, beside the tests without tags, as a list,
  which is empty by default
- `change-detection`: how rig finds that a file has changed, `'timestamp'` by default, by its modification time and
  size, or `'hash'`, by a BLAKE3 hash of its contents too. With `'hash'`, a file which is written again unchanged, or
  changed and changed back, rebuilds nothing, and what depends on an output which is written unchanged is not rebuilt.
  A file is only read again when its time or size has changed, or when it is a source which was modified within a tick
  of the file system's clock of being hashed, as it may have changed again without its time showing it
- `cache/dir`, `cache/size`, `cache/remote` and `cache/push`: rig's cache, as described for it, usually set in the user's
  configuration: its directory, `$XDG_CACHE_HOME/jig`, `%LOCALAPPDATA%\jig\cache` on Windows, or
  `~/.cache/jig` by default, or `'off'`; the size it is
  collected down to, `'10G'` by default; the remote cache it is in front of, empty by default; and whether it pushes
  to it, `false` by default. Each is overridden by an environment variable: `JIG_CACHE`, `JIG_CACHE_SIZE`,
  `JIG_REMOTE_CACHE` and `JIG_REMOTE_CACHE_PUSH`
- `cache/token-file`: the file of the token which proves rig to a remote cache which needs one, as one reached over
  TCP does, and to workers reached over TCP, empty by default
- `host-target`: the target triple of the host, as printed by `rustc --print=host-tuple`, which is read-only
- `msrv` and `profile/<name>`: the builtin files, which `use` processes, as described for `use`, which are read-only
- `rust-version`: the minimum supported Rust version, such as `1.95.0`, which `use msrv` requires of rustc, with no
  value until it is set
- `toolchain`: the rustup toolchain given with `+<toolchain>`, or empty if none was given, which is read-only
- `workers`: the workers which cached commands run on once a build's own jobs are full, as described for them, empty
  by default, which `JIG_WORKERS` overrides
- `workers-only`: whether what a worker may build waits for one, rather than being built here, as described for
  workers, `false` by default, which `JIG_WORKERS_ONLY=1` overrides
- `download`: what is fetched of what workers build, `'all'`, as by default, or `'targets'`, as described for workers,
  which `JIG_DOWNLOAD` overrides
- `codegen/<option>`: a codegen option of the crates declared while it is set, as described for `crate`, while
  `codegen/` is a dict of those which are set
- `codegen/<target>/<option>`: a codegen option of the crates for a target declared while it is set, which replaces
  `codegen/<option>`
- `clippy/<lint>`: the level of a clippy lint for the crates declared while it is set, which `jig lint` lints them with:
  `'allow'`, `'warn'`, `'force-warn'`, `'deny'` or `'forbid'`, such as `set clippy/needless_return 'deny'`, where
  `-` can be written for `_`. A group, such as `clippy/pedantic`, is given before the single lints, so that a lint in
  it can be given another level. As clippy has many lints, each is declared when it is first used, while `clippy/` is
  a dict of those which are set
- `diagnostic/<id>`: the severity of a diagnostic, as described below

### Diagnostics

Each diagnostic has an identifier, shown after its severity, as in `error[unknown-target]`, except for those of `log`
statements without an `id` attribute, whose identifier is `user`. Its severity is replaced by the value of its
property, `diagnostic/<id>`, where it is reported: `'fatal'`, `'error'`, `'warning'`, `'note'`, or `'allow'` to hide
it. A note or suggestion attached to a diagnostic, such as the list of targets after an unknown target, is hidden with
it. Errors within the first argument of `try` are still reported as warnings, except for `impure`. Diagnostics found
once the files are read, such as unknown targets of crates, have the severities set at the end.

A fatal error stops the configuration once the statement which reported it is done, so the rest of its file, the
files which used it and any repetitions of a `@for` are not processed, nor is the configuration checked or generated.
It suits a configuration which cannot go on, such as for an unsupported device, as later diagnostics would only follow
from it.

```
set diagnostic/unknown-target 'warning'
set diagnostic/user 'allow'
```

Hiding or lowering an error does not make the configuration valid, so what is generated may not build.
