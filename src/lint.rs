//! `jig lint`, which lints crates with clippy, and shows the diagnostics they saved.

use std::io::{IsTerminal, Write};

use crate::Error;
use crate::configured::{Configured, Output};

/// Lint the crates which are edited, or only those named, without evaluating the configuration.
///
/// The lints are brought up to date by ninja, which lints every crate it can even if one fails, then the diagnostics
/// each saved are shown, so that they are shown even when nothing needed linting. It fails if a lint did, such as for a
/// lint which is denied, or with `deny_warnings`, if there is a warning, which is found in what was saved, so that the
/// lints are not run again.
pub fn lint(names: &[String], deny_warnings: bool) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    let targets: Vec<_> = if names.is_empty() {
        vec!["lint".to_string()]
    } else {
        names.iter().map(|name| format!("lint/{name}")).collect()
    };
    let mut ninja = project.ninja();
    ninja.args(["-k", "0"]).args(&targets);
    let linted = match ninja.status() {
        Ok(status) => status.success(),
        Err(error) => return Err(Error::Exec(ninja, error)),
    };
    // read after ninja, which reconfigures if needed, so that it is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    // an unknown crate also fails ninja, as it has no target, but is the more useful error
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
    // the diagnostics are saved in colour, which is removed where stdout is not a terminal
    let pretty = std::io::stdout().is_terminal();
    let mut stdout = std::io::stdout().lock();
    let mut warned = false;
    for lint in lints {
        // a lint which has not run, as a crate it uses failed to check, has no diagnostics
        let Ok(diagnostics) = std::fs::read_to_string(project.root.join(lint.diagnostics)) else {
            continue;
        };
        let plain = without_colour(&diagnostics);
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

/// Returns text without the escape sequences which colour it, `ESC [ ... m`, and those which rustc writes to reset
/// the character set, `ESC ( B`.
fn without_colour(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            },
            Some('(') => _ = chars.next(),
            _ => (),
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_colour() {
        let coloured = "\x1b[0m\x1b[1m\x1b[33mwarning\x1b[0m\x1b[0m\x1b[1m: unneeded `return`\x1b(B\x1b[m\n";
        assert_eq!(without_colour(coloured), "warning: unneeded `return`\n");
        assert_eq!(without_colour("plain é\n"), "plain é\n");
    }

    #[test]
    fn finds_warnings() {
        assert!(has_warning("error: denied\nwarning: unneeded `return`\n"));
        assert!(has_warning("warning[E0170]: pattern binding\n"));
        // an error, or a note which mentions a warning, is not one
        assert!(!has_warning("error: denied\n  = note: `#[warn(clippy::x)]`\n  warning: in a snippet\n"));
        assert!(!has_warning(""));
    }
}
