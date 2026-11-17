//! `jig lint`, which lints crates with clippy, and shows the diagnostics they saved.

use std::io::Write;

use crate::Error;
use crate::configured::{Configured, Output, Request};
use crate::rig_builder::Options;

/// Lint the crates which are edited, and the sources of C of what has `c/tidy`, or only those named, without
/// evaluating the configuration.
///
/// The lints are brought up to date by building them, which lints every crate it can even if one fails, then the
/// diagnostics each saved are shown, so that they are shown even when nothing needed linting. It fails if a lint did,
/// such as for a lint which is denied, or with `deny_warnings`, if there is a warning, which is found in what was saved,
/// so that the lints are not run again. It builds as `build` says, which goes on after any failure unless `-k` is given.
///
/// The sources of C are checked by clang-tidy each time, once what they need is built, from the compile commands jig
/// writes for clangd, with the checks `c/tidy` gives. Where `deny_warnings`, a warning fails.
pub fn lint(names: &[String], deny_warnings: bool, build: Options) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    // the configuration was brought up to date, so the database is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let (mut lints, mut tidies) = (Vec::new(), Vec::new());
    let mut targets = Vec::new();
    for name in names {
        let lint = database.lints.iter().find(|lint| lint.name == name);
        let tidy = database.tidies.iter().find(|tidy| tidy.name == name);
        if lint.is_none() && tidy.is_none() {
            return Err(Error::UnknownLint(name.clone()));
        }
        targets.extend(lint.map(|_| format!("lint/{name}")));
        targets.extend(tidy.map(|tidy| tidy.target.to_string()));
        lints.extend(lint);
        tidies.extend(tidy);
    }
    if names.is_empty() {
        targets.push("lint".to_string());
        lints = database.lints.iter().collect();
        tidies = database.tidies.iter().collect();
    }
    let targets: Vec<_> = targets.iter().map(String::as_str).collect();
    let mut linted = project.builder().build(&Request { targets: &targets, keep_going: true, build, ..Default::default() })?;
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
    let _ = stdout.flush();
    drop(stdout);
    for tidy in tidies {
        let mut command = std::process::Command::new("clang-tidy");
        command.current_dir(&project.root).arg("-p").arg(&project.build_dir);
        command.arg(format!("--checks={}", tidy.checks)).arg("--quiet");
        command.arg(if pretty { "--use-color" } else { "--use-color=false" });
        if deny_warnings {
            command.arg("--warnings-as-errors=*");
        }
        command.args(&tidy.sources);
        match command.status() {
            // a warning which is denied is an error to clang-tidy, as an error is
            Ok(status) if !status.success() && deny_warnings => warned = true,
            Ok(status) => linted &= status.success(),
            Err(error) => return Err(Error::Exec(Box::new(command), error)),
        }
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
