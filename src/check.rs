//! `jig check`, the flycheck of rust-analyzer, which replays saved diagnostics.

use std::path::PathBuf;

use crate::configured::{Configured, Output};
use crate::generate::Unit;

/// Returns the diagnostics to replay for a flycheck of a unit: its own, then those of the units it depends on, directly
/// or not, which are edited.
pub fn replayed(unit: &Unit, units: &[Unit]) -> Vec<PathBuf> {
    let mut seen = vec![false; units.len()];
    let mut stack: Vec<_> = unit.deps.iter().rev().copied().collect();
    let mut paths = vec![unit.diagnostics()];
    while let Some(index) = stack.pop() {
        if seen[index] {
            continue;
        }
        seen[index] = true;
        if units[index].member {
            paths.push(units[index].diagnostics());
        }
        stack.extend(units[index].deps.iter().rev());
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
    for path in &entry.diagnostics {
        // a check which has not run, as a dependency failed, has no diagnostics
        if let Ok(mut file) = std::fs::File::open(project.root.join(path)) {
            let _ = std::io::copy(&mut file, &mut stdout);
        }
    }
}
