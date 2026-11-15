# jig check and rust-analyzer

`jig generate` writes `.rust-project.json` at the project root, which describes the project's crates to rust-analyzer:
each crate's root, edition, target, `cfg` and `env`, and the crates it uses, with the sources of the standard library
from the toolchain's sysroot. rust-analyzer finds it at the project root, as it does `rust-project.json`, so an editor
understands a jig project as it does a cargo one.

The file names `jig check <label>` as rust-analyzer's flycheck, which it runs when a file is saved, with the label of
the crate the file is in, as `<name>@<target>`. `jig check` brings the crate's check up to date, by building
`check/<label>`, then prints its diagnostics on stdout as rustc's JSON, followed by those of the crates it depends on,
which are edited, so rust-analyzer shows them all. Problems are shown on stderr, which rust-analyzer ignores, as is the
exit status.

A check is rustc with `--emit=metadata`, which is quick, and is cached as a build is. Its diagnostics are saved in
`<build-dir>/<target>/check/<name>.json`, so they are shown again when nothing needed checking. The checks aren't built
by `jig build` without targets, as they are for rust-analyzer.

```
./build/bootstrap/jig check app@x86_64-unknown-linux-gnu
./build/bootstrap/jig build check/app@x86_64-unknown-linux-gnu
```
