# Static libraries

[A kernel in Rust, C and assembly](../kernel.md) shows static libraries in a project without an operating system.

A `static-library` statement declares a static library of C and assembly, `lib<name>.a`, which is compiled by a C
compiler, archived, and linked into the crates which `use` it. It is followed by indented attributes:

- `source <path> ...`: the sources, as expressions separated by spaces, each a path or a list of them, relative to the
  file, or a reference to a custom command which generates one. The language of each is found by its extension: `.c` is
  C, `.S` and `.sx` are assembly which the C preprocessor reads first, and `.s` is assembly. It can be repeated, adding
  to them, and is required
- `target <string>`: the target, or a list of them, to declare the library for each, as for a crate, the `target`
  property by default
- `include <path> ...`: directories passed to the compiler with `-I`, like `source`. It can be repeated
- `public-include <path> ...`: directories which the library is compiled with, and so is what uses it, as for the
  directory of its headers
- `define <defines>`: macros passed with `-D`, as a name, a list of names, or a dict of names to values, such as
  `define [UART_BASE: 0x1000]`. It can be repeated
- `flags <arguments>`: arguments passed to the compiler for every source, after those of the properties below, as a
  `str`, which is one, or a list of them. It can be repeated
- `use <name>`: another static library, or a [shared library](shared-libraries.md), for the same target, as a name or
  a list of them, which is linked after this one, as a linker which reads each archive once needs, and whose
  `public-include` directories this one is compiled with
- `whole-archive <bool>`: link every object of the library, even one which nothing refers to, such as the entry point
  of a kernel, which only its linker script names. It is `false` by default, and `true` without a value
- `depends <path> ...`, `pass-env <names>` and `default <bool>`: as for a [custom command](custom-commands.md), where
  `default` builds the archive when `jig build` is run without targets

A library is a read-only property, `<target>/<name>`, as a crate is, whose value is a reference to its archive, and a
crate and a library cannot have the same name for a target. A name may be any that a crate may have, including `c` or
`m`, so that a project can build its own C library, as for a target without an operating system. The note
`name-shadows-system` says so, as a library is found by its name, and each library has its own directory, which is
given only to the links which use it, so it replaces the system's library of the name in those, and nowhere else.

<!-- jig-test
files: src/uart.c src/boot.S include/uart.h
-->
```jig
static-library uart
    source 'src/uart.c' 'src/boot.S'
    public-include 'include'
    define [UART_BASE: 0x1000]

crate drivers 'drivers/lib.rs'
    use uart

crate kernel 'kernel/main.rs'
    crate-type 'bin'
    use drivers
```

## Compiling

Each source is compiled in the build directory to an object, `<build-dir>/<target>/c/<name>/<path>.o`, named by the
path of the source, as

```
<compiler> -fPIC <derived> <flags> <library's flags> -I<include> ... -D<define> ... -MD -MF <object>.d -c <source> -o <object>
```

where `-fPIC` is given for a target with an operating system, but Windows, so that a shared library can link the
library, as Rust's libraries are compiled for such a target, and `<derived>` is `-O<level>` from `codegen/opt-level`, and `-g` from a `codegen/debuginfo` which is not 0, as they
are set where the library is declared, so that a [profile](codegen.md) reaches C too, and `<flags>` is `c/flags` for C,
and `asm/flags` for assembly, which come after them, so they can replace them. The compiler writes a depfile, so a
source is compiled again when a header it includes changes, and the paths it writes, in debug information and
`__FILE__`, are made relative to the project root by `-ffile-prefix-map`, as rustc's are. The objects are stamped by
their contents, so a header which is touched but not changed, or changed only in a comment, archives nothing again.

Each compile is kept in rig's [cache](../caching.md), as rustc's are, and fetched rather than run when the source, the
headers its depfile lists, the command and the compiler are as they were, in this checkout or another, as the paths it
is given are relative to the build directory. The compiler is identified by the contents of its program and what the
command prints with `--version`, so that a compiler wrapped by another, as `ccache gcc`, is identified by gcc's version,
and the project is configured again when the program changes. A compile which is cached runs in a hermetic
environment, as a crate does, so a variable the compiler reads, such as `CPATH`, is passed on with `pass-env`. A compile
is not sent to [workers](../workers.md), which have no C compiler a build knows of. The archiving is cheap, so it is
not cached.

The objects are archived by `<archiver> crsD <archive> <object> ...`, or `crs` on macOS, whose `ar` does the same with
`ZERO_AR_DATE`, so that the archive depends only on the objects. When a source is removed from the library, its
archive is removed as the project is configured, so that it keeps no object of it.

## Editors

jig writes `compile_commands.json` to the build directory, with the command of each source of C and assembly, as it is
compiled there, so that clangd finds the include directories, the macros and the flags of each file. clangd looks for
it in `build`, beside the directories of the sources, so it finds it there without being told, where the build
directory is `build`, and otherwise with `--compile-commands-dir=<build-dir>`, or `CompilationDatabase` in `.clangd`.
It is written as the project is configured, only when it changes, and removed when the project has no C.

## Compilers and flags

The compiler and the archiver must take gcc's options, as gcc and clang do. They, and the flags, are properties, each
of which also has a form for a target, such as `c/riscv64gc-unknown-none-elf/compiler`, which replaces it for the
libraries for that target, and is declared when it is first used, as a codegen option for a target is:

| Property | What it is | Without it |
|---|---|---|
| `c/compiler` | the compiler, a program or a list of it and its first arguments | `CC`, or `cc` |
| `c/archiver` | the archiver, as the compiler | `AR`, or `ar` |
| `c/flags` | the arguments of the compiler for C sources | `CFLAGS` |
| `asm/flags` | the arguments of the compiler for assembly sources | `ASFLAGS` |

Where neither is set, the property is given by the environment variable for the target, as the cc crate reads it,
`<variable>_<target>` with each character of the target which is not a letter or digit as `_`, such as
`CC_aarch64_unknown_linux_musl`, and for the host target alone, then by the variable itself, split at whitespace, and
then by `cc` or `ar`. So another target has no compiler unless one is set, which is the error `no-c-compiler`, as there
is no single right compiler for a target such as `riscv64gc-unknown-none-elf`. The variables are read only for the
libraries which need them, and the project is configured again when one which was read changes, as with `env`. A
compiler which is a path, containing a `/`, is relative to the project root.

<!-- jig-test
files: boot.S
-->
```jig
set c/riscv64gc-unknown-none-elf/compiler ['clang', '--target=riscv64-unknown-elf']
set c/riscv64gc-unknown-none-elf/archiver 'llvm-ar'
set asm/flags ['-march=rv64gc']

static-library boot
    source 'boot.S'
    target 'riscv64gc-unknown-none-elf'
    whole-archive
```

## Linking

A crate which uses a library is given it as rustc takes a native library, so that rustc orders the link:

- A library crate is given `-l static:-bundle=<name>`, by which it records in its metadata that what links it needs the
  library, without copying the archive into its rlib. So it neither depends on the archive nor is built again when the
  C changes.
- What links, a binary, a test harness or a procedural macro, is given `-l static=<name>` for each library it uses
  itself, and `-L native=<dir>` for the directory of each library which it, or a library crate it links, uses, through
  each other too, and depends on their archives.
- A library which `whole-archive` is given with `+whole-archive`.

rustc places a crate's native libraries after the crate in the link, so Rust calls C, and C calls a Rust function,
such as one with `#[unsafe(no_mangle)]`, of the crate which uses it. Checks, lints and documentation do not link, so
they do not wait for C.
