//! `jig install`, which installs the files recorded in the database.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::configured::Configured;

/// Install the files of the crates and custom commands with `install` in a prefix, or the `install-prefix` property if
/// none is given, without evaluating the configuration.
///
/// What is installed is built by ninja first, then each file is copied into its directory within the prefix, replacing
/// any file there, with its permissions.
pub fn install(prefix: Option<PathBuf>) -> Result<(), Error> {
    let project = Configured::find()?;
    let mut ninja = project.ninja();
    ninja.arg("install");
    match ninja.status() {
        Ok(status) if status.success() => (),
        Ok(_) => return Err(Error::BuildFailed),
        Err(error) => return Err(Error::Exec(ninja, error)),
    }
    // read after ninja, which reconfigures if needed, so that it is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    // a prefix which is given is relative to the working directory, and the property to the project root
    let prefix = prefix.unwrap_or_else(|| project.root.join(database.install_prefix));
    for file in &database.installs {
        let source = project.root.join(file.path);
        let dir = prefix.join(file.dir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return Err(Error::Create(dir, error));
        }
        let destination = dir.join(Path::new(file.path).file_name().unwrap_or_default());
        if let Err(error) = copy(&source, &destination, file.mode) {
            return Err(Error::Install(source, destination, error));
        }
        println!("INSTALL {}", destination.display());
    }
    Ok(())
}

/// Copy a file with permissions `mode`, replacing the destination rather than writing to it, as it may be running.
fn copy(source: &Path, destination: &Path, mode: u32) -> std::io::Result<()> {
    match std::fs::remove_file(destination) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => (),
    }
    std::fs::copy(source, destination)?;
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(mode))
}
