//! A configured project, found from the working directory by its database, for the commands which build it with ninja
//! without evaluating the configuration.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, database, generate};

pub struct Configured {
    /// The project root, where the database is, which is where commands and tests run from.
    pub root: PathBuf,
    /// The build directory, where `build.ninja` is, which is where ninja runs.
    pub build_dir: PathBuf,
}
impl Configured {
    /// Find the project which contains the working directory, which must have been configured.
    ///
    /// If a path which the configuration found was missing has been created since, ninja is made to regenerate
    /// `build.ninja` by touching the reconfigure stamp, which it depends on.
    pub fn find() -> Result<Self, Error> {
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
