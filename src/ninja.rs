//! ninja, which builds the graph from `build.ninja` in the build directory, which is written by [`NinjaFile`].

use std::path::Path;
use std::process::Command;

use super::Error;
use crate::builder::{Builder, Output, Request};
use crate::graph::{Edge, Graph, Rule};
use crate::project::has_line_break;
use crate::toolchain;

/// The file of the graph, in the build directory.
pub const FILE: &str = "build.ninja";

/// Writes `build.ninja`.
///
/// Paths are escaped when written, variable values are not. Neither can have a line break, which ninja cannot write, so
/// the file is not written with one, which would end the statement, and is an error.
pub struct NinjaFile {
    path: std::path::PathBuf,
    tmp_path: std::path::PathBuf,
    file: std::io::BufWriter<std::fs::File>,
}
impl NinjaFile {
    pub fn new<P: AsRef<std::path::Path>>(path: P) -> Result<Self, Error> {
        fn new(path: &std::path::Path) -> Result<NinjaFile, Error> {
            let tmp_path = path.with_added_extension("tmp");
            let file = match std::fs::File::create(&tmp_path) {
                Ok(f) => f,
                Err(error) => return Err(Error::Create(path.into(), error)),
            };
            Ok(NinjaFile {
                tmp_path,
                path: path.into(),
                file: std::io::BufWriter::new(file),
            })
        }
        new(path.as_ref())
    }
}
impl Graph for NinjaFile {
    /// Write a top-level variable.
    ///
    /// `value` is written verbatim, so it may reference other variables with `$`.
    fn variable(&mut self, name: &str, value: &str) -> Result<(), Error> {
        self.check(value)?;
        self.write(format!("{name} = {value}\n").as_bytes())
    }
    /// Write a `rule` statement.
    ///
    /// The command, description and depfile are written verbatim, so that other variables can be referenced with `$`.
    fn rule(&mut self, rule: &Rule) -> Result<(), Error> {
        let variables = rule.bindings();
        self.check_variables(&variables)?;
        let mut statement = format!("\nrule {}\n", rule.name).into_bytes();
        push_variables(&mut statement, &variables);
        self.write(&statement)
    }
    /// Write a `build` statement.
    fn edge(&mut self, edge: &Edge) -> Result<(), Error> {
        let paths = [edge.outputs, edge.implicit_outputs, edge.inputs, edge.implicit, edge.order_only];
        self.check_paths(paths.iter().copied().flatten())?;
        self.check_variables(edge.variables)?;
        let mut statement = b"\nbuild".to_vec();
        push_paths(&mut statement, "", edge.outputs);
        push_paths(&mut statement, " |", edge.implicit_outputs);
        statement.extend_from_slice(b": ");
        statement.extend_from_slice(edge.rule.as_bytes());
        push_paths(&mut statement, "", edge.inputs);
        push_paths(&mut statement, " |", edge.implicit);
        push_paths(&mut statement, " ||", edge.order_only);
        statement.push(b'\n');
        push_variables(&mut statement, edge.variables);
        self.write(&statement)
    }
    /// Write a `phony` edge.
    fn alias(&mut self, name: &Path, targets: &[&Path]) -> Result<(), Error> {
        self.edge(&Edge { rule: "phony", outputs: &[name], inputs: targets, ..Default::default() })
    }
    /// Write a `default` statement.
    fn default(&mut self, targets: &[&Path]) -> Result<(), Error> {
        self.check_paths(targets)?;
        let mut statement = b"\ndefault".to_vec();
        push_paths(&mut statement, "", targets);
        statement.push(b'\n');
        self.write(&statement)
    }
    /// Flush writes and rename the temporary file to the true file.
    fn finish(&mut self) -> Result<(), Error> {
        use std::io::Write;
        if let Err(error) = self.file.flush() {
            return Err(Error::Write(self.path.clone(), error));
        }
        if let Err(error) = self.file.get_mut().sync_all() {
            return Err(Error::Write(self.path.clone(), error));
        }
        if let Err(error) = std::fs::rename(&self.tmp_path, &self.path) {
            // last-ditch clean up attempt
            let _ = std::fs::remove_file(&self.tmp_path);
            return Err(Error::Write(self.path.clone(), error));
        }
        Ok(())
    }
}
impl NinjaFile {
    /// Fail with text which cannot be written, as it has a line break.
    fn check(&self, text: &str) -> Result<(), Error> {
        if has_line_break(text) {
            return Err(Error::LineBreak(self.path.clone(), text.into()));
        }
        Ok(())
    }
    fn check_paths<'a>(&self, paths: impl IntoIterator<Item = &'a &'a Path>) -> Result<(), Error> {
        for path in paths {
            self.check(&path.to_string_lossy())?;
        }
        Ok(())
    }
    fn check_variables(&self, variables: &[(&str, &str)]) -> Result<(), Error> {
        for (_, value) in variables {
            self.check(value)?;
        }
        Ok(())
    }
    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        use std::io::Write;
        if let Err(error) = self.file.write_all(data) {
            return Err(Error::Write(self.path.clone(), error));
        }
        Ok(())
    }
}

/// Replace `build.ninja` in a previous build directory with a stub which ninja finds is up to date, so that ninja
/// running there, which is regenerating it, stops, once `build-dir` has changed to `build_dir`.
pub fn retire(previous: &Path, build_dir: &Path) -> Result<(), Error> {
    let path = previous.join(FILE);
    if !path.is_file() {
        return Ok(());
    }
    let stub = format!(
        "# `build-dir` is now {:?}, where `jig generate` writes `build.ninja`\nbuild build.ninja: phony\n",
        build_dir.display().to_string(),
    );
    if let Err(error) = std::fs::write(&path, stub) {
        return Err(Error::Write(path, error));
    }
    Ok(())
}

/// ninja, which runs in the build directory, with the toolchain the project is configured with.
pub struct Ninja<'a> {
    pub build_dir: &'a Path,
    pub toolchain: Option<&'a str>,
}
impl Ninja<'_> {
    fn command(&self) -> Command {
        let mut ninja = Command::new("ninja");
        ninja.current_dir(self.build_dir);
        toolchain::apply(&mut ninja, self.toolchain);
        ninja
    }
}
impl Builder for Ninja<'_> {
    fn build(&self, request: &Request) -> Result<bool, Error> {
        let mut ninja = self.command();
        if request.quiet {
            ninja.arg("--quiet");
        }
        if request.keep_going {
            ninja.args(["-k", "0"]);
        }
        ninja.args(request.args).args(request.targets);
        if request.output == Output::Stderr {
            ninja.stdout(std::io::stderr());
        }
        match ninja.status() {
            Ok(status) => Ok(status.success()),
            Err(error) => Err(Error::Exec(Box::new(ninja), error)),
        }
    }
    /// Passes ninja's output on as it is written, except that it had no work to do, as a build follows.
    fn regenerate(&self, quiet: bool, output: Output) -> Result<(), Error> {
        use std::io::{BufRead, Write};
        let mut ninja = self.command();
        if quiet {
            ninja.arg("--quiet");
        }
        ninja.arg(FILE).stdout(std::process::Stdio::piped());
        let mut child = match ninja.spawn() {
            Ok(child) => child,
            Err(error) => return Err(Error::Exec(Box::new(ninja), error)),
        };
        let mut w: Box<dyn Write> = match output {
            Output::Stdout => Box::new(std::io::stdout()),
            Output::Stderr => Box::new(std::io::stderr()),
        };
        for line in std::io::BufReader::new(child.stdout.take().unwrap()).split(b'\n') {
            let Ok(line) = line else {
                break;
            };
            if line != NO_WORK.as_bytes() {
                let _ = w.write_all(&line).and_then(|()| w.write_all(b"\n"));
            }
        }
        match child.wait() {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err(Error::BuildFailed),
            Err(error) => Err(Error::Exec(Box::new(ninja), error)),
        }
    }
    /// Asks ninja for a dry run, which does not regenerate `build.ninja`.
    fn up_to_date(&self, target: &str) -> Result<bool, Error> {
        let mut dry_run = self.command();
        dry_run.args(["-n", target]);
        let output = match dry_run.output() {
            Ok(output) => output,
            Err(error) => return Err(Error::Exec(Box::new(dry_run), error)),
        };
        Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).contains(NO_WORK))
    }
    fn graph_up_to_date(&self) -> Result<bool, Error> {
        self.up_to_date(FILE)
    }
}

/// What ninja prints when nothing is out of date.
const NO_WORK: &str = "ninja: no work to do.";

/// Append `prefix` followed by each path, if there are any paths.
fn push_paths(statement: &mut Vec<u8>, prefix: &str, paths: &[&Path]) {
    if paths.is_empty() {
        return;
    }
    statement.extend_from_slice(prefix.as_bytes());
    for path in paths {
        statement.push(b' ');
        push_path(statement, path);
    }
}

/// Append a path, escaping the characters which ninja treats specially in a path list.
fn push_path(statement: &mut Vec<u8>, path: &Path) {
    for &b in crate::graph::path_string(path).as_bytes() {
        if matches!(b, b'$' | b' ' | b':') {
            statement.push(b'$');
        }
        statement.push(b);
    }
}

/// Append the indented variables of a `rule` or `build` statement.
fn push_variables(statement: &mut Vec<u8>, variables: &[(&str, &str)]) {
    for (name, value) in variables {
        statement.extend_from_slice(format!("    {name} = {value}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_line_breaks() {
        let dir = std::env::temp_dir().join(format!("jig-ninja-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("build.ninja");
        let mut ninja = NinjaFile::new(&path).unwrap_or_else(|error| panic!("{error}"));
        // a line break would end the statement, so nothing is written with one
        let broken = Path::new("a\nb");
        assert!(matches!(ninja.alias(broken, &[]), Err(Error::LineBreak(_, text)) if text == "a\nb"));
        let edge = Edge { rule: "phony", outputs: &[Path::new("a")], variables: &[("v", "x\ry")], ..Default::default() };
        assert!(matches!(ninja.edge(&edge), Err(Error::LineBreak(_, text)) if text == "x\ry"));
        let rule = Rule { name: "r", command: "a\nb", ..Default::default() };
        assert!(matches!(ninja.rule(&rule), Err(Error::LineBreak(..))));
        assert!(matches!(ninja.variable("v", "a\nb"), Err(Error::LineBreak(..))));
        assert!(matches!(ninja.default(&[broken]), Err(Error::LineBreak(..))));
        // while the rest is written as it was
        ninja.alias(Path::new("a b"), &[]).unwrap_or_else(|error| panic!("{error}"));
        ninja.finish().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\nbuild a$ b: phony\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
