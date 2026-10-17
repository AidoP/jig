//! A configured project, found from the working directory by its database, for the commands which build it with ninja
//! without evaluating the configuration.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, database, generate};

/// Where ninja's output is written while finding a project, as `jig check` keeps it out of stdout.
#[derive(Clone, Copy)]
pub enum Output {
    Stdout,
    Stderr,
    /// Stdout, where ninja is quiet, so that only the output of what it runs is shown.
    Quiet,
}

pub struct Configured {
    /// The project root, where the database is, which is where commands and tests run from.
    pub root: PathBuf,
    /// The build directory, where `build.ninja` is, which is where ninja runs.
    pub build_dir: PathBuf,
}
impl Configured {
    /// Find the project which contains the working directory, which must have been configured.
    ///
    /// The configuration is brought up to date, as the build directory may change, where ninja's output is written to
    /// `output`.
    pub fn find(output: Output) -> Result<Self, Error> {
        let mut project = Self::locate()?;
        project.reconfigure(output)?;
        Ok(project)
    }
    /// Find the project which contains the working directory, as [`Self::find`] does, without bringing the
    /// configuration up to date, so that the build directory may be changed by [`Self::reconfigure`].
    ///
    /// If a path which the configuration found was missing has been created since, ninja is made to regenerate
    /// `build.ninja` by touching the reconfigure stamp, which it depends on.
    pub fn locate() -> Result<Self, Error> {
        let Some(root) = database::root() else {
            return Err(Error::NotConfigured);
        };
        let bytes = read(&root)?;
        let database = parse(&root, &bytes)?;
        let build_dir = root.join(database.build_dir);
        if database.missing.iter().any(|path| root.join(path).exists()) {
            let stamp = build_dir.join(generate::RECONFIGURE_STAMP);
            let touched = std::fs::File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&stamp)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()));
            if let Err(error) = touched {
                return Err(Error::Write(stamp, error));
            }
        }
        Ok(Self { root, build_dir })
    }
    /// Regenerate `build.ninja` with ninja if it is out of date, then take the build directory from the database again,
    /// as `build-dir` may have changed, which leaves a stub in the previous build directory.
    ///
    /// Ninja's output is passed on as it is written, except that it had no work to do, as the build follows.
    pub fn reconfigure(&mut self, output: Output) -> Result<(), Error> {
        use std::io::{BufRead, Write};
        let mut ninja = self.ninja();
        if let Output::Quiet = output {
            ninja.arg("--quiet");
        }
        ninja.arg("build.ninja").stdout(std::process::Stdio::piped());
        let mut child = match ninja.spawn() {
            Ok(child) => child,
            Err(error) => return Err(Error::Exec(ninja, error)),
        };
        let mut w: Box<dyn Write> = match output {
            Output::Stdout | Output::Quiet => Box::new(std::io::stdout()),
            Output::Stderr => Box::new(std::io::stderr()),
        };
        for line in std::io::BufReader::new(child.stdout.take().unwrap()).split(b'\n') {
            let Ok(line) = line else {
                break;
            };
            if line != b"ninja: no work to do." {
                let _ = w.write_all(&line).and_then(|()| w.write_all(b"\n"));
            }
        }
        match child.wait() {
            Ok(status) if status.success() => (),
            Ok(_) => return Err(Error::BuildFailed),
            Err(error) => return Err(Error::Exec(ninja, error)),
        }
        let bytes = read(&self.root)?;
        self.build_dir = self.root.join(parse(&self.root, &bytes)?.build_dir);
        Ok(())
    }
    /// Returns ninja, to run in the build directory.
    pub fn ninja(&self) -> Command {
        let mut ninja = Command::new("ninja");
        ninja.current_dir(&self.build_dir);
        ninja
    }
    /// Returns the bytes of the database, which is read again once ninja has run, as it reconfigures if needed.
    pub fn read(&self) -> Result<Vec<u8>, Error> {
        read(&self.root)
    }
    /// Parse the database, which borrows from its bytes.
    pub fn parse<'a>(&self, bytes: &'a [u8]) -> Result<database::Database<'a>, Error> {
        parse(&self.root, bytes)
    }
}

fn read(root: &Path) -> Result<Vec<u8>, Error> {
    let path = root.join(database::PATH);
    std::fs::read(&path).map_err(|error| Error::Read(path, error))
}

fn parse<'a>(root: &Path, bytes: &'a [u8]) -> Result<database::Database<'a>, Error> {
    database::read(bytes).map_err(|error| Error::Read(root.join(database::PATH), error))
}
