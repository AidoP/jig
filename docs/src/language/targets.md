# Targets

A `target` statement is followed by indented attributes, which describe the target with a JSON target specification,
`<build-dir>/targets/<name>.json`. Custom targets need a nightly toolchain.

- `inherit <target>`: the builtin target whose specification it is based on
- `override`: set the attributes of the builtin target with the name, rather than declaring a target. Only `test`,
  `runner`, `link-script`, `linker` and `linker-flavor` can be used with it, as the builtin target keeps its specification. The linker
  and its flavor are passed to rustc as codegen options of each crate for the target, unless the crate has its own
  `codegen/linker` or `codegen/linker-flavor`, and a linker which is a path is relative to the file, as a runner is.
  `rust-lld`, which rustup installs with the toolchain, cross-links a musl target, such as
  `aarch64-unknown-linux-musl` on `x86_64-unknown-linux-gnu`, as it has its own C runtime, while a glibc target also
  needs that target's C libraries
- `env`, `os`, `arch`, `linker <str>`: replace the field of the specification
- `code-model`, `relocation-model`, `target-cpu`, `linker-flavor`, `panic <str>`: replace the field of the
  specification for the codegen option
- `target-feature <features>`: the features of the target, as a `str` separated by `,`, or a list of them
- `no-redzone`: disable the red zone
- `link-script <path>`: the linker script of the crates for the target, as a path relative to the file, or a reference
  to a crate or custom command with one output, such as one which generates it. It is passed to the linker with
  `-C link-arg=-T<path>`, rather than written to the specification, when a binary, procedural macro or test harness is
  built, which depends on it, unless the crate has its own `link-script`
- `test <bool>`: whether crates for the target can be tested, which by default is its `can-run` for a builtin target,
  or, if there are workers, `true`, as their tests run on a worker which runs the target, and `false` for a custom
  target. Without a value, it is `true`, and with `'workers'`, the tests of its crates run on workers, even where the
  host can run them
- `runner <program>`: the program which runs a test harness for the target, given as its last argument, or a list of the
  program and its first arguments, as for the `command` of a custom command. As with `run`, a program containing a `/`
  is a path relative to the file, while a reference, such as to a crate or a custom command, is its output, which is
  built before the tests. Without it, a test harness is run directly

```
target riscv64gc-unknown-none-elf
    override
    test
    runner ['qemu-system-riscv64', '-machine', 'virt', '-nographic', '-kernel']

target aarch64-unknown-linux-musl
    override
    linker 'rust-lld'

target bluemetal-riscv64
    inherit 'riscv64gc-unknown-none-elf'
    os 'bluemetal'
    code-model 'medium'
    link-script 'link.x'
```
