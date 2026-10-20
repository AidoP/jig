//! `jig run`, which builds a binary crate and runs it, by its target's runner if it has one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::Error;
use crate::configured::{Configured, Output};
use crate::database::Program;

/// Build a binary crate and run it with arguments, without evaluating the configuration, as `name`, or the only binary
/// crate if no name is given.
///
/// What is built is reported on stderr, so that stdout is the program's. The program runs in the working directory, as
/// it would if it were run directly, by the runner of its target if it has one, which it is given to before the
/// arguments. Its exit status is returned.
pub fn run(name: Option<&str>, args: &[std::ffi::OsString]) -> Result<ExitCode, Error> {
    let project = Configured::find(Output::Stderr)?;
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let program = find(&database.programs, name)?;
    let mut ninja = project.ninja();
    ninja.arg(program.target).stdout(std::io::stderr());
    match ninja.status() {
        Ok(status) if status.success() => (),
        Ok(_) => return Err(Error::BuildFailed),
        Err(error) => return Err(Error::Exec(Box::new(ninja), error)),
    }
    let binary = project.root.join(program.path);
    let mut command = match program.runner.split_first() {
        // a runner which is a path, such as one which is built, is relative to the project root
        Some((runner, first)) => {
            let mut command = std::process::Command::new(from_root(&project.root, runner));
            command.args(first).arg(&binary);
            command
        },
        None => std::process::Command::new(&binary),
    };
    command.args(args);
    // with the toolchain the project is configured with, as its tests are
    crate::toolchain::apply(&mut command, project.toolchain.as_deref());
    match command.status() {
        Ok(status) => Ok(exit_code(status)),
        Err(error) => Err(Error::Exec(Box::new(command), error)),
    }
}

/// Returns the program with a name, which is that of its crate, as `<name>@<target>` if its target is not the host
/// target, or the crate's name alone if the programs with it can be chosen from, as all of them can if no name is
/// given, as by [`choose`].
fn find<'a, 'b>(programs: &'b [Program<'a>], name: Option<&str>) -> Result<&'b Program<'a>, Error> {
    let Some(name) = name else {
        if programs.is_empty() {
            return Err(Error::NoPrograms);
        }
        return choose(programs.iter().collect());
    };
    if let Some(program) = programs.iter().find(|program| program.name == name) {
        return Ok(program);
    }
    let of_crate = |program: &&Program| program.name.split_once('@').is_some_and(|(crate_name, _)| crate_name == name);
    let matching: Vec<_> = programs.iter().filter(of_crate).collect();
    if matching.is_empty() {
        let names = programs.iter().map(|program| program.name.to_string()).collect();
        return Err(Error::UnknownProgram(name.into(), names));
    }
    choose(matching)
}

/// Returns the program of those which could be run, which is the only one, or else the only one which is built by
/// default, such as the crate for the target which is released, or reports those to choose from.
fn choose<'a, 'b>(candidates: Vec<&'b Program<'a>>) -> Result<&'b Program<'a>, Error> {
    if let [program] = candidates[..] {
        return Ok(program);
    }
    let defaults: Vec<_> = candidates.iter().copied().filter(|program| program.default).collect();
    if let [program] = defaults[..] {
        return Ok(program);
    }
    // of the defaults, if there are several, as they are the ones meant to be chosen from
    let names = if defaults.is_empty() { candidates } else { defaults };
    Err(Error::ProgramNotChosen(names.iter().map(|program| program.name.to_string()).collect()))
}

/// Returns a program as the project root has it, which is a path relative to it if it has a `/`, or else is found in
/// `PATH`.
fn from_root(root: &Path, program: &str) -> PathBuf {
    if program.contains('/') { root.join(program) } else { program.into() }
}

/// Returns the exit status of a program as jig's own, where one killed by a signal exits as a shell reports it, with
/// 128 and the signal.
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    let code = status.code().or_else(|| status.signal().map(|signal| 128 + signal)).unwrap_or(1);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(name: &'static str) -> Program<'static> {
        Program { name, path: "", target: "", runner: vec![], default: false }
    }

    #[test]
    fn finds_programs() {
        let programs = [program("app"), program("tool@riscv64"), program("kernel@a"), program("kernel@b")];
        let found = |name| find(&programs, name).map(|program| program.name).map_err(|error| error.to_string());
        assert_eq!(found(Some("app")), Ok("app"));
        assert_eq!(found(Some("tool@riscv64")), Ok("tool@riscv64"));
        // the name of a crate is enough where only one target has it
        assert_eq!(found(Some("tool")), Ok("tool@riscv64"));
        assert!(found(Some("kernel")).unwrap_err().contains("kernel@a, kernel@b"));
        assert!(found(Some("nope")).unwrap_err().contains("no program named \"nope\""));
        assert!(found(None).unwrap_err().contains("app, tool@riscv64, kernel@a, kernel@b"));
        assert_eq!(find(&programs[..1], None).map(|program| program.name).ok(), Some("app"));
        assert!(find(&[], None).is_err());
    }

    #[test]
    fn prefers_defaults() {
        let default = |name| Program { default: true, ..program(name) };
        let programs = [program("app"), default("tool"), program("kernel@a"), default("kernel@b"), default("kernel@c")];
        let found = |name| find(&programs, name).map(|program| program.name).map_err(|error| error.to_string());
        // the program built by default is run, of those with a name, or of all of them
        assert_eq!(found(Some("kernel@a")), Ok("kernel@a"));
        assert!(found(Some("kernel")).unwrap_err().contains("of kernel@b, kernel@c"));
        assert_eq!(find(&programs[..4], None).map(|program| program.name).ok(), None);
        assert_eq!(find(&programs[..3], None).map(|program| program.name).ok(), Some("tool"));
        assert_eq!(find(&programs[2..4], Some("kernel")).map(|program| program.name).ok(), Some("kernel@b"));
        // while several are chosen from
        assert!(found(None).unwrap_err().contains("of tool, kernel@b, kernel@c"));
    }
}
