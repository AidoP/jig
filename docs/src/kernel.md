# A kernel in Rust, C and assembly

A kernel for RISC-V, which QEMU runs, shows what jig builds without an operating system: Rust which calls C, its own C
library, assembly at its entry point, and a linker script written by the configuration. It needs the standard library
of the target, from `rustup target add riscv64gc-unknown-none-elf`, clang and llvm-ar, which build C for any target
LLVM has, and `qemu-system-riscv64`.

<!-- jig-test
files: boot.S c/string.c c/uart.c c/include/c.h kernel/main.rs
-->
```jig
set target 'riscv64gc-unknown-none-elf'
set codegen/opt-level 's'
set c/{target}/compiler ['clang', '--target=riscv64-unknown-elf', '-march=rv64gc', '-mabi=lp64d', '-mcmodel=medany', '-ffreestanding']
set c/{target}/archiver 'llvm-ar'

target riscv64gc-unknown-none-elf
    override
    runner ['qemu-system-riscv64', '-machine', 'virt', '-nographic', '-bios', 'none', '-kernel']

file kernel-link
    output 'kernel.x'
    content '''
        ENTRY(_start)
        SECTIONS {{
            . = 0x80000000;
            .text : {{ *(.text.boot) *(.text .text.*) }}
            .rodata : {{ *(.rodata .rodata.*) *(.srodata .srodata.*) }}
            .data : {{ *(.data .data.*) *(.sdata .sdata.*) }}
            .bss : {{ *(.bss .bss.*) *(.sbss .sbss.*) }}
            . = ALIGN(16) + 0x4000;
            stack_top = .;
        }}
    '''

static-library boot
    source 'boot.S'
    whole-archive

static-library c
    source 'c/string.c' 'c/uart.c'
    public-include 'c/include'

crate kernel 'kernel/main.rs'
    crate-type 'bin'
    use boot
    use c
    link-script kernel-link
```

The crates of the project, and its [static libraries](language/static-libraries.md), are for the target, as the
`target` property is, and C for it is compiled by clang for the same machine and ABI as Rust's, which
`c/<target>/compiler` gives with its first arguments. The target's runner is QEMU, which [`jig run`](commands/run.md)
gives the kernel to. The [linker script](language/files.md) is written as the project is configured, from a template,
and puts the entry point first, at the address QEMU jumps to.

The entry point is in `boot.S`, which sets up the stack and calls the kernel. Nothing refers to it but the linker
script's `ENTRY`, so its library is `whole-archive`, which links every object of it, as a library's objects are
otherwise linked only when something refers to them:

```
    .section .text.boot
    .globl _start
_start:
    la sp, stack_top
    call kmain
1:  wfi
    j 1b
```

The project has its own C library, `c`, which is the note `name-shadows-system`, as it replaces the system's in the
links which use it, which here, without an operating system, is none. Its headers are `public-include`, so what uses
it is compiled with them. `-ffreestanding` keeps clang from turning `strlen`'s loop into a call of `strlen`:

```c
// c/include/c.h
#include <stddef.h>
size_t strlen(const char *s);
void uart_puts(const char *s);

// c/string.c
#include "c.h"
size_t strlen(const char *s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

// c/uart.c
#include "c.h"
#define UART ((volatile char *)0x10000000)
void uart_puts(const char *s) {
    for (size_t i = 0; i < strlen(s); i++) *UART = s[i];
}
```

The kernel calls the C library, which writes to the virt machine's UART, then stops QEMU, by its test device:

```rust
// kernel/main.rs
#![no_std]
#![no_main]

unsafe extern "C" {
    fn uart_puts(s: *const core::ffi::c_char);
}

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    unsafe { uart_puts(c"hello from Rust, through C\n".as_ptr()) };
    // QEMU's test device on the virt machine, which exits
    unsafe { core::ptr::write_volatile(0x10_0000 as *mut u32, 0x5555) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

`jig run kernel` builds it, and runs it in QEMU:

```
$ jig run kernel
[1/6] AS riscv64gc-unknown-none-elf/c/boot/boot.S.o
[2/6] AR riscv64gc-unknown-none-elf/c/boot/libboot.a
[3/6] CC riscv64gc-unknown-none-elf/c/c/c-uart.c.o
[4/6] CC riscv64gc-unknown-none-elf/c/c/c-string.c.o
[5/6] AR riscv64gc-unknown-none-elf/c/c/libc.a
[6/6] RUSTC riscv64gc-unknown-none-elf/kernel
hello from Rust, through C
```
