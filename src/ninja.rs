use std::path::Path;

use super::Error;
use crate::project::has_line_break;

/// A `build` statement.
///
/// Paths are escaped when written, variable values are not. Neither can have a line break, which ninja cannot write, so
/// the file is not written with one, which would end the statement, and is an error.
#[derive(Default)]
pub struct Build<'a> {
    pub rule: &'a str,
    pub outputs: &'a [&'a Path],
    pub implicit_outputs: &'a [&'a Path],
    pub inputs: &'a [&'a Path],
    /// Inputs which trigger a rebuild but are not part of `$in`.
    pub implicit: &'a [&'a Path],
    /// Inputs which must be built first but do not trigger a rebuild.
    pub order_only: &'a [&'a Path],
    pub variables: &'a [(&'a str, &'a str)],
}

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
    /// Write a top-level variable.
    ///
    /// `value` is written verbatim, so it may reference other variables with `$`.
    pub fn variable(&mut self, name: &str, value: &str) -> Result<(), Error> {
        self.check(value)?;
        self.write(format!("{name} = {value}\n").as_bytes())
    }
    /// Write a `rule` statement.
    ///
    /// Variable values are written verbatim, so that other variables can be referenced with `$`.
    pub fn rule(&mut self, name: &str, variables: &[(&str, &str)]) -> Result<(), Error> {
        self.check_variables(variables)?;
        let mut statement = format!("\nrule {name}\n").into_bytes();
        push_variables(&mut statement, variables);
        self.write(&statement)
    }
    /// Write a `build` statement.
    pub fn build(&mut self, build: &Build) -> Result<(), Error> {
        let paths = [build.outputs, build.implicit_outputs, build.inputs, build.implicit, build.order_only];
        self.check_paths(paths.iter().copied().flatten())?;
        self.check_variables(build.variables)?;
        let mut statement = b"\nbuild".to_vec();
        push_paths(&mut statement, "", build.outputs);
        push_paths(&mut statement, " |", build.implicit_outputs);
        statement.extend_from_slice(b": ");
        statement.extend_from_slice(build.rule.as_bytes());
        push_paths(&mut statement, "", build.inputs);
        push_paths(&mut statement, " |", build.implicit);
        push_paths(&mut statement, " ||", build.order_only);
        statement.push(b'\n');
        push_variables(&mut statement, build.variables);
        self.write(&statement)
    }
    /// Write a `default` statement, adding to the targets built when none are requested.
    pub fn default(&mut self, targets: &[&Path]) -> Result<(), Error> {
        self.check_paths(targets)?;
        let mut statement = b"\ndefault".to_vec();
        push_paths(&mut statement, "", targets);
        statement.push(b'\n');
        self.write(&statement)
    }
    /// Finalize writing the ninja file.
    ///
    /// Flushes writes and renames the temporary file to the true file.
    pub fn sync_all(&mut self) -> Result<(), Error> {
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
    for &b in path.as_os_str().as_encoded_bytes() {
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
        let build = Build { rule: "phony", outputs: &[broken], ..Default::default() };
        assert!(matches!(ninja.build(&build), Err(Error::LineBreak(_, text)) if text == "a\nb"));
        let build = Build { rule: "phony", outputs: &[Path::new("a")], variables: &[("v", "x\ry")], ..Default::default() };
        assert!(matches!(ninja.build(&build), Err(Error::LineBreak(_, text)) if text == "x\ry"));
        assert!(matches!(ninja.rule("r", &[("command", "a\nb")]), Err(Error::LineBreak(..))));
        assert!(matches!(ninja.variable("v", "a\nb"), Err(Error::LineBreak(..))));
        assert!(matches!(ninja.default(&[broken]), Err(Error::LineBreak(..))));
        // while the rest is written as it was
        ninja.build(&Build { rule: "phony", outputs: &[Path::new("a b")], ..Default::default() }).unwrap_or_else(|error| panic!("{error}"));
        ninja.sync_all().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\nbuild a$ b: phony\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
