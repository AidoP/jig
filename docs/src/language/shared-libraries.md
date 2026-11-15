# Shared libraries

A `shared-library` statement declares a shared library of C and assembly, which is compiled as a
[static library](static-libraries.md) is, with `-fPIC`, and linked by the compiler, as an
[executable](executables.md) is. Crates, libraries and executables link it by `use`, and load it as they run. It takes
the attributes of an executable, where `use` names the static libraries it links into itself, and the shared libraries
it links, and is installed in `lib` by default, with the mode `0o755`.

The library is in its own directory, `<build-dir>/<target>/c/<name>/`, as a static library is, named as the platform's
linker finds it by its name: `lib<name>.so`, `lib<name>.dylib` on Apple's targets, or `<name>.dll` on Windows, which is
linked by its import library, `lib<name>.dll.a`, which its link writes beside it. It is linked as

```
<compiler> -shared <object> ... <library> ... <c/link-flags> <link-flags> -Wl,-rpath,<dir> ... <name> -o <library>
```

with `-dynamiclib` for `-shared` on Apple's targets, where `<name>` names it to what links it: its soname,
`-Wl,-soname,lib<name>.so`, by which what links it records it to load, its install name, `-install_name
@rpath/lib<name>.dylib`, on Apple's targets, or `-Wl,--out-implib,<import library>` on Windows. A target without an
operating system has no shared libraries, which is the error `shared-library-target`.

<!-- jig-test
files: codec.c tool.c base.c include/codec.h codec/lib.rs
-->
```jig
static-library base
    source 'base.c'

shared-library codec
    source 'codec.c'
    public-include 'include'
    use base
    install

executable tool
    source 'tool.c'
    use codec
    rpath '$ORIGIN/../lib'
    install

crate codec_sys 'codec/lib.rs'
    use codec
```

## Linking it

What uses a shared library links it as itself, without the static libraries in it, which are linked into it, but with
the shared libraries it uses, so that what it refers to in them is found:

- A crate is given `-l dylib=<name>`, which rustc records in a library crate's metadata, and what links is given its
  directory with `-L native=<dir>`, and depends on it, as for a static library.
- A shared library or an executable is given its path, or its import library's on Windows.

## Running what links it

Nothing is given an rpath unless it says so, so that what is built is the same wherever the build directory is, and is
fetched from the [cache](../caching.md) by it. [`jig run`](../commands/run.md) and the tests
[`jig test`](../commands/test.md) runs put the directories of the project's shared libraries before the platform's search
path of them, `LD_LIBRARY_PATH`, `DYLD_LIBRARY_PATH` on macOS, or `PATH` on Windows, as cargo does, so that a program
or test harness which links them finds them. A program which is installed finds those installed beside it by its
`rpath`, such as `'$ORIGIN/../lib'`, which is relative to the program, or `'@loader_path/../lib'` on macOS.
