//! The user's configuration, `$XDG_CONFIG_HOME/jig/config`, or on Windows, `%APPDATA%\jig\config`, or
//! `~/.config/jig/config`, or the file `JIG_CONFIG` gives, unless it is empty or `off`.
//!
//! It is a file of the same statements as `Jig`, which is processed before a project's own, as if the project began
//! with a `use` of it, so that it sets what belongs to the user or the machine rather than the project, such as rig's
//! cache, with `cache/dir`, `cache/remote` and the other builtin properties of the cache. A project's configuration
//! depends on it, so that it is configured again when it changes, or once it is created. `jig cache` and `jig serve`,
//! which have no project, process it alone.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::Error;
use crate::project::{File, Project};

/// The environment variable which gives the user's configuration, or turns it off.
pub const VARIABLE: &str = "JIG_CONFIG";

/// Returns the path of the user's configuration, absolute, from environment variables as `env` gives them, or `None`
/// if it is turned off, or there is nowhere for it, without `XDG_CONFIG_HOME`, `APPDATA` on Windows, and `HOME`.
///
/// A variable is only read if the one before it is not set, so that the configuration depends only on those which
/// decide where it is.
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

/// Process the user's configuration in a project, before the project's own files, if there is one, which the project
/// then depends on, or, while it is missing, is configured again once it exists.
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
