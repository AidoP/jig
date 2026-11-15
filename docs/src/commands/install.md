# jig install

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
