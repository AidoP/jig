//! A program which tests run as an edge's action, so that they need neither a shell nor coreutils, and run on every
//! platform: the test's own executable, run again with [`FLAG`], which does what its arguments say, and exits, before
//! the test harness's `main` runs. [`register`](crate::register_test_helper) registers it in a test executable, and
//! [`exec`] returns the program which runs its steps.
//!
//! Its arguments after the flag are steps, separated by `&&`, each of which runs only if those before it succeeded. A
//! step is an operation and its words, and what it writes to stdout goes to a file if it ends in `> <file>`, or is
//! appended to one if it ends in `>> <file>`:
//!
//! - `cat <file> ...`, the contents of the files, `echo <word> ...`, the words, with a space between them and a newline
//!   after them, and a `$<name>` at the start of one replaced by the variable's value, or nothing, `printf <text>`, the
//!   text with `\n`, `\e` and `\x<hex>` as a newline, an escape and a byte, `eecho <word> ...`, as `echo`, to stderr,
//!   and `pwd`, the working directory.
//! - `touch <file> ...`, `cp <from> <to>`, `mkdir <dir>` and `rmdir <dir>`, as their commands do, and `chmod-x <file>`,
//!   which makes a file executable, on Unix.
//! - `true`, `test-dir <dir>`, which fails unless the directory is there, and `cwd-is <dir>`, unless it is the
//!   working directory, as the system gives it.
//! - `sleep <seconds>` and `sleep-for <file>`, for as many seconds as the file gives.
//! - `exit <code> [<word> ...]` and `false`, which fail.
//! - `restat <output> <input> ...`, which writes the inputs to the output only if they are not what it has already.
//! - `getenv <name>`, the variable's value, or `unset`, with a newline, `env`, every variable, as `<name>=<value>`
//!   lines, and `args <word> ...`, each word followed by `|`.
//! - `zeros <count>`, as many zero bytes, `size <file>`, its size in bytes, with a newline, `first <file>`, the first
//!   character of each of its lines, and `strip <text> <prefix>`, the text without the prefix.
//! - `run <program> <argument> ...`, which runs a program, whose stdout is the step's, and fails as it does.
//! - `cc <argument> ...`, which stands in for a C compiler: it writes the object of `-o`, which is its arguments on a
//!   line, then the source of `-c`, with each line `#include "<header>"` replaced by the header, found beside the source
//!   or in a directory of `-I`, and the depfile of `-MF`, listing the source and the headers.
//! - `ar <flags> <archive> <member> ...`, which stands in for an archiver: it adds each member to the archive, by its
//!   file name, replacing one of the same name, as `ar r` does, each as `--- <name> <size>` on a line, then its contents.
//! - `spawn <seconds> <file>`, which runs the helper again to sleep, writing its process ID to the file, and waits for
//!   it, and `tokens`, which takes two of the jobserver's tokens, prints them, and gives them back, on Unix.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::Exec;

/// The first argument, which tells a test executable to run as the helper.
pub const FLAG: &str = "--rig-test-helper";

/// Returns the program which runs the helper's steps, given as words, from the build's directory.
#[must_use]
pub fn exec(words: Vec<String>) -> Exec {
    let args = std::iter::once(FLAG.to_string()).chain(words).collect();
    Exec { program: program(), args, ..Exec::default() }
}

/// Returns the test's executable, which is the helper's program.
///
/// # Panics
///
/// If it cannot be found.
#[must_use]
pub fn program() -> String {
    std::env::current_exe().expect("the test's executable is not found").display().to_string()
}

/// Split a line into words at spaces, but for those between single quotes, which are taken as they are.
#[must_use]
pub fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let (mut word, mut quoted, mut started) = (String::new(), false, false);
    for c in line.chars() {
        match c {
            '\'' => (quoted, started) = (!quoted, true),
            ' ' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            },
            c => {
                word.push(c);
                started = true;
            },
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// Run as the helper, and exit, if the executable was run as it, with [`FLAG`].
pub fn run_if_requested() {
    let args = args();
    if args.get(1).is_none_or(|first| first != FLAG) {
        return;
    }
    let code = match run(&args[2..]) {
        Ok(()) => 0,
        Err((code, message)) => {
            if !message.is_empty() {
                let _ = writeln!(std::io::stderr(), "{message}");
            }
            code
        },
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// Returns the arguments of the process, which on Linux are read from `/proc`, as `std` may not have them before
/// `main`, as with musl.
fn args() -> Vec<String> {
    #[cfg(target_os = "linux")]
    if let Ok(bytes) = std::fs::read("/proc/self/cmdline") {
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
        return bytes.split(|&byte| byte == 0).map(|arg| String::from_utf8_lossy(arg).into_owned()).collect();
    }
    std::env::args().collect()
}

type Failure = (i32, String);

fn failed(message: impl std::fmt::Display) -> Failure {
    (1, message.to_string())
}

fn read(path: &str) -> Result<Vec<u8>, Failure> {
    std::fs::read(path).map_err(|error| failed(format!("{path}: {error}")))
}

fn run(args: &[String]) -> Result<(), Failure> {
    for step in args.split(|arg| arg == "&&") {
        self::step(step)?;
    }
    Ok(())
}

fn step(words: &[String]) -> Result<(), Failure> {
    let (words, target) = match words {
        [rest @ .., redirect, file] if redirect == ">" || redirect == ">>" => (rest, Some((redirect == ">>", file))),
        _ => (words, None),
    };
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    let io = |result: std::io::Result<()>| result.map_err(failed);
    match words.as_slice() {
        ["cat", files @ ..] => {
            for file in files {
                out.extend(read(file)?);
            }
        },
        ["echo", words @ ..] => {
            let words: Vec<String> = words.iter().map(|word| expand(word)).collect();
            out = format!("{}\n", words.join(" ")).into_bytes();
        },
        ["eecho", words @ ..] => io(writeln!(std::io::stderr(), "{}", words.join(" ")))?,
        ["printf", text] => out = unescape(text),
        ["pwd"] => out = format!("{}\n", std::env::current_dir().map_err(failed)?.display()).into_bytes(),
        ["run", program, args @ ..] => {
            let ran = std::process::Command::new(program).args(args).output().map_err(failed)?;
            io(std::io::stderr().write_all(&ran.stderr))?;
            out = ran.stdout;
            if !ran.status.success() {
                io(std::io::stdout().write_all(&out))?;
                return Err((ran.status.code().unwrap_or(1), String::new()));
            }
        },
        ["touch", files @ ..] => {
            for file in files {
                let opened = std::fs::File::options().create(true).append(true).open(file);
                io(opened.and_then(|file| file.set_modified(std::time::SystemTime::now())))?;
            }
        },
        ["cp", from, to] => io(std::fs::copy(from, to).map(|_| ()))?,
        ["mkdir", dir] => io(std::fs::create_dir(dir))?,
        ["rmdir", dir] => io(std::fs::remove_dir(dir))?,
        ["test-dir", dir] if Path::new(dir).is_dir() => (),
        ["cwd-is", dir] if std::env::current_dir().is_ok_and(|cwd| cwd == Path::new(dir)) => (),
        ["true"] => (),
        #[cfg(unix)]
        ["chmod-x", file] => {
            use std::os::unix::fs::PermissionsExt;
            io(std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)))?;
        },
        ["sleep", seconds] => sleep(seconds)?,
        ["sleep-for", file] => sleep(String::from_utf8_lossy(&read(file)?).trim())?,
        // what follows the code is only shown, as when the program fails
        ["exit", code, ..] => return Err((code.parse().map_err(failed)?, String::new())),
        ["false"] => return Err((1, String::new())),
        ["restat", output, inputs @ ..] => {
            let contents = inputs.iter().map(|input| read(input)).collect::<Result<Vec<_>, _>>()?.concat();
            if std::fs::read(output).ok().as_ref() != Some(&contents) {
                io(std::fs::write(output, contents))?;
            }
        },
        ["getenv", name] => {
            let value = std::env::var(name).unwrap_or_else(|_| "unset".into());
            out = format!("{value}\n").into_bytes();
        },
        ["env"] => {
            for (name, value) in std::env::vars_os() {
                out.extend(format!("{}={}\n", name.to_string_lossy(), value.to_string_lossy()).into_bytes());
            }
        },
        ["args", words @ ..] => {
            for word in words {
                out.extend(word.as_bytes());
                out.push(b'|');
            }
        },
        ["zeros", count] => out = vec![0; count.parse().map_err(failed)?],
        ["size", file] => out = format!("{}\n", read(file)?.len()).into_bytes(),
        ["first", file] => {
            let contents = String::from_utf8_lossy(&read(file)?).into_owned();
            for c in contents.lines().filter_map(|line| line.chars().next()) {
                out.extend(c.to_string().into_bytes());
                out.push(b'\n');
            }
        },
        ["strip", text, prefix] => out = text.strip_prefix(prefix).unwrap_or(text).as_bytes().to_vec(),
        ["spawn", seconds, file] => {
            let mut command = std::process::Command::new(std::env::current_exe().map_err(failed)?);
            let mut child = command.args([FLAG, "sleep", seconds]).spawn().map_err(failed)?;
            io(std::fs::write(file, child.id().to_string()))?;
            io(child.wait().map(|_| ()))?;
        },
        ["cc", args @ ..] => compile(args)?,
        ["ar", _, archive, members @ ..] => archive_members(archive, members)?,
        #[cfg(unix)]
        ["tokens"] => out = tokens()?,
        _ => return Err(failed(format!("the test helper has no step {words:?}"))),
    }
    match target {
        Some((append, file)) => {
            let opened = std::fs::File::options().create(true).write(true).append(append).truncate(!append).open(file);
            io(opened.and_then(|mut file| file.write_all(&out)))
        },
        None => io(std::io::stdout().write_all(&out)),
    }
}

/// Stand in for a C compiler, as the `cc` step.
fn compile(args: &[&str]) -> Result<(), Failure> {
    let after = |flag: &str| args.iter().position(|arg| *arg == flag).and_then(|i| args.get(i + 1)).copied();
    let (Some(source), Some(object)) = (after("-c"), after("-o")) else {
        return Err(failed("cc needs -c <source> and -o <object>"));
    };
    let dirs: Vec<&Path> = std::iter::once(Path::new(source).parent().unwrap_or(Path::new(".")))
        .chain(args.iter().filter_map(|arg| arg.strip_prefix("-I")).map(Path::new))
        .collect();
    let mut out = format!("{}\n", args.join(" ")).into_bytes();
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(&read(source)?).lines() {
        let header = line.strip_prefix("#include \"").and_then(|rest| rest.strip_suffix('"'));
        match header {
            Some(header) => {
                let found = dirs.iter().map(|dir| dir.join(header)).find(|path| path.is_file());
                let path = found.ok_or_else(|| failed(format!("{source}: {header} not found")))?;
                out.extend(read(&path.display().to_string())?);
                headers.push(path.display().to_string());
            },
            None => out.extend(format!("{line}\n").into_bytes()),
        }
    }
    std::fs::write(object, out).map_err(failed)?;
    if let Some(depfile) = after("-MF") {
        let rule = format!("{object}: {source} {}\n", headers.join(" "));
        std::fs::write(depfile, rule).map_err(failed)?;
    }
    Ok(())
}

/// Stand in for an archiver, as the `ar` step.
fn archive_members(archive: &str, members: &[&str]) -> Result<(), Failure> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    if let Ok(bytes) = std::fs::read(archive) {
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            let end = rest.iter().position(|&byte| byte == b'\n').ok_or_else(|| failed("a truncated archive"))?;
            let header = String::from_utf8_lossy(&rest[..end]).into_owned();
            let mut parts = header.trim_start_matches("--- ").rsplitn(2, ' ');
            let size: usize = parts.next().and_then(|size| size.parse().ok()).ok_or_else(|| failed("a bad archive"))?;
            let name = parts.next().unwrap_or_default().to_string();
            entries.push((name, rest[end + 1..end + 1 + size].to_vec()));
            rest = &rest[end + 1 + size..];
        }
    }
    for member in members {
        let name = Path::new(member).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let contents = read(member)?;
        match entries.iter_mut().find(|(known, _)| *known == name) {
            Some(entry) => entry.1 = contents,
            None => entries.push((name, contents)),
        }
    }
    let mut out = Vec::new();
    for (name, contents) in entries {
        out.extend(format!("--- {name} {}\n", contents.len()).into_bytes());
        out.extend(contents);
    }
    std::fs::write(archive, out).map_err(failed)
}

/// Returns a word with a `$<name>` at its start replaced by the variable's value, or nothing if it is not set.
fn expand(word: &str) -> String {
    let Some(rest) = word.strip_prefix('$') else {
        return word.to_string();
    };
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(rest.len());
    std::env::var(&rest[..end]).unwrap_or_default() + &rest[end..]
}

/// Returns text with `\n`, `\e` and `\x<hex>`, of two digits, as a newline, an escape and a byte.
fn unescape(text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut rest = text.as_bytes();
    while let Some((&byte, after)) = rest.split_first() {
        rest = after;
        match (byte, rest) {
            (b'\\', [escape @ (b'n' | b'e'), after @ ..]) => {
                bytes.push(if *escape == b'n' { b'\n' } else { 0x1b });
                rest = after;
            },
            (b'\\', [b'x', high, low, after @ ..]) => {
                let hex = std::str::from_utf8(&[*high, *low]).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok());
                bytes.push(hex.unwrap_or(b'?'));
                rest = after;
            },
            (byte, _) => bytes.push(byte),
        }
    }
    bytes
}

fn sleep(seconds: &str) -> Result<(), Failure> {
    std::thread::sleep(Duration::from_secs_f64(seconds.parse().map_err(failed)?));
    Ok(())
}

/// Take two tokens from the jobserver's FIFO, which `MAKEFLAGS` gives, and give them back, returning them.
#[cfg(unix)]
fn tokens() -> Result<Vec<u8>, Failure> {
    use std::io::Read as _;
    let makeflags = std::env::var("MAKEFLAGS").map_err(failed)?;
    let fifo = makeflags.split_once("fifo:").map(|(_, fifo)| fifo).ok_or_else(|| failed("no jobserver"))?;
    let fifo = Path::new(fifo.split(' ').next().unwrap_or(fifo));
    let mut tokens = [0; 2];
    let read = std::fs::File::open(fifo).and_then(|mut file| file.read_exact(&mut tokens));
    read.map_err(failed)?;
    std::fs::File::options().write(true).open(fifo).and_then(|mut file| file.write_all(b"++")).map_err(failed)?;
    Ok(tokens.to_vec())
}

/// Registers the [test helper](self) in a test executable, which it runs as before `main`, when it is run as it.
#[macro_export]
macro_rules! register_test_helper {
    () => {
        #[used]
        #[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
        #[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__mod_init_func"))]
        #[cfg_attr(not(any(windows, target_vendor = "apple")), unsafe(link_section = ".init_array"))]
        static RIG_TEST_HELPER: extern "C" fn() = {
            extern "C" fn run() {
                $crate::test_helper::run_if_requested();
            }
            run
        };
    };
}
