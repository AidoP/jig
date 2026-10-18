//! Probes, which find whether the host can run what is built for a Linux or Windows target, by running a tiny program
//! for its architecture which exits successfully.
//!
//! The probe for Linux is a static ELF executable of one segment, whose code makes the `exit` system call with status
//! `0`, and for Windows, a PE executable without imports whose entry point returns `0`, which is its exit status. So it
//! runs where the kernel runs the architecture, or an emulator does, such as one registered with binfmt or given as the
//! target's runner, or Wine for Windows.
//!
//! macOS needs a code signature for arm64 and does not support static executables, so a macOS target is probed by
//! running `/usr/bin/true`, which is a universal program of macOS, as the target's architecture with `arch`, which runs
//! where macOS does, such as x86_64 with Rosetta.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// A probe for a target, which is written to a file with its extension and run, taking up to its timeout.
struct Executable {
    bytes: Vec<u8>,
    extension: &'static str,
    timeout: Duration,
}

/// Returns the probe for a target, if it is for Linux or Windows and its architecture has one.
fn executable(target: &str) -> Option<Executable> {
    if let Some(elf) = elf_probe(target) {
        return Some(Executable { bytes: elf.elf(), extension: "", timeout: TIMEOUT });
    }
    let pe = pe_probe(target)?;
    // Wine may create its prefix the first time it is run
    Some(Executable { bytes: pe.pe(), extension: ".exe", timeout: WINE_TIMEOUT })
}

/// The format and code of the ELF probe for an architecture.
struct Elf {
    elf64: bool,
    big_endian: bool,
    machine: u16,
    flags: u32,
    /// The instructions which exit with status `0`, in the byte order of the architecture.
    code: &'static [u8],
}

/// Returns the ELF probe for a target, if it is for Linux and its architecture has one.
fn elf_probe(target: &str) -> Option<Elf> {
    let mut parts = target.split('-');
    let arch = parts.next()?;
    let rest: Vec<_> = parts.collect();
    if !rest.contains(&"linux") {
        return None;
    }
    let env = rest.last().copied().unwrap_or_default();
    let probe = match arch {
        // the x32 ABI, which runs 32-bit code on x86_64, has its own system call numbers
        "x86_64" if env == "gnux32" => Elf {
            elf64: false,
            big_endian: false,
            machine: 62,
            flags: 0,
            // mov eax, 0x4000003c; xor edi, edi; syscall
            code: &[0xb8, 0x3c, 0x00, 0x00, 0x40, 0x31, 0xff, 0x0f, 0x05],
        },
        "x86_64" => Elf {
            elf64: true,
            big_endian: false,
            machine: 62,
            flags: 0,
            // mov eax, 60; xor edi, edi; syscall
            code: &[0xb8, 0x3c, 0x00, 0x00, 0x00, 0x31, 0xff, 0x0f, 0x05],
        },
        "i386" | "i586" | "i686" => Elf {
            elf64: false,
            big_endian: false,
            machine: 3,
            flags: 0,
            // mov eax, 1; xor ebx, ebx; int 0x80
            code: &[0xb8, 0x01, 0x00, 0x00, 0x00, 0x31, 0xdb, 0xcd, 0x80],
        },
        "aarch64" => Elf {
            elf64: true,
            big_endian: false,
            machine: 183,
            flags: 0,
            // mov x8, #93; mov x0, #0; svc #0
            code: &[0xa8, 0x0b, 0x80, 0xd2, 0x00, 0x00, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4],
        },
        // the ARM instruction set, rather than Thumb, which every ARM Linux runs, with the EABI
        arch if arch.starts_with("arm") && !arch.starts_with("armeb") || arch.starts_with("thumbv") => Elf {
            elf64: false,
            big_endian: false,
            machine: 40,
            flags: 0x0500_0000,
            // mov r7, #1; mov r0, #0; svc #0
            code: &[0x01, 0x70, 0xa0, 0xe3, 0x00, 0x00, 0xa0, 0xe3, 0x00, 0x00, 0x00, 0xef],
        },
        arch if arch.starts_with("riscv64") => Elf {
            elf64: true,
            big_endian: false,
            machine: 243,
            flags: 0,
            // li a7, 93; li a0, 0; ecall
            code: &[0x93, 0x08, 0xd0, 0x05, 0x13, 0x05, 0x00, 0x00, 0x73, 0x00, 0x00, 0x00],
        },
        arch if arch.starts_with("riscv32") => Elf {
            elf64: false,
            big_endian: false,
            machine: 243,
            flags: 0,
            // li a7, 93; li a0, 0; ecall
            code: &[0x93, 0x08, 0xd0, 0x05, 0x13, 0x05, 0x00, 0x00, 0x73, 0x00, 0x00, 0x00],
        },
        "powerpc64le" => Elf {
            elf64: true,
            big_endian: false,
            machine: 21,
            // the ELFv2 ABI
            flags: 2,
            // li r0, 1; li r3, 0; sc
            code: &[0x01, 0x00, 0x00, 0x38, 0x00, 0x00, 0x60, 0x38, 0x02, 0x00, 0x00, 0x44],
        },
        "s390x" => Elf {
            elf64: true,
            big_endian: true,
            machine: 22,
            flags: 0,
            // lghi %r2, 0; svc 1
            code: &[0xa7, 0x29, 0x00, 0x00, 0x0a, 0x01],
        },
        _ => return None,
    };
    Some(probe)
}

/// The address the probe is loaded at, which is above the lowest address a program may be mapped at.
const BASE: u64 = 0x40_0000;

impl Elf {
    /// Returns the executable, which is its ELF header and program header, followed by its code.
    fn elf(&self) -> Vec<u8> {
        let (header, program_header) = if self.elf64 { (64, 56) } else { (52, 32) };
        let size = (header + program_header + self.code.len()) as u64;
        let entry = BASE + header as u64 + program_header as u64;
        let mut elf = Vec::new();
        let half = |elf: &mut Vec<u8>, value: u16| {
            elf.extend(if self.big_endian { value.to_be_bytes() } else { value.to_le_bytes() });
        };
        let word = |elf: &mut Vec<u8>, value: u32| {
            elf.extend(if self.big_endian { value.to_be_bytes() } else { value.to_le_bytes() });
        };
        // an address or offset, which is the size of the class
        let address = |elf: &mut Vec<u8>, value: u64| match (self.elf64, self.big_endian) {
            (true, true) => elf.extend(value.to_be_bytes()),
            (true, false) => elf.extend(value.to_le_bytes()),
            (false, true) => elf.extend((value as u32).to_be_bytes()),
            (false, false) => elf.extend((value as u32).to_le_bytes()),
        };
        elf.extend(b"\x7fELF");
        elf.extend([if self.elf64 { 2 } else { 1 }, if self.big_endian { 2 } else { 1 }, 1]);
        elf.resize(16, 0);
        // an executable for the machine, with its entry point after the headers and its program header after the ELF
        // header, without sections
        half(&mut elf, 2);
        half(&mut elf, self.machine);
        word(&mut elf, 1);
        address(&mut elf, entry);
        address(&mut elf, header as u64);
        address(&mut elf, 0);
        word(&mut elf, self.flags);
        half(&mut elf, header as u16);
        half(&mut elf, program_header as u16);
        half(&mut elf, 1);
        half(&mut elf, 0);
        half(&mut elf, 0);
        half(&mut elf, 0);
        // a loadable segment of the whole file, which is readable and executable
        let (load, read_execute) = (1, 5);
        if self.elf64 {
            word(&mut elf, load);
            word(&mut elf, read_execute);
            for value in [0, BASE, BASE, size, size, 0x1000] {
                address(&mut elf, value);
            }
        } else {
            word(&mut elf, load);
            for value in [0, BASE, BASE, size, size] {
                address(&mut elf, value);
            }
            word(&mut elf, read_execute);
            address(&mut elf, 0x1000);
        }
        elf.extend(self.code);
        elf
    }
}

/// The format and code of the PE probe for an architecture.
struct Pe {
    pe32_plus: bool,
    machine: u16,
    /// The instructions of the entry point, which returns `0`.
    code: &'static [u8],
}

/// Returns the PE probe for a target, if it is for Windows and its architecture has one.
fn pe_probe(target: &str) -> Option<Pe> {
    let mut parts = target.split('-');
    let arch = parts.next()?;
    if !parts.any(|part| part == "windows") {
        return None;
    }
    let pe = match arch {
        // xor eax, eax; ret
        "x86_64" => Pe { pe32_plus: true, machine: 0x8664, code: &[0x31, 0xc0, 0xc3] },
        "i586" | "i686" => Pe { pe32_plus: false, machine: 0x14c, code: &[0x31, 0xc0, 0xc3] },
        // mov w0, #0; ret
        "aarch64" => Pe { pe32_plus: true, machine: 0xaa64, code: &[0x00, 0x00, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6] },
        _ => return None,
    };
    Some(pe)
}

/// The alignment of the headers and the code section in a PE file.
const FILE_ALIGNMENT: u32 = 0x200;
/// The alignment of the sections in a PE image, which is where the code section is.
const SECTION_ALIGNMENT: u32 = 0x1000;

impl Pe {
    /// Returns the executable, which is a DOS header pointing at the PE headers, of one section of code, followed by
    /// the code, without imports or relocations, as the image is loaded at its base.
    fn pe(&self) -> Vec<u8> {
        let mut pe = Vec::new();
        let half = |pe: &mut Vec<u8>, value: u16| pe.extend(value.to_le_bytes());
        let word = |pe: &mut Vec<u8>, value: u32| pe.extend(value.to_le_bytes());
        let quad = |pe: &mut Vec<u8>, value: u64| pe.extend(value.to_le_bytes());
        // the DOS header, whose last field is the offset of the PE signature, which follows it
        pe.extend(b"MZ");
        pe.resize(0x3c, 0);
        word(&mut pe, 0x40);
        pe.extend(b"PE\0\0");
        // the COFF header, of an executable image without relocations, which is large address aware for PE32+ and
        // 32-bit otherwise
        let optional_header = if self.pe32_plus { 240 } else { 224 };
        half(&mut pe, self.machine);
        half(&mut pe, 1);
        word(&mut pe, 0);
        word(&mut pe, 0);
        word(&mut pe, 0);
        half(&mut pe, optional_header);
        half(&mut pe, if self.pe32_plus { 0x0023 } else { 0x0103 });
        // the optional header, of a console program for Windows Vista or later, whose entry point is the start of the
        // code section, after the headers
        half(&mut pe, if self.pe32_plus { 0x20b } else { 0x10b });
        half(&mut pe, 0);
        word(&mut pe, FILE_ALIGNMENT);
        word(&mut pe, 0);
        word(&mut pe, 0);
        word(&mut pe, SECTION_ALIGNMENT);
        word(&mut pe, SECTION_ALIGNMENT);
        if self.pe32_plus {
            quad(&mut pe, 0x1_4000_0000);
        } else {
            word(&mut pe, SECTION_ALIGNMENT);
            word(&mut pe, 0x40_0000);
        }
        word(&mut pe, SECTION_ALIGNMENT);
        word(&mut pe, FILE_ALIGNMENT);
        for version in [6, 0, 0, 0, 6, 0] {
            half(&mut pe, version);
        }
        word(&mut pe, 0);
        word(&mut pe, 2 * SECTION_ALIGNMENT);
        word(&mut pe, FILE_ALIGNMENT);
        word(&mut pe, 0);
        half(&mut pe, 3);
        half(&mut pe, 0);
        // the stack and heap, reserved and committed
        for size in [0x10_0000, 0x1000, 0x10_0000, 0x1000] {
            if self.pe32_plus {
                quad(&mut pe, size);
            } else {
                word(&mut pe, size as u32);
            }
        }
        word(&mut pe, 0);
        // the data directories, which are all empty
        word(&mut pe, 16);
        pe.resize(pe.len() + 16 * 8, 0);
        // the code section, which is readable and executable
        pe.extend(b".text\0\0\0");
        word(&mut pe, FILE_ALIGNMENT);
        word(&mut pe, SECTION_ALIGNMENT);
        word(&mut pe, FILE_ALIGNMENT);
        word(&mut pe, FILE_ALIGNMENT);
        pe.resize(pe.len() + 12, 0);
        word(&mut pe, 0x6000_0020);
        pe.resize(FILE_ALIGNMENT as usize, 0);
        pe.extend(self.code);
        pe.resize(2 * FILE_ALIGNMENT as usize, 0);
        pe
    }
}

/// How long a probe may run before it is taken to have failed, such as when a runner does not exit.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long a probe for Windows may run, as Wine may create its prefix when it is first run.
const WINE_TIMEOUT: Duration = Duration::from_secs(60);

/// Returns whether the host can run what is built for a target, by running its probe, with the runner of the target
/// if it has one, as a program and its first arguments.
///
/// A target which is not for Linux, Windows or macOS, or whose architecture has no probe, cannot be found to run, nor
/// can a macOS target where the host is not macOS.
pub(super) fn can_run(target: &str, runner: &[String], toolchain: Option<&str>) -> bool {
    if let Some(command) = macos_probe(target) {
        return std::env::consts::OS == "macos" && run(command, toolchain, TIMEOUT);
    }
    let Some(probe) = executable(target) else {
        return false;
    };
    let name = format!("jig-probe-{}-{target}{}", std::process::id(), probe.extension);
    let path = std::env::temp_dir().join(name);
    let ran = write(&path, &probe.bytes) && run(command(&path, runner), toolchain, probe.timeout);
    let _ = std::fs::remove_file(&path);
    ran
}

/// Returns the command which probes a macOS target, which runs `/usr/bin/true` as its architecture.
fn macos_probe(target: &str) -> Option<Command> {
    let (arch, rest) = target.split_once('-')?;
    if rest != "apple-darwin" {
        return None;
    }
    // as `arch` names them
    let arch = match arch {
        "aarch64" => "arm64",
        arch @ ("x86_64" | "x86_64h" | "arm64e") => arch,
        _ => return None,
    };
    let mut command = Command::new("/usr/bin/arch");
    command.args([&format!("-{arch}"), "/usr/bin/true"]);
    Some(command)
}

/// Write an executable file.
fn write(path: &Path, contents: &[u8]) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(path);
    file.and_then(|mut file| file.write_all(contents)).is_ok()
}

/// Returns the command which runs an executable, with a runner if one is given.
fn command(path: &Path, runner: &[String]) -> Command {
    match runner.split_first() {
        Some((program, arguments)) => {
            let mut command = Command::new(program);
            command.args(arguments).arg(path);
            command
        },
        None => Command::new(PathBuf::from(path)),
    }
}

/// Run a command, returning whether it exited successfully within the timeout.
fn run(mut command: Command, toolchain: Option<&str>, timeout: Duration) -> bool {
    crate::toolchain::apply(&mut command, toolchain);
    use std::process::Stdio;
    let Ok(mut child) = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{can_run, executable, macos_probe};

    #[test]
    fn probes() {
        for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-musl", "armv7-unknown-linux-gnueabihf"] {
            let probe = executable(target).unwrap();
            assert_eq!((&probe.bytes[..4], probe.extension), (&b"\x7fELF"[..], ""), "{target}");
        }
        for target in ["x86_64-pc-windows-msvc", "i686-pc-windows-gnu", "aarch64-pc-windows-msvc"] {
            let probe = executable(target).unwrap();
            assert_eq!((&probe.bytes[..2], probe.extension), (&b"MZ"[..], ".exe"), "{target}");
            assert_eq!(probe.bytes.len(), 0x400, "{target}");
        }
        // only Linux and Windows targets, of an architecture with a probe
        assert!(executable("riscv64gc-unknown-none-elf").is_none());
        assert!(executable("x86_64-apple-darwin").is_none());
        assert!(executable("mips64-unknown-linux-gnuabi64").is_none());
    }

    #[test]
    fn runners() {
        // the probe for the host's architecture runs on Linux, as does any with a runner which succeeds
        if cfg!(target_os = "linux") {
            let host = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
            assert!(can_run(&host, &[], None));
        }
        assert!(can_run("s390x-unknown-linux-gnu", &["true".into()], None));
        assert!(can_run("x86_64-pc-windows-msvc", &["true".into()], None));
        assert!(!can_run("s390x-unknown-linux-gnu", &["false".into()], None));
        assert!(!can_run("riscv64gc-unknown-none-elf", &["true".into()], None));
    }

    #[test]
    fn macos() {
        let probe = |target| macos_probe(target).map(|command| {
            let arguments: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            (command.get_program().to_string_lossy().into_owned(), arguments)
        });
        let arguments = |arch: &str| Some(("/usr/bin/arch".into(), vec![format!("-{arch}"), "/usr/bin/true".into()]));
        assert_eq!(probe("aarch64-apple-darwin"), arguments("arm64"));
        assert_eq!(probe("x86_64-apple-darwin"), arguments("x86_64"));
        assert_eq!(probe("aarch64-apple-ios"), None);
        // which only runs on macOS, where the host's own architecture runs
        let host = format!("{}-apple-darwin", std::env::consts::ARCH);
        assert_eq!(can_run(&host, &[], None), cfg!(target_os = "macos"));
    }
}
