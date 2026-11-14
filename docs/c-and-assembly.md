# C and assembly

A plan for building C and assembly sources with jig, and linking them into Rust crates. **Not started.**

jig builds Rust crates with rustc, and anything else with a `custom` command. A project with C or assembly in it today
writes a `custom` command for each object, with no headers discovered, no cache unless it says `cache`, no compiler
identity, and links the objects by `codegen/link-arg`, which every crate that links has to repeat. That is what cargo's
`cc` crate does for cargo, from a build script. jig has no build scripts, so the support belongs in the configuration,
where the target, the profile and the linker already are.

Goals:

1. Assembly in a Rust-only project needs no C compiler.
2. C (`.c`) and assembly (`.s`, and `.S`, which is preprocessed) are compiled for any target a crate is, by a
   compiler which drives like gcc or clang, configured per target as the linker is.
3. Headers are discovered from depfiles, as rustc's sources are, so an edit to a header rebuilds what includes it.
4. Compiling C is cached, and can be shared between checkouts, as rustc is, with the compiler identified as the
   toolchain is.
5. A crate uses C code as it uses a crate, by `use`, and the objects reach the link of every program and test harness
   which needs them, in an order the linker accepts.
6. Nothing new is needed to build a project with no C in it: no `cc`, no `ar`, no new configure-time probes.

Not goals, for now: C++, MSVC's `cl.exe` and `lib.exe`, programs written only in C, and Rust libraries called from C
(`staticlib`). The design leaves room for each, in [later](#phase-5-later).

## What exists to build on

- **Edges with depfiles.** rig reads gcc's depfile format (`rig/depfile.rs`), as every rustc edge writes one, and a
  `discover` exec finds what an edge reads without running it, for workers.
- **Linker identity.** `identify_linkers` in `src/generate.rs` finds the linker of each crate which links, depends on
  it, hashes its path and `--version`, and gives that to the edge through `LINKER_VARIABLE`, so that a different linker
  is a different cache key. A compiler and an archiver are identified the same way.
- **Per-target settings.** A `target` statement already takes `linker`, `linker-flavor`, `runner` and `link-script`,
  with `override` for builtin targets, and a program which is a path is relative to the file.
- **Namespaced properties.** `codegen/<option>` and `codegen/<target>/<option>` are set for the project and replaced
  per target and per crate. C's flags follow the same shape.
- **Relocation.** Every rustc edge runs in the build directory with paths relative to it, and
  `--remap-path-prefix` makes its outputs independent of the checkout. gcc and clang have `-ffile-prefix-map`.
- **The transitive libraries of a link.** `transitive_libraries` already finds every library a linked crate needs,
  through others, which is where the native libraries they use are collected.

## Phase 0: assembly without a C compiler

rustc assembles with LLVM's assembler, for every target it has, so a `.s` file reaches a crate as

```rust
core::arch::global_asm!(include_str!("boot.s"));
```

and rustc's dep-info already lists `boot.s`, so the crate is rebuilt when it changes, and cached by it. This covers the
common case of jig's bare-metal users, a boot stub for a kernel with a `link-script`, with no new code.

- Document it in the README, beside `link-script`, with the kernel example.
- Add an integration test of a crate whose `include_str!`'d assembly is edited, which is rebuilt.
- Say what it does not cover: `.S` files, which need the C preprocessor, and assembly in GNU syntax which LLVM's
  assembler refuses. Those wait for phase 1.

## Phase 1: the `native` statement

A new statement declares a native library: C and assembly sources, compiled to objects and archived, for one or more
targets.

```
native uart
    source 'uart.c' 'boot.S'
    include 'include'
    define [UART_BASE: uart-base]

crate kernel 'kernel/main.rs'
    crate-type 'bin'
    use uart
```

### Attributes

- `source <path> ...`: the sources, as for a custom command's `input`, relative to the file, repeatable. The language
  is found by the extension: `.c` is C, `.S` and `.sx` are assembly which is preprocessed, and `.s` is assembly. Another
  extension is an error, `unknown-source-language`, until C++ is supported.
- `target <string>`: as for a crate, a target or a list of them, the `target` property by default. A native library is
  a read-only property `<target>/<name>`, as a crate is, so the two share the names of a target, and a name which is
  both is an error.
- `include <path> ...`: directories passed with `-I`, relative to the file, repeatable. A reference, such as to a
  custom command which generates a header, is passed as its directory and is a dependency of each compile.
- `define <defines>`: macros passed with `-D`, as a name, a list of names, or a dict of names to values, as `cfg` is.
- `flags <list>`: arguments passed to the compiler for every source, after those of the properties below.
- `whole-archive <bool>`: link every object, even one nothing refers to, as for the entry point of a kernel, which only
  the linker script names. `false` by default.
- `use <name>`: another native library for the same target, whose objects follow these in the link, and whose
  `include` directories these sources are compiled with too.
- `depends`, `pass-env`, `worker-tag`, `default`: as for a crate.

A native library's attributes are taken with `.`, as a crate's are: `source`, `target`, `include`, `define`, `output`
and the rest.

### Compilers and flags

A target statement takes two new attributes, which `override` takes too:

- `cc <program>`: the compiler, as a program or a list of it and its first arguments, as `runner` is, such as
  `['clang', '--target=riscv64-unknown-elf']`. It must take gcc's options: `-c`, `-o`, `-MD`, `-MF`, `-I`, `-D`,
  `-x` and `-ffile-prefix-map`.
- `ar <program>`: the archiver, which must take `crsD`.

The host target's are `cc` and `ar`. Another target has none, so a native library for it is an error,
`no-c-compiler`, at the library, naming the target statement to add. jig does not read `CC`, `CFLAGS` or `AR` from the
environment, as the cc crate does: the configuration reads them where it wants them, with `env('CC')`, so that what
configured the project is in the project.

jig does not guess a cross compiler's name from the target, as the cc crate does, as `riscv64gc-unknown-none-elf` has
no single right answer. A later phase may add a function which gives clang's `--target` for a builtin target, from
the `llvm-target` of `rustc --print target-spec-json`, which is unstable, so only where the toolchain is nightly.

Flags are properties, as codegen options are:

- `c/flags` and `c/<target>/flags`: arguments for C sources, the second replacing the first for a target.
- `asm/flags` and `asm/<target>/flags`: arguments for assembly sources, preprocessed or not.

From the crate's codegen options where the library is declared, jig derives `-O<n>` from `codegen/opt-level`,
including `s` and `z` as `-Os` and `-Oz`, and `-g` from a `codegen/debuginfo` which is not 0, so that `use
profile/release` reaches C too. The flags come after them, so they can replace them.

### What is built

Each source is compiled, in the build directory, as

```
<cc...> <derived> <flags> <include> <define> -ffile-prefix-map=<root>=. -MD -MF <object>.d -c <source> -o <object>
```

to `<build-dir>/<target>/native/<name>/<source path>.o`, so that sources of the same name in different directories
don't collide. The edge is `CC <object>`, or `AS <object>` for assembly. The objects are archived, as

```
<ar...> crsD <build-dir>/<target>/native/lib<name>.a <objects>
```

by an edge `AR <archive>`, which first removes the archive, as `ar r` adds to one which exists. `D` leaves the
timestamps and owners out, so the archive depends only on the objects.

Each compile edge is hashed by default, so that a header which is touched but not changed rebuilds nothing, and its
depfile gives the headers it includes. `-MD` lists system headers too, as absolute paths, which are then inputs of
the edge: they change only with the compiler, which is part of its identity anyway, so `-MMD`, which leaves them out,
is the alternative if their stats are slow. Measure both on the perf test.

### Linking into crates

A crate which `use`s a native library is given it in the form rustc already understands, so that rustc orders the
link:

- A library crate is given `-l static:-bundle=<name>`. Without `bundle`, the archive is not copied into the rlib, so
  the library is not rebuilt when the C changes, and does not depend on the archive. It only records, in its
  metadata, that what links it needs `lib<name>.a`. That keeps a library pipelined on metadata, as it is now.
- A crate which links, as a program, a test harness or a procedural macro, is given `-L native=<build-dir>/<target>/native`
  and depends on the archive of every native library used by it or by any library it links, found through
  `transitive_libraries`. A native library it uses itself is given with `-l static=<name>` too.
- `whole-archive` adds `+whole-archive` to the `-l` of the library.
- A native library's `use` of another is given to rustc as the other's `-l`, after its own, so a library's
  dependencies follow it in the link, as a single-pass linker needs.

rustc places each crate's native libraries after the crate in the link line, so Rust calling C links, and a native
library which calls a Rust function, `#[no_mangle]` in the crate which uses it, is placed after that crate too. A
cycle between C and a library crate is the one case which needs `--start-group`, which is left to `codegen/link-arg`.

Checks, lints and documentation don't link, so they don't depend on any of this, and `jig check` stays as fast as it
is for a project with C in it.

The `-bundle` modifier is stable since Rust 1.63, and `+whole-archive` since 1.61, well before the 1.97 jig needs.

### Diagnostics

- `unknown-source-language`: a source whose extension has no language.
- `no-c-compiler`: a native library for a target with no `cc`, or no `ar`.
- `native-target`: a crate uses a native library which is not declared for its target.
- `native-cycle`: native libraries which use each other.
- `newline-in-path` and the existing checks of outputs which overlap apply to objects and archives as to crates.

### Tests

The integration tests run without a POSIX shell, and a runner may have no C compiler, so most of them use the test
helper as the compiler and the archiver: it takes `-MF`, `-o` and `crsD`, writes a depfile listing the headers its
source names, and writes its output. They test the shape of the graph through `jig graph`, that an edited header
rebuilds only what includes it, that a library crate is not rebuilt when its C changes, and the diagnostics.

A few tests, tagged `cc`, build a real program with C and `.S` sources with the host's `cc` and `ar`, and run it. CI's
Linux and macOS runners have both, and the Windows job has MinGW's, as `x86_64-w64-mingw32-gcc` and
`x86_64-w64-mingw32-ar`, which its `Jig.local` sets as the target's `cc` and `ar`.

## Phase 2: identity and the cache

- **Identifying tools.** Generalise `identify_linkers` to identify each compiler and archiver as it identifies a
  linker: by its canonical path and its `--version`, depended on by the configuration, so that it is configured again
  when the tool is replaced. Without it, a cached object would survive an upgrade of the compiler.
- **Caching.** Compile edges are cached by default, as rustc's are, as their depfiles make their inputs complete. The
  cache key is the action, which holds every flag, and the identity of the compiler in place of the toolchain's. The
  archive edge is cheap and is not cached, but is hashed, so an object restored unchanged does not relink.
- **Relocation.** `-ffile-prefix-map=<root>=.` makes debug info and `__FILE__` relative to the project root, as
  `--remap-path-prefix` does for rustc. An absolute `include` keeps the edge from being shared between checkouts, as
  an absolute path in rustc's arguments does today.
- **Hermetic environment.** A cached compile inherits only `pass-env` and the variables which locate tools. gcc reads
  `CPATH`, `C_INCLUDE_PATH`, `GCC_EXEC_PREFIX` and `COMPILER_PATH`, and clang some of them, so a project which relies on
  one passes it by name.
- **Workers.** Compiles can run on workers with the same `worker-tag` rules as rustc, as their `discover` exec is the
  compile with `-M -MF <depfile>` in place of `-c -o`. A worker must have a compiler of the same identity, which it
  reports as it reports a toolchain. This is the last part of the phase, and can be dropped if workers with C
  compilers are rare.

## Phase 3: editors

- Write `compile_commands.json` to the build directory beside `.rust-project.json`, with each source's command and
  `"directory"` the build directory, so clangd finds the includes and defines of each file.
- `jig check` compiles C with `-fsyntax-only`, so that C's errors are shown where Rust's are.

## Phase 4: plain objects and kernels

A kernel often needs an object linked as it is, rather than from an archive, as with an entry point which must come
first in the image. `whole-archive` covers most of it. What's left:

- `link 'objects'` on a native library, which passes its objects with `-C link-arg=<object>` to each crate which links
  it, in the order of its sources, with no archive. They follow the Rust objects, so this is only right where nothing
  in C calls into a library crate.
- An example in the README of a RISC-V kernel with a boot stub in `.S`, a linker script from a `file` statement, and a
  runner of QEMU, built with clang as `cc`.

## Phase 5: later

- **C++**, by `.cc`, `.cpp` and `.cxx`, with a `cxx` compiler for the target and `c++/flags`, and the C++ runtime
  linked by a `link-cxx-runtime` attribute.
- **Programs in C**, as `native <name>` with `kind 'bin'`, linked by the target's `cc`.
- **Rust for C**, a `staticlib` crate type, archived by rustc, which a native library or a C program uses.
- **An archiver in jig.** Archives are a simple format, so jig could write them itself, as `jig archive`, and not need
  `ar` at all. The GNU and BSD formats need a symbol table, which needs the symbols of ELF, Mach-O and COFF objects
  read. Worth it only if finding an `ar` for each target turns out to be the main source of friction.
- **MSVC**, whose `cl.exe` takes different options and writes no depfile, but `/sourceDependencies` JSON.
- **Cross-language LTO**, with clang's `-flto=thin` and `codegen/linker-plugin-lto`, where clang's LLVM matches
  rustc's.

## Risks

- **The variety of compilers.** gcc and clang agree on the options here, but not on every flag a user passes. jig
  only promises the options it passes itself.
- **Absolute paths in depfiles.** A depfile listing system headers makes every compile stat them. If that is
  measurable on the perf test, switch to `-MMD`.
- **rustc's ordering of native libraries.** It is the documented behaviour of `-l`, but a project with C which calls
  back into Rust across crates may need `--start-group`. The phase 1 tests include one such project, to find out.
- **Archive names.** A `lib<name>.a` in a crate's search path is searched before the system's, so a native library
  named `m` or `c` would shadow libm or libc. Make those names an error.

## Decisions to make

1. **Statement name.** `native`, as above, matching rustc's `-L native=` and the term the Rust reference uses.
   Alternatives: `c`, which is wrong for assembly and C++, or `library`, which is confused with a crate's.
2. **Archives by default, or objects.** Archives, as above: rustc orders them, deduplicates them, and leaves out
   members nothing refers to, as a C library expects. Objects (phase 4) are opt-in.
3. **A compiler for non-host targets.** None by default, rather than guessing one from the target's name.
4. **Reading `CC` and `CFLAGS`.** Not read: the configuration reads them where it wants them.
