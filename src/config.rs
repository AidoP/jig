//! The user's configuration. It is the file `JIG_CONFIG` names, unless that is empty or `off`, which turn it off,
//! and otherwise `$XDG_CONFIG_HOME/jig/config`, or failing that, on Windows, `%APPDATA%\jig\config`, or failing
//! that `~/.config/jig/config`.
//!
//! It holds the same statements as a `Jig` file, and is processed before a project's own files, as if the project
//! began with a `use` of it. It is for settings which belong to the user or the machine rather than to a project,
//! such as rig's cache: `cache/dir`, `cache/remote` and the other builtin properties of the cache. A project's
//! configuration depends on it, so the project is configured again when it changes, or when it is first created.
//! `jig cache` and `jig serve`, which have no project, process it on its own.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Error;
use crate::project::{File, Project};

/// The environment variable which gives the user's configuration, or turns it off.
pub const VARIABLE: &str = "JIG_CONFIG";

/// Returns the absolute path of the user's configuration, from environment variables as `env` gives them. Returns
/// `None` if it is turned off, or if there is nowhere for it, as none of `XDG_CONFIG_HOME`, `APPDATA` on Windows,
/// and `HOME` is set.
///
/// Each variable is only read if the one before it is not set, so that the configuration depends on no more
/// variables than those which decide where the file is.
pub fn path(mut env: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    let path = match env(VARIABLE) {
        Some(path) if path.is_empty() || path == "off" => return None,
        Some(path) => PathBuf::from(path),
        None => match set(env("XDG_CONFIG_HOME")) {
            Some(config) => PathBuf::from(config).join("jig/config"),
            // on Windows, where a user's settings go
            None if cfg!(windows) && let Some(data) = set(env("APPDATA")) => PathBuf::from(data).join("jig/config"),
            None => PathBuf::from(set(env("HOME"))?).join(".config/jig/config"),
        },
    };
    std::path::absolute(&path).ok()
}

/// Process the user's configuration in a project, before the project's own files. The project then depends on
/// it. If the file does not exist, nothing is processed, and the project is configured again once it does.
///
/// # Errors
///
/// If it exists but cannot be read.
pub fn apply(project: &mut Project) -> Result<(), Error> {
    let Some(path) = path(|name| project.read_env(name)) else {
        return Ok(());
    };
    match File::read(path.clone()) {
        Ok(file) => {
            project.parse(file);
            Ok(())
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            project.missing.push(path);
            Ok(())
        },
        Err(error) => Err(Error::Read(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_configuration() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(var, _)| *var == name).map(|(_, value)| OsString::from(value))
        };
        // each absolute, which on Windows has a drive
        let absolute = |path: &str| Some(std::path::absolute(path).unwrap());
        assert_eq!(path(env(&[("JIG_CONFIG", "/c/jig"), ("HOME", "/h")])), absolute("/c/jig"));
        assert_eq!(path(env(&[("JIG_CONFIG", "off"), ("HOME", "/h")])), None);
        assert_eq!(path(env(&[("JIG_CONFIG", ""), ("HOME", "/h")])), None);
        assert_eq!(path(env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])), absolute("/x/jig/config"));
        assert_eq!(path(env(&[("HOME", "/h")])), absolute("/h/.config/jig/config"));
        let data = path(env(&[("APPDATA", "/a"), ("HOME", "/h")]));
        assert_eq!(data, absolute(if cfg!(windows) { "/a/jig/config" } else { "/h/.config/jig/config" }));
        assert_eq!(path(env(&[])), None);
    }
}
