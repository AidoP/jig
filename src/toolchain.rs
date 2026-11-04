//! rustup toolchains, which are selected with `+<toolchain>` before the command, as with cargo.
//!
//! A toolchain is selected by running rustup's proxies, such as `rustc`, with `RUSTUP_TOOLCHAIN`, which jig gives each
//! command it runs, and `build.ninja` gives each of its commands, once the project is configured with it.

use std::ffi::OsString;
use std::iter::Peekable;
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

/// Returns what identifies a toolchain, by which rig caches what it compiles, and finds a worker which has it: a hash
/// of rustc's version, with its commit and host, from `rustc -vV`, and of the files which rustup writes to the sysroot
/// when it installs or updates the toolchain, or without them, of rustc in the sysroot, by their paths in the sysroot,
/// so that the toolchain has the same identity wherever it is installed. The files hashed which exist are returned too.
///
/// The name the toolchain was chosen by is not part of it, as one toolchain has several, and a command which is given
/// one has it in its action.
pub fn identity(verbose_version: &str, sysroot: &std::path::Path) -> (String, Vec<std::path::PathBuf>) {
    let mut key = rig::cache::Key::new("jig toolchain");
    key.part(verbose_version.as_bytes());
    let rustup = ["lib/rustlib/multirust-channel-manifest.toml", "lib/rustlib/components"];
    let files: &[&str] = if rustup.iter().any(|path| sysroot.join(path).is_file()) { &rustup } else { &["bin/rustc"] };
    let mut found = Vec::new();
    for path in files {
        key.part(path.as_bytes());
        let hash = rig::blake3::hash_file(&sysroot.join(path)).ok();
        key.hash(hash.as_ref());
        if hash.is_some() {
            found.push(sysroot.join(path));
        }
    }
    (rig::cache::hex(&key.finish()), found)
}

/// Returns the toolchains a worker has, by their identities: each that rustup has, which is run with
/// `RUSTUP_TOOLCHAIN`, and the `rustc` which is run without it, which is the only one without rustup.
pub fn installed() -> Vec<rig::worker::Toolchain> {
    let mut rustup = Command::new("rustup");
    rustup.args(["toolchain", "list"]);
    let listed = rustup.output().ok().filter(|output| output.status.success());
    let listed = listed.map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default();
    let names = listed.lines().filter_map(|line| line.split_whitespace().next()).filter(|name| !name.is_empty());
    let mut toolchains: Vec<rig::worker::Toolchain> = names
        .filter_map(|name| {
            let identity = identify(Some(name))?;
            Some(rig::worker::Toolchain { identity, env: vec![(VARIABLE.into(), name.into())] })
        })
        .collect();
    if let Some(identity) = identify(None)
        && !toolchains.iter().any(|toolchain| toolchain.identity == identity)
    {
        toolchains.push(rig::worker::Toolchain { identity, env: Vec::new() });
    }
    toolchains
}

/// Returns the identity of the toolchain `rustc` runs, with a toolchain if one is given, if it can be run.
fn identify(toolchain: Option<&str>) -> Option<String> {
    let output = |argument: &str| {
        let mut rustc = Command::new("rustc");
        rustc.arg(argument);
        apply(&mut rustc, toolchain);
        let output = rustc.output().ok().filter(|output| output.status.success())?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let (verbose_version, sysroot) = (output("-vV")?, output("--print=sysroot")?);
    Some(identity(&verbose_version, std::path::Path::new(&sysroot)).0)
}
