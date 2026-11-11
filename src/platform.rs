//! What jig does differently on each platform: file modes, links, what identifies a file, the programs in a directory,
//! how a program which was ended by a signal exits, and where `jig install` installs by default.
//!
//! On Windows, files have no modes, a link which cannot be made, as making one needs a privilege, is a copy, a file is
//! identified by its canonical path, a program is found with the extensions Windows runs it by, and what is installed
//! goes where a user's own programs do.

use std::path::{Path, PathBuf};

/// Returns the permissions of a file, as Unix modes, of which Windows has only whether it is read-only.
#[cfg(unix)]
pub fn mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}
#[cfg(all(windows, test))]
pub fn mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.permissions().readonly() { 0o444 } else { 0o644 }
}

/// Whether a file has permissions, as Unix modes, as [`set_mode`] gives them, which on Windows is whether it is
/// read-only, as it would be with them.
#[cfg(unix)]
pub fn has_mode(metadata: &std::fs::Metadata, mode: u32) -> bool {
    self::mode(metadata) == mode
}
#[cfg(windows)]
pub fn has_mode(metadata: &std::fs::Metadata, mode: u32) -> bool {
    metadata.permissions().readonly() == (mode & 0o222 == 0)
}

/// Give a file permissions, as Unix modes, of which Windows takes only whether it is written by none.
///
/// # Errors
///
/// If its permissions cannot be set.
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}
#[cfg(windows)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    std::fs::set_permissions(path, permissions)
}

/// Whether a file is a program: executable by its mode, or on Windows, by its extension.
#[cfg(unix)]
pub fn is_program(path: &Path, metadata: &std::fs::Metadata) -> bool {
    let _ = path;
    metadata.is_file() && mode(metadata) & 0o111 != 0
}
#[cfg(windows)]
pub fn is_program(path: &Path, metadata: &std::fs::Metadata) -> bool {
    let extension = path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase);
    metadata.is_file() && extension.is_some_and(|extension| PROGRAM_EXTENSIONS.contains(&extension.as_str()))
}

/// The extensions of programs which Windows runs, as `PATHEXT` has them by default.
#[cfg(windows)]
const PROGRAM_EXTENSIONS: [&str; 4] = ["exe", "com", "bat", "cmd"];

/// Returns a program in a directory, by its name, or on Windows, by its name with an extension of a program, if it is
/// there.
pub fn program_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(name);
    let found = |path: &Path| path.metadata().is_ok_and(|metadata| is_program(path, &metadata));
    if found(&path) {
        return Some(path);
    }
    #[cfg(windows)]
    for extension in PROGRAM_EXTENSIONS {
        let path = dir.join(format!("{name}.{extension}"));
        if found(&path) {
            return Some(path);
        }
    }
    None
}

/// Link to a file, by its absolute path, or on Windows, copy it where a link cannot be made.
///
/// # Errors
///
/// If it can be neither linked nor copied.
#[cfg(unix)]
pub fn link(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::path::absolute(source)?, destination)
}
#[cfg(windows)]
pub fn link(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = std::path::absolute(source)?;
    let linked = if source.is_dir() {
        std::os::windows::fs::symlink_dir(&source, destination)
    } else {
        std::os::windows::fs::symlink_file(&source, destination)
    };
    match linked {
        Ok(()) => Ok(()),
        Err(_) if source.is_file() => std::fs::copy(&source, destination).map(|_| ()),
        Err(error) => Err(error),
    }
}

/// What identifies a file which may be reached by many paths: its device and inode on Unix, or its canonical path on
/// Windows, which reads each link of its path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileId {
    #[cfg(unix)]
    Inode { dev: u64, ino: u64 },
    Path(PathBuf),
}

/// Returns what identifies a file, from its metadata where it can.
#[cfg(unix)]
pub fn file_id(path: &Path, metadata: &std::fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    let _ = path;
    FileId::Inode { dev: metadata.dev(), ino: metadata.ino() }
}
#[cfg(windows)]
pub fn file_id(path: &Path, metadata: &std::fs::Metadata) -> FileId {
    let _ = metadata;
    FileId::Path(std::fs::canonicalize(path).unwrap_or_else(|_| path.into()))
}

/// Returns the inode of a file, which changes when it is replaced, or `0` on Windows, which has none in `std`.
#[cfg(unix)]
pub fn inode(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}
#[cfg(windows)]
pub fn inode(metadata: &std::fs::Metadata) -> u64 {
    let _ = metadata;
    0
}

/// Returns the directory `jig install` installs in by default, with `read` reading an environment variable:
/// `/usr/local` on Unix, and on Windows, where programs a user installs for themselves go, which needs no
/// administrator, `%LOCALAPPDATA%\Programs`.
#[cfg(unix)]
pub fn install_prefix(read: impl FnMut(&str) -> Option<std::ffi::OsString>) -> String {
    let _ = read;
    "/usr/local".into()
}
#[cfg(windows)]
pub fn install_prefix(mut read: impl FnMut(&str) -> Option<std::ffi::OsString>) -> String {
    let set = |value: Option<std::ffi::OsString>| value.filter(|value| !value.is_empty()).map(PathBuf::from);
    let local = set(read("LOCALAPPDATA")).or_else(|| set(read("USERPROFILE")).map(|home| home.join("AppData/Local")));
    match local {
        Some(local) => local.join("Programs").display().to_string(),
        None => r"C:\Program Files".into(),
    }
}

/// Returns the exit code of a program, or if a signal ended it, as on Unix, 128 and the signal, as a shell reports it.
#[cfg(unix)]
pub fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|signal| 128 + signal))
}
#[cfg(windows)]
pub fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    status.code()
}

/// Create a file which is a program, executable on Unix, failing if it exists.
///
/// # Errors
///
/// If it exists, or cannot be created.
#[cfg(unix)]
pub fn create_program(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(path)
}
#[cfg(windows)]
pub fn create_program(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_where_the_platform_does() {
        let env = |vars: &'static [(&str, &str)]| {
            move |name: &str| vars.iter().find(|(var, _)| *var == name).map(|(_, value)| value.into())
        };
        if cfg!(windows) {
            let local = install_prefix(env(&[("LOCALAPPDATA", r"C:\Users\a\AppData\Local"), ("USERPROFILE", "x")]));
            assert_eq!(Path::new(&local), Path::new(r"C:\Users\a\AppData\Local\Programs"));
            let home = install_prefix(env(&[("LOCALAPPDATA", ""), ("USERPROFILE", r"C:\Users\a")]));
            assert_eq!(Path::new(&home), Path::new(r"C:\Users\a\AppData\Local\Programs"));
            assert_eq!(install_prefix(env(&[])), r"C:\Program Files");
        } else {
            assert_eq!(install_prefix(env(&[("LOCALAPPDATA", "x")])), "/usr/local");
        }
    }

    #[test]
    fn finds_programs_and_identifies_files() {
        let dir = std::env::temp_dir().join(format!("jig-platform-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "tool.exe" } else { "tool" };
        drop(create_program(&dir.join(name)).unwrap());
        assert!(create_program(&dir.join(name)).is_err());
        std::fs::write(dir.join("data"), "").unwrap();
        // a program is found by its name, as Windows runs one without its extension
        assert_eq!(program_in(&dir, "tool"), Some(dir.join(name)));
        assert_eq!(program_in(&dir, "data"), None);
        assert_eq!(program_in(&dir, "missing"), None);
        // which a link reaches as the same file
        link(&dir.join("data"), &dir.join("linked")).unwrap();
        let id = |path: &Path| file_id(path, &std::fs::metadata(path).unwrap());
        if cfg!(unix) {
            assert_eq!(id(&dir.join("data")), id(&dir.join("linked")));
        }
        assert_ne!(id(&dir.join("data")), id(&dir.join(name)));
        // and a file written by none is read-only
        set_mode(&dir.join("data"), 0o444).unwrap();
        assert_eq!(mode(&std::fs::metadata(dir.join("data")).unwrap()) & 0o222, 0);
        set_mode(&dir.join("data"), 0o644).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
