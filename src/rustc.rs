//! What configuring a project asks rustc: its sysroot, its host, the targets it knows and its version.
//!
//! The answers are kept in the database, with a key of what chose and identifies the toolchain which gave them, so that
//! the project is configured again without running rustc while none of it has changed, as for an edit of a `Jig` file.
//! rustup's proxy, `rustc`, chooses the toolchain by `RUSTUP_TOOLCHAIN`, a `rust-toolchain` or `rust-toolchain.toml`
//! file in the working directory or a directory which contains it, and its settings, of overrides by directory and
//! the default, in `$RUSTUP_HOME/settings.toml`, while `rustc` is found by `PATH`. The key is of each of these, with
//! the size and time of each file, and those of the file a toolchain's identity is of, which rustup writes when it
//! updates the toolchain in place, or of rustc in the sysroot, as [`crate::toolchain::identity`] finds it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// What rustc answered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answers {
    /// The key of what chose and identifies the toolchain, by which the answers are known again.
    pub key: String,
    pub sysroot: String,
    pub host: String,
    pub targets: Vec<String>,
    /// The output of `rustc -vV`.
    pub verbose_version: String,
}

impl Answers {
    /// Returns the version of rustc, from the `release` line of the output of `rustc -vV`.
    pub fn version(&self) -> Result<String, Error> {
        let version = self.verbose_version.lines().find_map(|line| line.strip_prefix("release: "));
        version.map(|version| version.trim().into()).ok_or_else(|| Error::InvalidOutput(Box::new(command(&["-vV"], None))))
    }
}

/// Returns rustc's answers, with a toolchain if one is given, as `known` has them if nothing which chose or identifies
/// the toolchain has changed since, or otherwise from rustc.
pub fn ask(toolchain: Option<&str>, known: Option<Answers>) -> Result<Answers, Error> {
    // what chose the toolchain is found before rustc runs, so that what changes while it runs is found to have changed
    let chosen = chosen(toolchain);
    if let Some(known) = known
        && key(&chosen, &known.sysroot) == known.key
    {
        return Ok(known);
    }
    // the version is asked alone, as rustc prints nothing more after it, while running both at once takes as long as one
    let (printed, verbose_version) = std::thread::scope(|scope| {
        let verbose_version = scope.spawn(|| output(&["-vV"], toolchain));
        let printed = output(&["--print=sysroot", "--print=host-tuple", "--print=target-list"], toolchain);
        (printed, verbose_version.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
    });
    let (printed, verbose_version) = (printed?, verbose_version?);
    let mut lines = printed.lines().map(String::from);
    let (Some(sysroot), Some(host)) = (lines.next(), lines.next()) else {
        return Err(Error::InvalidOutput(Box::new(command(&["--print=sysroot", "--print=host-tuple"], toolchain))));
    };
    let targets = lines.collect();
    Ok(Answers { key: key(&chosen, &sysroot), sysroot, host, targets, verbose_version })
}

/// Returns what chose the toolchain, as it is when it is asked, as text.
fn chosen(toolchain: Option<&str>) -> String {
    let var = |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
    // a toolchain which is given is given to rustc by the variable, in place of any it inherits
    let selected = toolchain.map(String::from).or_else(|| var("RUSTUP_TOOLCHAIN"));
    let mut lines = vec![format!("RUSTUP_TOOLCHAIN={selected:?}")];
    lines.extend(["PATH", "RUSTUP_HOME", "CARGO_HOME"].map(|name| format!("{name}={:?}", var(name))));
    let path = var("PATH").unwrap_or_default();
    let rustc = std::env::split_paths(&path).find_map(|dir| crate::platform::program_in(&dir, "rustc"));
    lines.push(format!("rustc {}", rustc.as_deref().map_or_else(|| "missing".into(), described)));
    lines.extend(rustup_home().map(|home| described(&home.join("settings.toml"))));
    // the toolchain file closest to the working directory is the one rustup reads
    if let Ok(dir) = std::env::current_dir() {
        let files = dir.ancestors().flat_map(|dir| ["rust-toolchain", "rust-toolchain.toml"].map(|name| dir.join(name)));
        lines.extend(files.map(|file| described(&file)));
    }
    lines.join("\n")
}

/// Returns the key of what chose the toolchain, and of the file its identity is of, in its sysroot.
fn key(chosen: &str, sysroot: &str) -> String {
    let mut key = rig::cache::Key::new("jig rustc");
    key.part(chosen.as_bytes());
    key.part(sysroot.as_bytes());
    key.part(described(&crate::toolchain::identity_file(Path::new(sysroot))).as_bytes());
    rig::cache::hex(&key.finish())
}

/// Returns a path with the size and time of the file it is of, following symbolic links, or that it is missing.
fn described(path: &Path) -> String {
    let stamp = crate::toolchain::stamp(path);
    format!("{} {}", path.display(), stamp.as_deref().unwrap_or("missing"))
}

/// Returns rustup's home, `RUSTUP_HOME`, or `.rustup` in the user's home directory.
fn rustup_home() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    var("RUSTUP_HOME").or_else(|| var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|home| home.join(".rustup")))
}

/// Returns rustc with arguments, with a toolchain if one is given.
pub fn command(arguments: &[&str], toolchain: Option<&str>) -> Command {
    let mut command = Command::new("rustc");
    command.args(arguments);
    crate::toolchain::apply(&mut command, toolchain);
    command
}

/// Returns the trimmed output of rustc with arguments, with a toolchain if one is given.
pub fn output(arguments: &[&str], toolchain: Option<&str>) -> Result<String, Error> {
    let mut command = command(arguments, toolchain);
    let output = match command.output() {
        Err(error) => return Err(Error::Exec(Box::new(command), error)),
        Ok(output) => output,
    };
    if !output.status.success() {
        return Err(Error::InvalidOutput(Box::new(command)));
    }
    match String::from_utf8(output.stdout) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().into()),
        _ => Err(Error::InvalidOutput(Box::new(command))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_known_again_until_the_toolchain_changes() {
        // what was answered is known again, without asking rustc, while nothing which chose the toolchain has changed
        let sysroot = std::env::temp_dir().join(format!("jig-rustc-{}", std::process::id()));
        std::fs::create_dir_all(sysroot.join("bin")).unwrap();
        let sysroot_str = sysroot.to_str().unwrap();
        let known = Answers {
            key: key(&chosen(None), sysroot_str),
            sysroot: sysroot_str.into(),
            host: "known".into(),
            targets: vec!["known".into()],
            verbose_version: "rustc 0.0.0".into(),
        };
        assert_eq!(ask(None, Some(known.clone())).ok().as_ref(), Some(&known));
        // a toolchain which is given chooses it
        assert_ne!(chosen(Some("nightly")), chosen(None));
        assert_eq!(chosen(Some("nightly")), chosen(Some("nightly")));
        // as does the file the toolchain's identity is of, when it is installed or updated in place
        let before = key("chosen", sysroot_str);
        std::fs::write(sysroot.join("bin/rustc"), "rustc").unwrap();
        let installed = key("chosen", sysroot_str);
        assert_ne!(installed, before);
        assert_eq!(key("chosen", sysroot_str), installed);
        std::fs::write(sysroot.join("bin/rustc"), "updated").unwrap();
        assert_ne!(key("chosen", sysroot_str), installed);
        assert_ne!(key("other", sysroot_str), key("chosen", sysroot_str));
        let _ = std::fs::remove_dir_all(&sysroot);
    }
}
