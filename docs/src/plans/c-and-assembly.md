# C and assembly

A plan for building C and assembly sources with jig, as static libraries, shared libraries and executables, and
linking them into Rust crates. **Phases 0 to 3 are done**; the others are not started.

jig builds Rust crates with rustc, and anything else with a `custom` command. A project with C or assembly in it today
writes a `custom` command for each object, with no headers discovered, no cache unless it says `cache`, no compiler
identity, and links the objects by `codegen/link-arg`, which every crate that links has to repeat. That is what cargo's
`cc` crate does for cargo, from a build script. jig has no build scripts, so the support belongs in the configuration,
where the target, the profile and the linker already are.

Goals:

1. Assembly in a Rust-only project needs no C compiler.
2. C (`.c`) and assembly (`.s`, and `.S`, which is preprocessed) are compiled for any target a crate is, by a
   compiler which drives like gcc or clang, configured per target as codegen options are.
3. Headers are discovered from depfiles, as rustc's sources are, so an edit to a header rebuilds what includes it.
4. Compiling C is cached, and can be shared between checkouts, as rustc is, with the compiler identified as the
   toolchain is.
5. A crate uses a C library as it uses a crate, by `use`, and the library reaches the link of every program and test
   harness which needs it, in an order the linker accepts.
6. Static libraries, shared libraries and executables are each declared by a statement of their own.
7. Any library name is valid, including `c` and `m`, so a project can build its own or a cross C library.
8. Nothing new is needed to build a project with no C in it: no compiler, no archiver, no new configure-time probes.

Not goals, for now: C++, MSVC's `cl.exe` and `lib.exe`, and Rust libraries called from C (`staticlib`). The design
leaves room for each, in [later](#phase-6-later).

## What exists to build on

- **Edges with depfiles.** rig reads gcc's depfile format (`rig/depfile.rs`), as every rustc edge writes one, and a
  `discover` exec finds what an edge reads without running it, for workers.
- **Linker identity.** `identify_linkers` in `src/generate.rs` finds the linker of each crate which links, depends on
  it, hashes its path and `--version`, and gives that to the edge through `LINKER_VARIABLE`, so that a different linker
  is a different cache key. A compiler and an archiver are identified the same way.
- **Namespaced properties.** `codegen/<option>` and `codegen/<target>/<option>` are set for the project and replaced
  per target and per crate. C's compiler and flags follow the same shape.
- **Environment variables.** `env()` and `pass-env` already make the project configure again, or a command run again,
  when a variable changes, so defaults taken from `CC` and `CFLAGS` are tracked like any other.
- **Relocation.** Every rustc edge runs in the build directory with paths relative to it, and
  `--remap-path-prefix` makes its outputs independent of the checkout. gcc and clang have `-ffile-prefix-map`.
- **The transitive libraries of a link.** `transitive_libraries` already finds every library a linked crate needs,
  through others, which is where the C libraries they use are collected.
- **Running programs.** `jig run`, `jig test` and a target's `runner` already decide how a program is run, which is
  where a shared library's directory is added to the search path.

## Phase 0: assembly without a C compiler (done)

rustc assembles with LLVM's assembler, for every target it has, so a `.s` file reaches a crate as

```rust
core::arch::global_asm!(include_str!("boot.s"));
```

and rustc's dep-info already lists `boot.s`, so the crate is rebuilt when it changes, and cached by it. This covers the
common case of jig's bare-metal users, a boot stub for a kernel with a `link-script`, with no new code.

- [Generated files](../language/files.md) documents it beside `link-script`, with the kernel example.
- `assembly_in_a_crate`, an integration test, runs a program whose byte is defined in `include_str!`'d assembly,
  which is not built again when nothing changed, and is built again with the new byte when the assembly is edited.
  The byte is labelled both with and without a leading `_`, and is in `.data`, so the same file links on ELF, Mach-O
  and COFF, and on every host CI runs, without a C compiler.
- It says what it does not cover: `.S` files, which need the C preprocessor, and assembly in GNU syntax which LLVM's
  assembler refuses. Those wait for phase 1.

## The statements

Three statements declare what is built from C and assembly:

- `static-library <name>`: an archive, `lib<name>.a`, which crates, shared libraries and executables link.
- `shared-library <name>`: a shared library, `lib<name>.so`, `lib<name>.dylib` or `<name>.dll`, which they link and
  load when they run.
- `executable <name>`: a program, linked by the C compiler, which `jig run` runs and a test can run.

```
static-library uart
    source 'uart.c' 'boot.S'
    include 'include'
    define [UART_BASE: uart-base]

crate kernel 'kernel/main.rs'
    crate-type 'bin'
    use uart

shared-library codec
    source 'codec/*.c'

executable codec-cli
    source 'cli.c'
    use codec
    default
```

Each is a read-only property `<target>/<name>`, as a crate is, whose value is a reference to its output, and its
attributes are taken with `.`. Crates and the three share the names of a target, so a name which is two of them is an
error. A name is a crate name, so `c`, `m` and `gcc` are as valid as any other.

### Attributes

All three take:

- `source <path> ...`: the sources, as for a custom command's `input`, relative to the file, repeatable, where a
  reference is the output of a custom command which generates a source. The language is found by the extension: `.c` is
  C, `.S` and `.sx` are assembly which is preprocessed, and `.s` is assembly. Another extension is an error,
  `unknown-source-language`, until C++ is supported.
- `target <string>`: as for a crate, a target or a list of them, the `target` property by default.
- `include <path> ...`: directories passed with `-I`, relative to the file, repeatable. A reference to a custom command
  which generates a header is passed as its directory, and is a dependency of each compile.
- `define <defines>`: macros passed with `-D`, as a name, a list of names, or a dict of names to values, as `cfg` is.
- `flags <list>`: arguments passed to the compiler for every source, after those of the properties below.
- `use <name>`: a static or shared library for the same target, which is linked after this one, and whose
  `public-include` directories these sources are compiled with. It can be repeated, or a list, as for a crate.
- `public-include <path> ...`: directories which what uses the library is compiled with too, as its headers.
- `depends`, `pass-env`, `worker-tag`, `default`, `install`, `install-dir`, `install-mode`: as for a crate.

A static library also takes `whole-archive <bool>`, to link every object, even one nothing refers to, as for the
entry point of a kernel, which only the linker script names. A shared library and an executable also take
`link-flags <list>`, passed to the compiler as it links, and `link-script`, as a crate does.

### Compilers, archivers and flags

They are properties, as codegen options are, each with a form for a target which replaces it, such as
`c/riscv64gc-unknown-none-elf/compiler`, which can be set before the target is declared:

| Property | What it is | Default, from the environment |
| --- | --- | --- |
| `c/compiler` | the compiler, a program or a list of it and its first arguments | `CC`, or `cc` |
| `c/archiver` | the archiver, as the compiler | `AR`, or `ar` |
| `c/flags` | arguments for C sources | `CFLAGS`, split at whitespace, or none |
| `asm/flags` | arguments for assembly sources, preprocessed or not | `ASFLAGS`, or none |
| `c/link-flags` | arguments for linking a shared library or an executable | `LDFLAGS`, or none |

For a target, the default of `c/<target>/<property>` is the variable for the target, as the `cc` crate reads them,
`CC_<target>` with the target's `-` and `.` as `_`, such as `CC_aarch64_unknown_linux_musl`, and only the host target
falls back to the variable without a target and then to `cc` or `ar`. So a cross compiler is set by the environment, or
in `Jig.local`:

```
set c/aarch64-unknown-linux-musl/compiler 'aarch64-linux-musl-gcc'
set c/riscv64gc-unknown-none-elf/compiler ['clang', '--target=riscv64-unknown-elf']
set c/riscv64gc-unknown-none-elf/archiver 'llvm-ar'
```

The variables are read as `env()` reads them, so the project is configured again when one changes. They are read only
when something is built from C for the target, so a Rust-only project does not depend on `CC`. A library for a target
with no compiler is an error, `no-c-compiler`, at the statement, naming the property to set.

The compiler must take gcc's options: `-c`, `-o`, `-MD`, `-MF`, `-I`, `-D`, `-fPIC`, `-shared` and
`-ffile-prefix-map`, and the archiver must take `crsD`. jig does not guess a cross compiler's name from the target,
as `riscv64gc-unknown-none-elf` has no single right answer. A later phase may add a function which gives clang's
`--target` for a builtin target.

From the codegen options where a statement is, jig derives `-O<n>` from `codegen/opt-level`, including `s` and `z` as
`-Os` and `-Oz`, and `-g` from a `codegen/debuginfo` which is not 0, so that `use profile/release` reaches C too. The
flags properties, then the statement's `flags`, come after them, so they can replace them.

### What is built

Each source is compiled, in the build directory, as

```
<compiler...> <derived> <flags> <include> <define> -ffile-prefix-map=<root>=. -MD -MF <object>.d -c <source> -o <object>
```

with `-fPIC` for a shared library, to `<build-dir>/<target>/c/<name>/<source path>.o`, so that sources of the same
name in different directories don't collide. The edge is `CC <object>`, or `AS <object>` for assembly. Each compile
edge is hashed, so a header which is touched but not changed rebuilds nothing, and its depfile gives the headers it
includes.

Each library or executable has its own directory, `<build-dir>/<target>/c/<name>/`, which holds its output beside its
objects:

- a static library, `lib<name>.a`, by `AR`: `<archiver...> crsD <archive> <objects>`, removing the archive first, as
  `ar r` adds to one which exists. `D` leaves the timestamps and owners out, so it depends only on the objects.
- a shared library, by `LINK`: `<compiler...> -shared <objects> <libraries> <link-flags> -o <output>`, with
  `-Wl,-soname,lib<name>.so` on ELF, `-dynamiclib -install_name @rpath/lib<name>.dylib` on Apple's targets, and on
  Windows, `-Wl,--out-implib,lib<name>.dll.a`, its import library, which is what links it.
- an executable, by `LINK`: `<compiler...> <objects> <libraries> <link-flags> -o <output>`, with `.exe` on Windows.

### Library names and search directories

A library is found by the linker by its name in a search directory, so a project's `libc.a` would shadow the
system's C library in every link given its directory. That is why each library has a directory of its own: a link is
given the directory of each library it uses, and only those. So a static library named `c` replaces the system's C
library exactly where it is used, as a custom or cross C library should, and nowhere else. Its directory comes before
the system's, as `-L` does, so it is the one found.

Where a project's library and a system library of the same name are both wanted in one link, which one the linker
takes is its choice, not jig's; the `name-shadows-system` note says so when a library is named after one the target's
standard library links, such as `c`, `m`, `pthread` or `gcc_s`, and can be allowed.

### Linking into crates

A crate which `use`s a library is given it in the form rustc already understands, so that rustc orders the link:

- A library crate is given `-l static:-bundle=<name>` for a static library, or `-l dylib=<name>` for a shared one.
  Without `bundle`, the archive is not copied into the rlib, so the crate is not rebuilt when the C changes, and does
  not depend on the archive. It only records, in its metadata, that what links it needs the library. That keeps a
  library pipelined on metadata, as it is now.
- A crate which links, as a program, a test harness or a procedural macro, is given `-L native=<library's directory>`
  for every library used by it or by any library crate it links, found through `transitive_libraries`, and depends on
  their outputs. A library it uses itself is given with `-l` too.
- `whole-archive` adds `+whole-archive` to the `-l` of a static library.
- A library's `use` of another is given to rustc as the other's `-l`, after its own, so a library's dependencies
  follow it in the link, as a single-pass linker needs.

rustc places each crate's native libraries after the crate in the link line, so Rust calling C links, and a library
which calls a Rust function, `#[no_mangle]` in the crate which uses it, is placed after that crate too. A cycle between
C and a library crate is the one case which needs `--start-group`, which is left to `codegen/link-arg`.

Checks, lints and documentation don't link, so they don't depend on any of this, and `jig check` stays as fast as it
is for a project with C in it.

The `-bundle` modifier is stable since Rust 1.63, and `+whole-archive` since 1.61, well before the 1.97 jig needs.

### Running what uses a shared library

What links a shared library finds it when it runs by the platform's search path, as nothing is given an rpath by
default, so that what is built is the same wherever the build directory is, and is cached by it:

- `jig run`, `jig test` and a test whose command is an executable or a crate add the directories of the shared
  libraries the program links to `LD_LIBRARY_PATH`, `DYLD_LIBRARY_PATH`, or on Windows `PATH`, as cargo does.
- `jig install` installs a shared library in `lib`, and an executable or a crate which links one is given
  `-Wl,-rpath,$ORIGIN/../lib`, or `@loader_path/../lib`, by an `rpath` attribute, which is off by default, as the
  layout of an installation is the project's choice.

## Phases

### Phase 1: static libraries (done)

`static-library`, its attributes, the `c/` and `asm/` properties with their defaults from the environment, compiling
and archiving, `use` from crates and from each other, and attributes taken with `.`, as
[static libraries](../language/static-libraries.md) describes. What was built differs from the plan above in places:

- **Diagnostics.** `unknown-source-language`, `no-c-compiler`, `redeclare-library` (a library declared again, or with
  a crate's name), `unknown-static-library` (one library using another which isn't declared for its target),
  `library-cycle`, and the note `name-shadows-system`. A crate which uses a name which is neither a crate nor a static
  library for its target is still `unknown-crate`, rather than a `c-target` of its own.
- **Stale members.** `ar` adds to an archive which is there, and rig has no step before a command, so rather than the
  archive edge removing it, configuring records the objects of each library beside its archive, in
  `lib<name>.a.members`, and removes the archive when they change, which only configuring can change.
- **Determinism on macOS.** Apple's `ar` does not take `D`, so on macOS hosts the archiver is given `crs`, with
  `ZERO_AR_DATE=1`, which does the same.
- **Relocation** came early: each compile is given `-ffile-prefix-map`, as phase 2 planned, as it is cheap and the
  graph shows it.
- **Hashing.** Compile and archive edges are stamped by contents whatever `change-detection` is, so a header touched
  but not changed, or changed only in a comment, archives and links nothing again.
- **Left for later.** `worker-tag`, which is for phase 2's workers; `install`; and an `include` which is a reference
  to a generated header being a dependency of each compile, which `depends` covers until then.

The integration tests use the test helper as the compiler and the archiver, as its `cc` and `ar` steps, which write a
depfile of the headers a source includes, and an archive which replaces members by name, as `ar r` does, so that most
tests need neither:

- `static_library_graph`: the commands, flags, include directories, `-l` and `-L` of each edge.
- `static_library_rebuilds`: an edited header compiles only what includes it and archives again, without building the
  library crate which uses it again, and a removed source leaves the archive.
- `static_library_tools_from_the_environment`: `CC`, `AR`, `CFLAGS`, `ASFLAGS` and `CC_<target>`, and the properties
  which replace them.
- `static_library_errors`: the diagnostics.
- `static_library_links`, on Unix where `cc` and `ar` are installed: a program which calls C through a library crate,
  from two libraries, one using the other, links and runs.

Not yet tested: linking on Windows with MinGW, which the Windows job could do by setting
`c/x86_64-pc-windows-gnu/compiler` and `c/x86_64-pc-windows-gnu/archiver`, and a library named `c` given only to the
links which use it, beyond `-L` being per library in `static_library_graph`.

### Phase 2: identity and the cache (done)

- **Identifying compilers.** Once caching is on, each library's compiler is identified by the BLAKE3 hash of its
  program and what the whole command prints with `--version`, so `ccache gcc` is identified by gcc's version as well
  as by ccache. The program is found in `PATH` as a cached custom command's is, so the configuration depends on `PATH`
  and the program, and is configured again when either changes. A compiler which is not found is not cached. The
  archiver is not identified, as the archive is not cached.
- **Caching.** Compile edges are cached by the compiler's identity, in place of the toolchain's, with their depfiles
  completing their inputs. The archive edge is cheap and is not cached, but is hashed, so an object restored unchanged
  does not relink.
- **Relocation** came in phase 1, with `-ffile-prefix-map`. rig replaces the absolute build directory in it with a
  marker in the key, as it does rustc's `--remap-path-prefix`, so a compile is shared between checkouts. System
  headers, which `-MD` lists as absolute paths, are part of the second key, by their contents, as the sysroot's files
  are for rustc. `-MMD` was not needed.
- **Hermetic environment.** A cached compile inherits only `pass-env` and the variables which locate tools, which the
  book says, naming `CPATH`.
- **Workers: dropped for now.** A worker runs an edge whose identity is a toolchain it reports, and no worker reports
  a C compiler, so compiles stay on the machine which builds, as cached custom commands do. Sending them needs workers
  to report the compilers they have, by the same identity, which can wait for someone with workers and C.

`static_library_cache` tests it with a copy of the test helper as the compiler, which logs each compile in the build
directory: an object which is removed is fetched rather than compiled, an edited header compiles again, another
checkout fetches every object, and a changed compiler configures the project again and compiles. A compile with the
host's `cc`, wrapped in a script which logs, was checked by hand to be fetched once its outputs were removed, with
system headers in its depfile.

### Phase 3: executables (done)

`executable`, as [executables](../language/executables.md) describes: compiled as a static library is, and linked by
the compiler, with the archives of the static libraries it uses by their paths, in the order a single-pass linker
needs, `whole-archive` as `--whole-archive` or `-force_load`, `c/link-flags` from `LDFLAGS` and its own `link-flags`
after them, its linker script or its target's, and `.exe` on Windows. `jig run` runs one by name, through its target's
runner, a `test` or custom command runs one by the property with its name, and `jig install` installs one in `bin`.

As built, it differs from the plan above in places:

- **One item, two kinds.** A static library and an executable are one `Native` with a kind, sharing their attributes,
  their compiling and the names of a target, so a static library and an executable cannot have the same name either,
  which `redeclare-library` reports, naming which it is.
- **Using an executable** is `use-executable`, from a crate, a library or an executable.
- **Link flags come last**, after the objects and archives, rather than among them, as `LDFLAGS` often has `-l`, which
  finds only what is needed by what comes before it.
- **The link is cached** by the compiler's identity, as rustc's link is by the toolchain's and the linker's. The
  system's libraries it links, such as libc, are not its inputs, as they are not rustc's, so an update of them alone
  does not link again, as it does not for a crate.

Tests: `executable_graph`, with the test helper as the compiler and archiver, checks the compile and link commands,
the order of archives, `whole-archive`, the link flags, the target's linker script, attributes taken with `.`, and that
it is built by default. `executable_errors` checks the diagnostics, and `executable_runs`, on Unix where `cc` and `ar`
are installed, runs one with `jig run`, with its arguments and exit code, by a test, and installed by `jig install`.

### Phase 4: shared libraries

`shared-library`, with `-fPIC`, the soname, install name or import library of each platform, `-l dylib=` for crates,
`use` from executables and other shared libraries, the search path set by `jig run` and `jig test`, and `rpath`. A
shared library is only for a target which has them, so one for a target without an operating system is an error.

### Phase 5: editors and kernels

- Write `compile_commands.json` to the build directory beside `.rust-project.json`, with each source's command and
  `"directory"` the build directory, so clangd finds the includes and defines of each file.
- `jig check` compiles C with `-fsyntax-only`, so that C's errors are shown where Rust's are.
- An example in the book, beside the kernel's linker script, of a RISC-V kernel with a boot stub in `.S`, a linker
  script from a `file` statement, its own `static-library c`, and a runner of QEMU, built with clang.

### Phase 6: later

- **C++**, by `.cc`, `.cpp` and `.cxx`, with `c++/compiler` from `CXX`, `c++/flags` from `CXXFLAGS`, and the C++
  runtime linked by a `link-cxx-runtime` attribute.
- **Rust for C**, a `staticlib` and `cdylib` crate type, which an executable or a library uses.
- **An archiver in jig.** Archives are a simple format, so jig could write them itself, as `jig archive`, and not need
  an archiver at all. The GNU and BSD formats need a symbol table, which needs the symbols of ELF, Mach-O and COFF
  objects read. Worth it only if finding an archiver for each target turns out to be the main source of friction.
- **Plain objects**, by a `link 'objects'` on a static library, passed with `-C link-arg=<object>` in the order of its
  sources, where an object must be linked as it is.
- **MSVC**, whose `cl.exe` takes different options and writes no depfile, but `/sourceDependencies` JSON.
- **Cross-language LTO**, with clang's `-flto=thin` and `codegen/linker-plugin-lto`, where clang's LLVM matches
  rustc's.

## Risks

- **The variety of compilers.** gcc and clang agree on the options here, but not on every flag a user passes. jig
  only promises the options it passes itself.
- **`CFLAGS` from the environment.** A developer's `CFLAGS` changes what is built, and its cache key, without the
  project saying so. It is what a C developer expects, and `jig generate` reports the variables it read, but a project
  which wants to be independent of them sets the properties itself.
- **Absolute paths in depfiles.** `-MD` lists system headers, as absolute paths, which every compile then stats. They
  change only with the compiler, which is part of its identity, so if that is measurable on the perf test, switch to
  `-MMD`.
- **rustc's ordering of native libraries.** It is the documented behaviour of `-l`, but a project with C which calls
  back into Rust across crates may need `--start-group`. The phase 1 tests include one such project, to find out.
- **A library which replaces a system one.** A project's `c` comes first in its links, but the target's standard
  library may also link `-lc` itself, with its own expectations of what it finds. That is the project's to get right,
  as it would be with any build system, and the kernel example in phase 5 shows it working.

## Decisions

1. **Statements.** `static-library`, `shared-library` and `executable`, one for each kind of output, rather than one
   statement with a kind.
2. **Compilers and flags from the environment.** `CC`, `AR`, `CFLAGS`, `ASFLAGS` and `LDFLAGS`, and their forms for a
   target, give the defaults of the `c/` and `asm/` properties, which the configuration can replace.
3. **Any library name.** `c` and `m` are valid, and each library has a directory of its own, so it is only found by
   the links which use it.
4. **Static libraries are archives.** rustc orders them, deduplicates them, and leaves out members nothing refers
   to, as a C library expects. Plain objects are later, and opt-in.
5. **No compiler for another target** unless one is set, rather than guessing one from the target's name.
6. **No rpath by default.** jig sets the search path where it runs a program, and `rpath` is for installation.
