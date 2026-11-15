# Executables

An `executable` statement declares a program of C and assembly, which is compiled as a
[static library](static-libraries.md) is, with the same compiler, flags and [cache](../caching.md), and linked by the
compiler. It takes the attributes of a static library but `whole-archive`, where `use` names the static and
[shared libraries](shared-libraries.md) it links, and these:

- `link-flags <arguments>`: arguments of the compiler as it links, after those of `c/link-flags`, as a `str`, which is
  one, or a list of them. It can be repeated
- `link-script <path>`: the linker script, which replaces that of its [target](targets.md), as for a crate
- `rpath <dirs>`: the directories, as a `str` or a list of them, in which it looks for the shared libraries it links as
  it runs, as `-Wl,-rpath,<dir>`, such as `'$ORIGIN/../lib'`, or `'@loader_path/../lib'` on macOS, so that an installed
  program finds those installed beside it
- `install <bool>`, `install-dir <str>` and `install-mode <mode>`: install the program with
  [`jig install`](../commands/install.md), as for a binary crate, in `bin` by default

The program is `<build-dir>/<target>/<name>`, with `.exe` for Windows, where a binary crate's is, and an executable and a
crate cannot have the same name for a target. [`jig run`](../commands/run.md) runs it by its name, as it runs a binary
crate, with `<name>@<target>` for a target other than the host's, and a test or a custom command runs it by the
property with its name, which builds it first.

<!-- jig-test
files: hello.c greeting.c include/greeting.h
-->
```jig
static-library greeting
    source 'greeting.c'
    public-include 'include'

executable hello
    source 'hello.c'
    use greeting
    link-flags '-lm'
    install

test hello
    command hello '--version'
```

## Linking

An executable is linked as

```
<compiler> <object> ... <library> ... <c/link-flags> <link-flags> -Wl,-rpath,<dir> ... -T<link script> -o <program>
```

where each library it uses, through each other too, is given by the path of its archive or shared library, but for the
static libraries in a shared library, which are not linked again, each before those it uses, as a linker which reads
each archive once needs. A library with `whole-archive` is wrapped in `-Wl,--whole-archive` and
`-Wl,--no-whole-archive`, or given as `-Wl,-force_load,<archive>` on Apple's targets. The
link flags come after what they may need, as `-l` finds only what is needed by what comes before it. `c/link-flags` is
`LDFLAGS`, or `LDFLAGS_<target>`, by default, as [the other properties](static-libraries.md#compilers-and-flags) are.

The link is cached by the identity of the compiler, as its compiles are, and is stamped by the contents of what it
reads, so an object or archive which is built again unchanged links nothing again.
