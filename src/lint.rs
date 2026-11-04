//! `jig lint`, which lints crates with clippy, and shows the diagnostics they saved.

use std::io::Write;

use crate::Error;
use crate::builder::Request;
use crate::configured::{Configured, Output};

/// Lint the crates which are edited, or only those named, without evaluating the configuration.
///
/// The lints are brought up to date by building them, which lints every crate it can even if one fails, then the
/// diagnostics each saved are shown, so that they are shown even when nothing needed linting. It fails if a lint did,
/// such as for a lint which is denied, or with `deny_warnings`, if there is a warning, which is found in what was saved,
/// so that the lints are not run again.
pub fn lint(names: &[String], deny_warnings: bool) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    let targets: Vec<_> = if names.is_empty() {
        vec!["lint".to_string()]
    } else {
        names.iter().map(|name| format!("lint/{name}")).collect()
    };
    let targets: Vec<_> = targets.iter().map(String::as_str).collect();
    let linted = project.builder().build(&Request { targets: &targets, keep_going: true, ..Default::default() })?;
    // read after building, which reconfigures if needed, so that it is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    // an unknown crate also fails the build, as it has no target, but is the more useful error
    let mut lints = Vec::new();
    for name in names {
        match database.lints.iter().find(|lint| lint.name == name) {
            Some(lint) => lints.push(lint),
            None => return Err(Error::UnknownLint(name.clone())),
        }
    }
    if names.is_empty() {
        lints = database.lints.iter().collect();
    }
    // the diagnostics are saved in colour, which is removed where stdout is not in colour
    let pretty = crate::colour(&std::io::stdout());
    let mut stdout = std::io::stdout().lock();
    let mut warned = false;
    for lint in lints {
        // a lint which has not run, as a crate it uses failed to check, has no diagnostics
        let Ok(diagnostics) = std::fs::read_to_string(project.root.join(lint.diagnostics)) else {
            continue;
        };
        let plain = String::from_utf8_lossy(&rig::exec::without_colour(diagnostics.as_bytes())).into_owned();
        warned |= has_warning(&plain);
        let _ = stdout.write_all(if pretty { diagnostics } else { plain }.as_bytes());
    }
    if !linted {
        return Err(Error::LintFailed);
    }
    if deny_warnings && warned {
        return Err(Error::LintWarnings);
    }
    Ok(())
}

/// Whether the diagnostics of rustc, without colour, have a warning, whose heading starts a line, as
/// `warning: <message>` or `warning[<code>]: <message>`.
fn has_warning(diagnostics: &str) -> bool {
    diagnostics.lines().any(|line| line.starts_with("warning:") || line.starts_with("warning["))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_warnings() {
        assert!(has_warning("error: denied\nwarning: unneeded `return`\n"));
        assert!(has_warning("warning[E0170]: pattern binding\n"));
        // an error, or a note which mentions a warning, is not one
        assert!(!has_warning("error: denied\n  = note: `#[warn(clippy::x)]`\n  warning: in a snippet\n"));
        assert!(!has_warning(""));
    }
}
