//! rustup toolchains, which are selected with `+<toolchain>` before the command, as with cargo.
//!
//! A toolchain is selected by running rustup's proxies, such as `rustc`, with `RUSTUP_TOOLCHAIN`, which jig gives each
//! command it runs, and `build.ninja` gives each of its commands, once the project is configured with it.

use std::ffi::OsString;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// The environment variable which selects the toolchain of rustup's proxies.
const VARIABLE: &str = "RUSTUP_TOOLCHAIN";

/// Take the toolchain from the arguments, if the first is `+<toolchain>`, checking that rustup has it.
pub fn parse(args: &mut Peekable<impl Iterator<Item = OsString>>) -> Result<Option<String>, Error> {
    let Some(arg) = args.next_if(|arg| arg.to_string_lossy().starts_with('+')) else {
        return Ok(None);
    };
    let name = arg.to_string_lossy()[1..].to_string();
    if name.is_empty() {
        return Err(Error::MissingArg("toolchain"));
    }
    let mut rustup = Command::new("rustup");
    rustup.args(["which", "rustc", "--toolchain", &name]);
    match rustup.output() {
        Ok(output) if output.status.success() => Ok(Some(name)),
        Ok(output) => {
            let message = String::from_utf8_lossy(&output.stderr);
            let message = message.trim().trim_start_matches("error: ");
            Err(Error::Toolchain(name, message.into()))
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(Error::Toolchain(name, "selecting a toolchain needs rustup, which was not found".into()))
        },
        Err(error) => Err(Error::Exec(Box::new(rustup), error)),
    }
}

/// Run a command with a toolchain, if there is one.
pub fn apply(command: &mut Command, toolchain: Option<&str>) {
    if let Some(toolchain) = toolchain {
        command.env(VARIABLE, toolchain);
    }
}

/// Returns the prefix of a command run by `sh` which runs it with a toolchain, if there is one.
pub fn shell_prefix(toolchain: Option<&str>) -> String {
    match toolchain {
        Some(toolchain) => format!("{VARIABLE}={} ", crate::generate::shell_quote(toolchain)),
        None => String::new(),
    }
}

/// The channel's manifest which rustup writes to a toolchain's sysroot when it installs or updates it.
const MANIFEST: &str = "lib/rustlib/multirust-channel-manifest.toml";

/// Returns what identifies a toolchain, by which rig caches what it compiles, and finds a worker which has it: a hash
/// of rustc's version, with its commit and host, from `rustc -vV`, and of the channel's manifest which rustup writes to
/// the sysroot when it installs or updates the toolchain, or without it, of rustc in the sysroot, so that the toolchain
/// has the same identity wherever it is installed, and whichever components it has. The file hashed is returned too,
/// if it exists.
///
/// The name the toolchain was chosen by is not part of it, as one toolchain has several, and a command which is given
/// one has it in its action.
pub fn identity(verbose_version: &str, sysroot: &Path) -> (String, Option<PathBuf>) {
    let mut key = rig::cache::Key::new("jig toolchain");
    key.part(verbose_version.as_bytes());
    let file = if sysroot.join(MANIFEST).is_file() { MANIFEST } else { "bin/rustc" };
    key.part(file.as_bytes());
    let hash = rig::blake3::hash_file(&sysroot.join(file)).ok();
    key.hash(hash.as_ref());
    (rig::cache::hex(&key.finish()), hash.map(|_| sysroot.join(file)))
}

/// Returns what identifies a toolchain's release on any host, as [`identity`] does but for the host in rustc's
/// version, by which a worker on another host may run what does not depend on the host.
pub fn release_identity(verbose_version: &str, sysroot: &Path) -> String {
    let verbose_version: String = verbose_version.lines().filter(|line| !line.starts_with("host:")).collect();
    identity(&verbose_version, sysroot).0
}

/// Returns the name rustup installs a toolchain by, from the manifest of its channel in its sysroot: the version of a
/// stable release, such as `1.97.0`, or the channel and date of a nightly or beta, such as `nightly-2026-11-03`, or
/// `None` without the manifest, as for a toolchain which rustup did not install.
pub fn install_name(sysroot: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(sysroot.join(MANIFEST)).ok()?;
    let value = |line: &str, key: &str| {
        let value = line.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
        Some(value.strip_prefix('"')?.strip_suffix('"')?.to_string())
    };
    let date = manifest.lines().find_map(|line| value(line, "date"))?;
    let mut lines = manifest.lines().skip_while(|line| line.trim() != "[pkg.rustc]");
    let version = lines.find_map(|line| value(line, "version"))?;
    let version = version.split_whitespace().next()?;
    let name = match version.split_once('-') {
        Some((_, channel @ ("nightly" | "beta"))) => format!("{channel}-{date}"),
        Some(_) => return None,
        None => version.to_string(),
    };
    // a name is given to rustup as an argument, so it has only what names have
    let valid = name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    valid.then_some(name)
}

/// Returns what a worker has, with its tags: the host of the `rustc` which is run without a toolchain, each toolchain
/// rustup has, which is run with `RUSTUP_TOOLCHAIN`, and that `rustc`, which is the only one without rustup.
///
/// What identifies each toolchain is kept in a file, `known`, by its sysroot, with the size and time of the file its
/// identity is of, so that rustc is run only for a toolchain which is new, or has been updated.
pub fn capabilities(tags: Vec<String>, known: &Path) -> rig::worker::Capabilities {
    let mut identified = Known::read(known);
    let mut rustup = Command::new("rustup");
    rustup.args(["toolchain", "list", "-v"]);
    let listed = rustup.output().ok().filter(|output| output.status.success());
    let listed = listed.map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default();
    // each line is the name, what is active or the default, and the sysroot, which is last
    let listed = listed.lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        Some((words.next()?, words.next_back()?))
    });
    let mut toolchains: Vec<rig::worker::Toolchain> = listed
        .filter_map(|(name, sysroot)| identified.toolchain(Some(name), Path::new(sysroot)))
        .collect();
    let default = rustc_output(None, "--print=sysroot");
    let default = default.and_then(|sysroot| identified.toolchain(None, Path::new(&sysroot)));
    let host = default.as_ref().map(|toolchain| toolchain.host.clone()).unwrap_or_default();
    if let Some(default) = default
        && !toolchains.iter().any(|toolchain| toolchain.identity == default.identity)
    {
        toolchains.push(default);
    }
    identified.write(known);
    rig::worker::Capabilities { host, tags, toolchains, installs: false }
}

/// Returns the trimmed output of rustc with an argument, with a toolchain if one is given, if it succeeds.
fn rustc_output(toolchain: Option<&str>, argument: &str) -> Option<String> {
    let mut rustc = Command::new("rustc");
    rustc.arg(argument);
    apply(&mut rustc, toolchain);
    let output = rustc.output().ok().filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// What identifies each toolchain a worker has found, by its sysroot: the size and time of the file its identity is
/// of, its identity, the identity of its release, its release and its host.
#[derive(Default)]
struct Known {
    toolchains: std::collections::BTreeMap<String, [String; 5]>,
    changed: bool,
}

impl Known {
    /// Read what is known, from lines of tab-separated fields, of which those which are not valid are left out.
    fn read(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let toolchains = text.lines().filter_map(|line| {
            let mut fields = line.split('\t').map(String::from);
            let sysroot = fields.next()?;
            let fields: [String; 5] = fields.collect::<Vec<_>>().try_into().ok()?;
            Some((sysroot, fields))
        });
        Self { toolchains: toolchains.collect(), changed: false }
    }
    /// Write what is known, if it changed, replacing the file once it is written.
    fn write(&self, path: &Path) {
        if !self.changed {
            return;
        }
        let lines = self.toolchains.iter().map(|(sysroot, fields)| format!("{sysroot}\t{}\n", fields.join("\t")));
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, lines.collect::<String>()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
    /// Returns the toolchain of a sysroot, run with a toolchain if one is given, identifying it with rustc unless it
    /// is known, with the targets whose standard libraries it has now.
    fn toolchain(&mut self, toolchain: Option<&str>, sysroot: &Path) -> Option<rig::worker::Toolchain> {
        let file = if sysroot.join(MANIFEST).is_file() { sysroot.join(MANIFEST) } else { sysroot.join("bin/rustc") };
        let metadata = std::fs::metadata(&file).ok()?;
        // as Unix's `mtime` and `mtime_nsec` have it, for a file changed since 1970
        let modified = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        let stamp = format!("{}-{}.{}", metadata.len(), modified.as_secs(), modified.subsec_nanos());
        let key = sysroot.display().to_string();
        let found = self.toolchains.get(&key).filter(|fields| fields[0] == stamp).cloned();
        let [_, identity, release_identity, release, host] = if let Some(fields) = found {
            fields
        } else {
            let verbose_version = rustc_output(toolchain, "-vV")?;
            let lines = || verbose_version.lines();
            let field = |name: &str| lines().find_map(|line| line.strip_prefix(name)).map(str::trim);
            let field = |name: &str| field(name).unwrap_or_default().to_string();
            let fields = [
                stamp,
                identity(&verbose_version, sysroot).0,
                release_identity(&verbose_version, sysroot),
                field("release:"),
                field("host:"),
            ];
            // a sysroot which cannot be written as a field is identified each time
            if !key.contains(['\t', '\n']) {
                self.toolchains.insert(key, fields.clone());
                self.changed = true;
            }
            fields
        };
        // the targets whose standard libraries are installed have their libraries in the sysroot
        let targets = std::fs::read_dir(sysroot.join("lib/rustlib")).into_iter().flatten().flatten();
        let mut targets: Vec<String> = targets
            .filter(|entry| entry.path().join("lib").is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        targets.sort();
        Some(rig::worker::Toolchain {
            identity,
            name: toolchain.unwrap_or_default().into(),
            release,
            host,
            targets,
            release_identity,
            env: toolchain.map(|name| (VARIABLE.into(), name.into())).into_iter().collect(),
        })
    }
}

/// Install a toolchain with rustup, by its name, with its minimal profile, and the standard library of a target, if one
/// is given, which is added to it if it is installed, returning whether it was.
pub fn install(name: &str, target: Option<&str>) -> bool {
    let mut rustup = Command::new("rustup");
    rustup.args(["toolchain", "install", name, "--profile", "minimal", "--no-self-update"]);
    // a target is given to rustup as an argument, so it has only what targets have
    if let Some(target) = target {
        if !target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
            return false;
        }
        rustup.args(["--target", target]);
    }
    // what rustup prints goes to stderr, which a client keeps to tell why the worker failed
    rustup.stdout(std::process::Stdio::null());
    rustup.status().is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_name_rustup_installs_by() {
        let dir = std::env::temp_dir().join(format!("jig-toolchain-{}", std::process::id()));
        let manifest = |date: &str, version: &str| {
            std::fs::create_dir_all(dir.join("lib/rustlib")).unwrap();
            let rust = "[pkg.rust]\nversion = \"0.0.0\"\n";
            let rustc = format!("[pkg.rustc]\nversion = \"{version}\"\n");
            let contents = format!("manifest-version = \"2\"\ndate = \"{date}\"\n\n{rust}\n{rustc}");
            std::fs::write(dir.join(MANIFEST), contents).unwrap();
            install_name(&dir)
        };
        assert_eq!(manifest("2026-07-09", "1.97.0 (2d8144b78 2026-07-07)").as_deref(), Some("1.97.0"));
        let nightly = manifest("2026-11-03", "1.101.0-nightly (1c3a65069 2026-11-02)");
        assert_eq!(nightly.as_deref(), Some("nightly-2026-11-03"));
        assert_eq!(manifest("2026-10-30", "1.100.0-beta.4 (abc 2026-10-29)"), None);
        assert_eq!(manifest("2026-10-30", "1.100.0-beta (abc 2026-10-29)").as_deref(), Some("beta-2026-10-30"));
        assert_eq!(manifest("$(reboot)", "1.101.0-nightly (1c3a65069 2026-11-02)"), None);
        // a toolchain's identity is of its manifest, wherever it is
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("lib/rustlib")).unwrap();
        std::fs::copy(dir.join(MANIFEST), elsewhere.join(MANIFEST)).unwrap();
        let (identity, file) = super::identity("rustc 1.101.0-nightly", &dir);
        assert_eq!(file, Some(dir.join(MANIFEST)));
        assert_eq!(super::identity("rustc 1.101.0-nightly", &elsewhere).0, identity);
        assert_ne!(super::identity("rustc 1.100.0-nightly", &elsewhere).0, identity);
        // its release's identity is the same on another host
        let on = |host: &str| format!("rustc 1.101.0-nightly\nhost: {host}\nrelease: 1.101.0-nightly");
        assert_ne!(super::identity(&on("x86_64"), &dir).0, super::identity(&on("aarch64"), &dir).0);
        assert_eq!(release_identity(&on("x86_64"), &dir), release_identity(&on("aarch64"), &elsewhere));
        assert_ne!(release_identity(&on("x86_64"), &dir), super::identity(&on("x86_64"), &dir).0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
