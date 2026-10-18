//! `jig check`, the flycheck of rust-analyzer, which replays saved diagnostics.

use crate::configured::{Configured, Output};
use crate::database::Flycheck;

/// Returns the diagnostics to replay for a flycheck: its own, then those of the flychecks it depends on, directly or
/// not, which are edited.
pub fn replayed<'a>(flycheck: &Flycheck<'a>, flychecks: &[Flycheck<'a>]) -> Vec<&'a str> {
    let mut seen = vec![false; flychecks.len()];
    let mut stack: Vec<_> = flycheck.deps.iter().rev().copied().collect();
    let mut paths = vec![flycheck.diagnostics];
    while let Some(index) = stack.pop() {
        // an index out of range is a corrupt database, whose flycheck replays what it can
        let Some(dep) = flychecks.get(index as usize) else {
            continue;
        };
        if std::mem::replace(&mut seen[index as usize], true) {
            continue;
        }
        if dep.member {
            paths.push(dep.diagnostics);
        }
        stack.extend(dep.deps.iter().rev());
    }
    paths
}

/// Run a flycheck for rust-analyzer, without evaluating the configuration.
///
/// The checks are brought up to date by ninja, then their saved diagnostics are copied to stdout, so that diagnostics
/// are shown even when nothing needed checking. Problems are reported on stderr, which rust-analyzer ignores, as is the
/// exit status.
pub fn check(label: &str) {
    let project = match Configured::find(Output::Stderr) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return;
        },
    };
    // ninja's output is not JSON, so it is kept out of stdout
    let status = project.ninja()
        .args(["--quiet", &format!("check/{label}")])
        .stdout(std::io::stderr())
        .status();
    if let Err(error) = status {
        eprintln!("failed to run ninja: {error}");
    }
    // read after ninja, which reconfigures if needed, so that it is current
    let bytes = match project.read() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{error}");
            return;
        },
    };
    // the entries borrow from the bytes
    let database = match project.parse(&bytes) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("{error}");
            return;
        },
    };
    let Some(entry) = database.flychecks.iter().find(|entry| entry.label == label) else {
        eprintln!("no crate with the label {label:?}");
        return;
    };
    let mut stdout = std::io::stdout().lock();
    for path in replayed(entry, &database.flychecks) {
        // a check which has not run, as a dependency failed, has no diagnostics
        if let Ok(mut file) = std::fs::File::open(project.root.join(path)) {
            let _ = std::io::copy(&mut file, &mut stdout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flycheck(diagnostics: &'static str, member: bool, deps: &[u32]) -> Flycheck<'static> {
        Flycheck { label: diagnostics, diagnostics, member, deps: deps.to_vec() }
    }

    #[test]
    fn replays_dependencies_once_in_order() {
        // app uses lib and util, which both use core, which is not edited, and base
        let flychecks = [
            flycheck("app", true, &[1, 2]),
            flycheck("lib", true, &[3]),
            flycheck("util", true, &[3, 4]),
            flycheck("core", false, &[4]),
            flycheck("base", true, &[]),
        ];
        assert_eq!(replayed(&flychecks[0], &flychecks), ["app", "lib", "base", "util"]);
        // its own diagnostics are replayed, even if it is not edited
        assert_eq!(replayed(&flychecks[3], &flychecks), ["core", "base"]);
        // a corrupt index is skipped
        let corrupt = [flycheck("app", true, &[7, 1]), flycheck("lib", true, &[])];
        assert_eq!(replayed(&corrupt[0], &corrupt), ["app", "lib"]);
    }
}
