//! The build database, which records what the build needs after configuring, such as the build directory, the
//! diagnostics to replay for each flycheck label, the tests to run and the files to install. It is at the project root,
//! which the commands that read it find it from.
//!
//! It is written when configuring, so that its users do not evaluate the configuration again, which may be impure.
//! When read, its contents borrow from its bytes.
//!
//! The format is the magic bytes and a format version, then the input, the toolchain, the build directory, the missing
//! paths, the install prefix, the severities of `proxy-build` and `other-jig`, and the `jig` which configured it,
//! followed by each section: the flychecks, the tests, then the installed files. A section is the number of its records
//! followed by each record. Numbers are little-endian `u32`, strings are their length followed by their UTF-8 bytes, a
//! list of strings or numbers is their number followed by each of them, and a `bool` is a number which is `0` or `1`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The path of the database, relative to the project root.
pub const PATH: &str = ".jig";
const MAGIC: &[u8; 4] = b"JIGD";
const VERSION: u32 = 10;

#[derive(Debug, Default, PartialEq)]
pub struct Database<'a> {
    /// The root file of the configuration, relative to the project root, which it is generated from.
    pub input: &'a str,
    /// The rustup toolchain it is configured with, given with `+<toolchain>`, or empty if none was given.
    pub toolchain: &'a str,
    /// The build directory, where ninja runs, relative to the project root.
    pub build_dir: &'a str,
    /// The paths which the configuration found were missing, relative to the project root, which it is generated again
    /// once one of them exists.
    pub missing: Vec<&'a str>,
    /// The directory `jig install` installs in by default, relative to the project root unless it is absolute.
    pub install_prefix: &'a str,
    /// The severity of the `proxy-build` diagnostic, as the value of its property, which `jig build --proxy` reports.
    pub proxy_build: &'a str,
    /// The severity of the `other-jig` diagnostic, which is reported when a `jig` other than the one which configured
    /// the project, and which regenerates `build.ninja`, is run.
    pub other_jig: &'a str,
    /// The `jig` which configured the project.
    pub jig: &'a str,
    pub flychecks: Vec<Flycheck<'a>>,
    pub tests: Vec<Test<'a>>,
    pub installs: Vec<Install<'a>>,
}

/// A flycheck label, whose diagnostics are replayed with those of the flychecks it depends on.
///
/// The dependencies are kept rather than every file to replay, which would grow with the square of the length of a chain
/// of crates.
#[derive(Debug, PartialEq)]
pub struct Flycheck<'a> {
    pub label: &'a str,
    /// The file of the JSON diagnostics saved by its check.
    pub diagnostics: &'a str,
    /// Whether the crate is edited, so that its diagnostics are replayed for the flychecks which depend on it.
    pub member: bool,
    /// The indices of the flychecks of the crates it depends on directly.
    pub deps: Vec<u32>,
}

/// A test, which passes if its command succeeds.
#[derive(Debug, PartialEq)]
pub struct Test<'a> {
    pub name: &'a str,
    /// The program and its arguments, run from the project root.
    pub command: Vec<&'a str>,
    /// Whether the program is a test harness built by libtest, run directly, which takes its arguments, such as
    /// `--color`.
    pub libtest: bool,
}

/// A file which `jig install` installs.
#[derive(Debug, PartialEq)]
pub struct Install<'a> {
    /// The file, relative to the project root.
    pub path: &'a str,
    /// The ninja target which builds the file, relative to the build directory.
    pub target: &'a str,
    /// The directory it is installed in, relative to the prefix unless it is absolute.
    pub dir: &'a str,
    /// The permissions of the installed file.
    pub mode: u32,
    /// Whether the file is a program, which `jig install --proxy` installs a wrapper for.
    pub program: bool,
}

pub fn write(w: &mut dyn Write, database: &Database) -> std::io::Result<()> {
    w.write_all(MAGIC)?;
    write_u32(w, VERSION)?;
    write_str(w, database.input)?;
    write_str(w, database.toolchain)?;
    write_str(w, database.build_dir)?;
    write_strs(w, &database.missing)?;
    write_str(w, database.install_prefix)?;
    write_str(w, database.proxy_build)?;
    write_str(w, database.other_jig)?;
    write_str(w, database.jig)?;
    write_u32(w, database.flychecks.len())?;
    for flycheck in &database.flychecks {
        write_str(w, flycheck.label)?;
        write_str(w, flycheck.diagnostics)?;
        write_u32(w, flycheck.member as u32)?;
        write_u32s(w, &flycheck.deps)?;
    }
    write_u32(w, database.tests.len())?;
    for test in &database.tests {
        write_str(w, test.name)?;
        write_strs(w, &test.command)?;
        write_u32(w, test.libtest as u32)?;
    }
    write_u32(w, database.installs.len())?;
    for install in &database.installs {
        write_str(w, install.path)?;
        write_str(w, install.target)?;
        write_str(w, install.dir)?;
        write_u32(w, install.mode)?;
        write_u32(w, install.program as u32)?;
    }
    Ok(())
}

/// Parse a database, failing with [`std::io::ErrorKind::InvalidData`] if it is not a database of this version.
pub fn read(bytes: &[u8]) -> std::io::Result<Database<'_>> {
    let mut r = Reader(bytes);
    if r.take(MAGIC.len())? != MAGIC || r.u32()? != VERSION {
        return Err(invalid());
    }
    let mut database = Database {
        input: r.str()?,
        toolchain: r.str()?,
        build_dir: r.str()?,
        missing: r.strs()?,
        install_prefix: r.str()?,
        proxy_build: r.str()?,
        other_jig: r.str()?,
        jig: r.str()?,
        ..Default::default()
    };
    for _ in 0..r.u32()? {
        database.flychecks.push(Flycheck { label: r.str()?, diagnostics: r.str()?, member: r.bool()?, deps: r.u32s()? });
    }
    for _ in 0..r.u32()? {
        database.tests.push(Test { name: r.str()?, command: r.strs()?, libtest: r.bool()? });
    }
    for _ in 0..r.u32()? {
        database.installs.push(Install {
            path: r.str()?,
            target: r.str()?,
            dir: r.str()?,
            mode: r.u32()?,
            program: r.bool()?,
        });
    }
    Ok(database)
}

/// Returns the root of the project which contains the working directory, which is the closest directory to it, or it
/// itself, with a database.
pub fn root() -> Option<PathBuf> {
    let dir = std::env::current_dir().ok()?;
    dir.ancestors().find(|dir| dir.join(PATH).is_file()).map(Path::to_path_buf)
}

fn invalid() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "not a build database of this version")
}

fn write_u32(w: &mut dyn Write, value: impl TryInto<u32>) -> std::io::Result<()> {
    let value: u32 = value.try_into().map_err(|_| std::io::Error::other("value too large for the database"))?;
    w.write_all(&value.to_le_bytes())
}

fn write_str(w: &mut dyn Write, s: &str) -> std::io::Result<()> {
    write_u32(w, s.len())?;
    w.write_all(s.as_bytes())
}

fn write_strs(w: &mut dyn Write, strs: &[&str]) -> std::io::Result<()> {
    write_u32(w, strs.len())?;
    for s in strs {
        write_str(w, s)?;
    }
    Ok(())
}

fn write_u32s(w: &mut dyn Write, values: &[u32]) -> std::io::Result<()> {
    write_u32(w, values.len())?;
    for &value in values {
        write_u32(w, value)?;
    }
    Ok(())
}

/// The remaining bytes of a database being parsed.
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> std::io::Result<&'a [u8]> {
        let Some((taken, rest)) = self.0.split_at_checked(len) else {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        };
        self.0 = rest;
        Ok(taken)
    }
    fn u32(&mut self) -> std::io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn str(&mut self) -> std::io::Result<&'a str> {
        let len = self.u32()? as usize;
        std::str::from_utf8(self.take(len)?).map_err(|_| invalid())
    }
    fn bool(&mut self) -> std::io::Result<bool> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid()),
        }
    }
    fn strs(&mut self) -> std::io::Result<Vec<&'a str>> {
        let len = self.u32()?;
        (0..len).map(|_| self.str()).collect()
    }
    fn u32s(&mut self) -> std::io::Result<Vec<u32>> {
        let len = self.u32()?;
        (0..len).map(|_| self.u32()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let database = Database {
            build_dir: "build",
            input: "Jig",
            toolchain: "nightly",
            missing: vec!["Debug"],
            install_prefix: "/usr/local",
            proxy_build: "warning",
            other_jig: "warning",
            jig: "/usr/bin/jig",
            flychecks: vec![
                Flycheck { label: "app@x86_64", diagnostics: "a.json", member: true, deps: vec![1] },
                Flycheck { label: "lib é", diagnostics: "b é.json", member: false, deps: vec![] },
            ],
            tests: vec![
                Test { name: "unit", command: vec!["./run", "a b"], libtest: false },
                Test { name: "app", command: vec!["./build/test/app"], libtest: true },
            ],
            installs: vec![
                Install { path: "build/app", target: "app", dir: "bin", mode: 0o755, program: true },
                Install { path: "build/data", target: "custom/data", dir: "share", mode: 0o644, program: false },
            ],
        };
        let mut bytes = Vec::new();
        write(&mut bytes, &database).unwrap();
        assert_eq!(read(&bytes).unwrap(), database);
    }

    #[test]
    fn rejects_other_files() {
        let mut bytes = Vec::new();
        write(&mut bytes, &Database::default()).unwrap();
        bytes[4] += 1;
        assert_eq!(read(&bytes).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(read(b"{\"crates\"").unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        // truncated
        let mut bytes = Vec::new();
        let tests = vec![Test { name: "x", command: vec![], libtest: false }];
        write(&mut bytes, &Database { tests, ..Default::default() }).unwrap();
        assert_eq!(read(&bytes[..bytes.len() - 1]).unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    }
}

#[cfg(test)]
mod benches {
    use super::*;
    use crate::bench::assert_linear;

    #[test]
    fn round_trip() {
        let labels: Vec<_> = (0..20000).map(|i| (format!("c{i}@x86_64-unknown-linux-gnu"), format!("check/c{i}.json"))).collect();
        let database = |n: usize| {
            let flychecks = labels[..n].iter().enumerate().map(|(i, (label, diagnostics))| {
                let deps = if i > 0 { vec![i as u32 - 1, 0] } else { vec![] };
                Flycheck { label, diagnostics, member: true, deps }
            }).collect();
            let tests = labels[..n].iter().map(|(label, _)| Test { name: label, command: vec![label, "--quiet"], libtest: true }).collect();
            Database { flychecks, tests, ..Default::default() }
        };
        assert_linear("database", 5000, database, |database| {
            let mut bytes = Vec::new();
            write(&mut bytes, database).unwrap();
            assert_eq!(read(&bytes).unwrap().flychecks.len(), database.flychecks.len());
        });
    }
}
