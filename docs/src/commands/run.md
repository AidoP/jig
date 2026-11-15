# jig run

`jig run [<name>] [--] [<argument> ...]` builds a binary crate, or an [executable](../language/executables.md) of C,
and runs it with the arguments, in the working
directory, by the `runner` of its target if it has one, which it is given to before the arguments, and exits as it
does. What is built is reported on stderr, so that stdout is the program's. A crate is named as a test is, as
`<name>@<target>` if its target is not the host target, though its name alone is enough if only one target has a binary
crate with it, or only one of those is `default`, and without a name, the project's only binary crate is run, or the
only one which is `default`. Arguments which start with `-` follow `--`,
as in `jig run -- --help`.

```
./build/bootstrap/jig run
./build/bootstrap/jig run app -- --help
./build/bootstrap/jig run kernel@riscv64gc-unknown-none-elf
```
