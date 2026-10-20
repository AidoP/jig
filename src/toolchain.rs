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
