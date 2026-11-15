# Generated files

A `file` statement is followed by indented attributes, which give the contents of the file, as it is written when the
project is configured, rather than by rig. It is only written when its contents change, so that what uses it is only
built again then, and is written again if it is removed. Like a custom command, it is a property with its name, whose
value is a reference to it, so it can be an input, or a `link-script`. Its output cannot overlap another output.

- `content <str>`: the contents, which are usually a multi-line string, where braces must be doubled. The file ends with
  a newline, which the last line of a multi-line string does not have, unless it is empty
- `output <path>`: the file, relative to the build directory, `<build-dir>/<name>` by default
- `mode <mode>`: the permissions of the file in the build directory, as for `install-mode`, such as `'+x'` for a
  script, which are `0o644` by default, and are set again when the project is configured if they have been changed
- `install`, `install-dir` and `install-mode`: as for a custom command, where the file is installed in `share` by
  default, with its `mode`, which a symbolic `install-mode` changes. A file whose installed permissions are executable
  is a program for `jig install --proxy`

<!-- jig-test
declare kernel-base
    default 0x8000_0000
-->
```jig
file kernel-link
    output 'kernel.x'
    content '''
        ENTRY(_start)
        SECTIONS {{
            . = {kernel-base};
        }}
    '''

crate kernel 'kernel/main.rs'
    crate-type 'bin'
    link-script kernel-link
```

Assembly needs no C compiler, as rustc assembles it with LLVM's assembler, for any target it has: a `.s` file is
included in a crate with `global_asm!`, such as the kernel's entry point, `_start`, which its linker script names.
rustc lists the file in the crate's dep-info, so the crate is built again when it changes, and is cached by its
contents, as the crate's own sources are. The file is in LLVM's syntax for the target, which is GNU's, but for x86,
where it is Intel's unless it starts with `.att_syntax`. A `.S` file, which the C preprocessor reads first, needs a C
compiler, as a `custom` command.

```rust
// kernel/main.rs
#![no_std]
#![no_main]
core::arch::global_asm!(include_str!("boot.s"));
```
