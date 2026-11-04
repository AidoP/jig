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
pub fn capabilities(tags: Vec<String>) -> rig::worker::Capabilities {
    let mut rustup = Command::new("rustup");
    rustup.args(["toolchain", "list"]);
    let listed = rustup.output().ok().filter(|output| output.status.success());
    let listed = listed.map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default();
    let names = listed.lines().filter_map(|line| line.split_whitespace().next()).filter(|name| !name.is_empty());
    let mut toolchains: Vec<rig::worker::Toolchain> = names.filter_map(|name| identify(Some(name))).collect();
    let default = identify(None);
    let host = default.as_ref().map(|toolchain| toolchain.host.clone()).unwrap_or_default();
    if let Some(default) = default
        && !toolchains.iter().any(|toolchain| toolchain.identity == default.identity)
    {
        toolchains.push(default);
    }
    rig::worker::Capabilities { host, tags, toolchains }
}

/// Returns the toolchain `rustc` runs, with a toolchain if one is given, if it can be run.
fn identify(toolchain: Option<&str>) -> Option<rig::worker::Toolchain> {
    let output = |argument: &str| {
        let mut rustc = Command::new("rustc");
        rustc.arg(argument);
        apply(&mut rustc, toolchain);
        let output = rustc.output().ok().filter(|output| output.status.success())?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let (verbose_version, sysroot) = (output("-vV")?, output("--print=sysroot")?);
    let field = |name: &str| verbose_version.lines().find_map(|line| line.strip_prefix(name)).map(str::trim);
    let field = |name: &str| field(name).unwrap_or_default();
    let sysroot = Path::new(&sysroot);
    // the targets whose standard libraries are installed have their libraries in the sysroot
    let targets = std::fs::read_dir(sysroot.join("lib/rustlib")).into_iter().flatten().flatten();
    let mut targets: Vec<String> = targets
        .filter(|entry| entry.path().join("lib").is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    targets.sort();
    Some(rig::worker::Toolchain {
        identity: identity(&verbose_version, sysroot).0,
        name: toolchain.unwrap_or_default().into(),
        release: field("release:").into(),
        host: field("host:").into(),
        targets,
        env: toolchain.map(|name| (VARIABLE.into(), name.into())).into_iter().collect(),
    })
}

/// Install a toolchain with rustup, by its name, with its minimal profile, returning whether it was.
pub fn install(name: &str) -> bool {
    let mut rustup = Command::new("rustup");
    rustup.args(["toolchain", "install", name, "--profile", "minimal", "--no-self-update"]);
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
