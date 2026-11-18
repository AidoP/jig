//! `jig bench`, which builds the harnesses of the crates' benchmarks and runs them.

use crate::Error;
use crate::configured::{Configured, Output, Request};

/// Build the harnesses of the crates with `bench`, or only those named, as `profile/release` builds, then run each with
/// `--bench`, which runs its benchmarks, and the arguments given, in turn, from the project root, showing what it
/// prints as it prints it, as a benchmark is timed rather than passed or failed. It fails if one of them does.
pub fn bench(names: &[String], args: &[std::ffi::OsString], build: crate::rig_builder::Options) -> Result<(), Error> {
    let project = Configured::find(Output::Stderr)?;
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let mut benches = Vec::new();
    for name in names {
        match database.benches.iter().find(|bench| bench.name == name) {
            Some(bench) => benches.push(bench),
            None => return Err(Error::UnknownBench(name.clone())),
        }
    }
    if names.is_empty() {
        benches = database.benches.iter().collect();
    }
    if benches.is_empty() {
        return Err(Error::NoBenches);
    }
    let targets: Vec<&str> = benches.iter().map(|bench| bench.target).collect();
    project.build(&Request { targets: &targets, output: Output::Stderr, build, ..Default::default() })?;
    // a build which a benchmark starts may need the state, which is closed first
    crate::rig_builder::release();
    let mut failed = 0;
    for bench in benches {
        let mut command = std::process::Command::new(project.root.join(bench.path));
        command.current_dir(&project.root).arg("--bench").args(args);
        crate::platform::add_library_path(&mut command, &project.root, &database.library_path);
        crate::toolchain::apply(&mut command, project.toolchain.as_deref());
        match crate::timings::step("bench", || command.status()) {
            Ok(status) => failed += usize::from(!status.success()),
            Err(error) => return Err(Error::Exec(Box::new(command), error)),
        }
    }
    if failed == 0 { Ok(()) } else { Err(Error::BenchesFailed(failed)) }
}
