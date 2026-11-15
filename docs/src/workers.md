# Workers

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

```jig
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
