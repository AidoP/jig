# jig graph

`jig graph` shows rig's graph of the project, once it is up to date, for reading what it builds and how, as each of its
commands, or with outputs, only those which produce them. Each is a block headed by its outputs, followed by a line for
each of what it has, as `<key> = <value>`: its `description`, its `inputs`, the files it reads, which must be there, its
`dependencies`, those it depends on, which may be missing, as a crate it uses, what it is built `after`, without
depending on it, or for an alias, the targets it is an `alias` for, then its `command` line, as `-v` shows it, its
`depfile`, the `discover` command which writes the depfile without building, the `early` outputs it reports as it writes
them, the variables it passes on with `pass-env`, the `console` or `serial` pool, its `timeout` in seconds, the
identity it is `cache`d by, the `target`, `tags` and `any-host` of the workers which may run it, and whether it is a
`generator`, `restat`, `hashed` or runs `always`. The
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
