//! `jig fmt`, which formats the project's sources: its crates with rustfmt, and its C and C++ with clang-format.

use std::process::Command;

use crate::Error;
use crate::configured::{Configured, Output};

/// The edition the crates are formatted with, which they are built with.
const EDITION: &str = "2024";

/// Format the roots of the crates which are edited, which rustfmt follows to their modules, with the project's
/// toolchain, and the sources of C and C++, with clang-format if it is installed, each by the configuration files they
/// find, such as `rustfmt.toml` and `.clang-format`. With `check`, nothing is changed, and it fails if a file is not
/// formatted, showing how it would change.
pub fn fmt(check: bool) -> Result<(), Error> {
    let project = Configured::find(Output::Stderr)?;
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let mut formatted = true;
    if !database.rust_roots.is_empty() {
        let mut rustfmt = Command::new("rustfmt");
        rustfmt.current_dir(&project.root).args(["--edition", EDITION]);
        if check {
            rustfmt.arg("--check");
        }
        if let Some(toolchain) = &project.toolchain {
            rustfmt.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        rustfmt.args(&database.rust_roots);
        formatted &= run(rustfmt)?;
    }
    if !database.c_sources.is_empty() {
        let mut clang_format = Command::new("clang-format");
        clang_format.current_dir(&project.root);
        clang_format.args(if check { ["--dry-run", "--Werror"] } else { ["-i", "--Werror"] });
        clang_format.args(&database.c_sources);
        match clang_format.status() {
            Ok(status) => formatted &= status.success(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("note: the sources of C are not formatted, as clang-format is not installed");
            },
            Err(error) => return Err(Error::Exec(Box::new(clang_format), error)),
        }
    }
    if formatted { Ok(()) } else { Err(Error::NotFormatted) }
}

/// Run a formatter, returning whether it succeeded.
fn run(mut command: Command) -> Result<bool, Error> {
    match command.status() {
        Ok(status) => Ok(status.success()),
        Err(error) => Err(Error::Exec(Box::new(command), error)),
    }
}
