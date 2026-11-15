# Windows

jig builds and tests projects on Windows, on x86-64 and Arm, as CI checks with the MSVC toolchains, and with the GNU
toolchain under Wine. There is no release for Windows, so jig is built from source there, with `bootstrap.sh` in Git
for Windows' bash:

```
sh bootstrap.sh
build/bootstrap/jig install
```

What differs from Unix:

- **Line endings.** jig reads `Jig` files with LF line endings only. Git for Windows checks text files out with CRLF by
  default, so a repository should have them checked out with LF, by `.gitattributes`, as jig's own does:

  ```
  * text=auto eol=lf
  ```

- **No shell.** Every command runs a program directly, as on Unix, and nothing is run by `cmd`. A custom command which
  needs a shell runs one itself, such as `'cmd' '/C' ...`, or `'sh' '-c' ...` where one is installed.
- **Programs.** A binary crate's output, and its test harness, are named with `.exe`, as rustc names them. A program
  found in `PATH`, as by `which` or a custom command's program, is found with the extensions of `PATHEXT`.
- **Directories.** The install prefix is `%LOCALAPPDATA%\Programs`, which needs no administrator, the cache is
  `%LOCALAPPDATA%\jig\cache`, and the user's configuration is `%APPDATA%\jig\config`, after the XDG variables, which are
  still taken first.
- **Modes.** A file's mode is only whether it is read-only, so an executable `install-mode` or `mode` makes nothing
  executable, and a generated file is compared by whether it is read-only.
- **Links.** Where a link can't be made, as making one needs a privilege, a copy is made instead.
- **`jig install --proxy`.** A wrapper is a program rather than a shell script, compiled once for the project with its
  toolchain. A wrapper which is running when it is reinstalled is renamed to `<wrapper>.old0` and so on, as Windows
  doesn't remove a program which is running, and the next reinstall removes it.
- **Hermetic commands** inherit, beside `PATH`, `HOME` and `TMPDIR`, the variables Windows programs need:
  `SystemRoot`, `SystemDrive`, `windir`, `TEMP`, `TMP`, `USERPROFILE`, `PATHEXT` and `ComSpec`.
- **Interrupts.** Each command runs in a job object, which Ctrl-C or Ctrl-Break interrupts, and which is ended a second
  later if it hasn't stopped. The jobserver is a named semaphore, as GNU make's is on Windows.
- **Paths.** jig shows the paths of the graph with `/` between their components on every platform, and gives commands
  paths as they are usually written, rather than as `\\?\C:\...`.

jig is developed for Windows on Linux too, building for `x86_64-pc-windows-gnu` and testing with Wine, as [developing
jig](developing.md#windows) describes.
