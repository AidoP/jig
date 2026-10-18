//! A configured project, found from the working directory by its database, for the commands which build it with ninja
//! without evaluating the configuration.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::project::{self, Diagnostic, Message, Severity};
use crate::{Error, database, generate, toolchain};

/// Where ninja's output is written while finding a project, as `jig check` keeps it out of stdout.
#[derive(Clone, Copy)]
pub enum Output {
    Stdout,
    Stderr,
    /// Stdout, where ninja is quiet, so that only the output of what it runs is shown.
    Quiet,
}

pub struct Configured {
    /// The project root, where the database is, which is where commands and tests run from.
    pub root: PathBuf,
    /// The build directory, where `build.ninja` is, which is where ninja runs.
    pub build_dir: PathBuf,
    /// The rustup toolchain the project is configured with, which ninja and the tests are run with.
    pub toolchain: Option<String>,
}
impl Configured {
    /// Find the project which contains the working directory, which must have been configured.
    ///
    /// The configuration is brought up to date, as the build directory may change, where ninja's output is written to
    /// `output`.
    pub fn find(output: Output) -> Result<Self, Error> {
        let mut project = Self::locate()?;
        project.check_jig()?;
        project.reconfigure(output)?;
        Ok(project)
    }
    /// Find the project which contains the working directory, as [`Self::find`] does, without bringing the
    /// configuration up to date, so that the build directory may be changed by [`Self::reconfigure`].
    ///
    /// If a path which the configuration found was missing has been created since, ninja is made to regenerate
    /// `build.ninja` by touching the reconfigure stamp, which it depends on.
    pub fn locate() -> Result<Self, Error> {
        let root = ensure(None)?;
        let bytes = read(&root)?;
        let database = parse(&root, &bytes)?;
        let build_dir = root.join(database.build_dir);
        if database.missing.iter().any(|path| root.join(path).exists()) {
            let stamp = build_dir.join(generate::RECONFIGURE_STAMP);
            let touched = std::fs::File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&stamp)
                .and_then(|file| file.set_modified(std::time::SystemTime::now()));
            if let Err(error) = touched {
                return Err(Error::Write(stamp, error));
            }
        }
        let toolchain = toolchain(&database);
        Ok(Self { root, build_dir, toolchain })
    }
    /// Report `other-jig` if the running `jig` is not the one which configured the project, which regenerates
    /// `build.ninja`.
    ///
    /// The wrappers installed by `jig install --proxy` are not reported, as they run the `jig` they were installed by.
    fn check_jig(&self) -> Result<(), Error> {
        let bytes = read(&self.root)?;
        let database = parse(&self.root, &bytes)?;
        let Ok(running) = jig() else {
            return Ok(());
        };
        if database.jig.is_empty() || same_file(Path::new(database.jig), &running) {
            return Ok(());
        }
        let severity = project::severity_override(database.other_jig).unwrap_or(Severity::Warning);
        let message = Message::OtherJig { running: running.display().to_string(), configured: database.jig.into() };
        report(&self.root, message, severity)
    }
    /// Regenerate `build.ninja` with ninja if it is out of date, then take the build directory from the database again,
    /// as `build-dir` may have changed, which leaves a stub in the previous build directory.
    ///
    /// Ninja's output is passed on as it is written, except that it had no work to do, as the build follows.
    pub fn reconfigure(&mut self, output: Output) -> Result<(), Error> {
        use std::io::{BufRead, Write};
        let mut ninja = self.ninja();
        if let Output::Quiet = output {
            ninja.arg("--quiet");
        }
        ninja.arg("build.ninja").stdout(std::process::Stdio::piped());
        let mut child = match ninja.spawn() {
            Ok(child) => child,
            Err(error) => return Err(Error::Exec(ninja, error)),
        };
        let mut w: Box<dyn Write> = match output {
            Output::Stdout | Output::Quiet => Box::new(std::io::stdout()),
            Output::Stderr => Box::new(std::io::stderr()),
        };
        for line in std::io::BufReader::new(child.stdout.take().unwrap()).split(b'\n') {
            let Ok(line) = line else {
                break;
            };
            if line != b"ninja: no work to do." {
                let _ = w.write_all(&line).and_then(|()| w.write_all(b"\n"));
            }
        }
        match child.wait() {
            Ok(status) if status.success() => (),
            Ok(_) => return Err(Error::BuildFailed),
            Err(error) => return Err(Error::Exec(ninja, error)),
        }
        let bytes = read(&self.root)?;
        let database = parse(&self.root, &bytes)?;
        self.build_dir = self.root.join(database.build_dir);
        self.toolchain = toolchain(&database);
        Ok(())
    }
    /// Returns ninja, to run in the build directory.
    pub fn ninja(&self) -> Command {
        let mut ninja = Command::new("ninja");
        ninja.current_dir(&self.build_dir);
        toolchain::apply(&mut ninja, self.toolchain.as_deref());
        ninja
    }
    /// Returns the bytes of the database, which is read again once ninja has run, as it reconfigures if needed.
    pub fn read(&self) -> Result<Vec<u8>, Error> {
        read(&self.root)
    }
    /// Parse the database, which borrows from its bytes.
    pub fn parse<'a>(&self, bytes: &'a [u8]) -> Result<database::Database<'a>, Error> {
        parse(&self.root, bytes)
    }
}

fn read(root: &Path) -> Result<Vec<u8>, Error> {
    let path = root.join(database::PATH);
    std::fs::read(&path).map_err(|error| Error::Read(path, error))
}

fn parse<'a>(root: &Path, bytes: &'a [u8]) -> Result<database::Database<'a>, Error> {
    database::read(bytes).map_err(|error| Error::Read(root.join(database::PATH), error))
}

/// Returns the toolchain a project is configured with, from its database, if it is configured with one.
fn toolchain(database: &database::Database) -> Option<String> {
    (!database.toolchain.is_empty()).then(|| database.toolchain.into())
}

/// Configure the project which contains the working directory if it has not been, or cannot be built as it was, and
/// with a toolchain if one is given which it is not configured with, returning its root.
///
/// The project root is the closest directory to the working directory, or it itself, with a database, or without one,
/// with a `Jig` file. It is configured again from the root file it was configured with, and with its toolchain unless
/// another is given, when its database is of another version, or `build.ninja` is missing, such as when the build
/// directory has been removed.
pub fn ensure(toolchain: Option<&str>) -> Result<PathBuf, Error> {
    let root = match database::root() {
        Some(root) => root,
        None => {
            let dir = std::env::current_dir().map_err(|error| Error::Read(".".into(), error))?;
            match dir.ancestors().find(|dir| dir.join("Jig").is_file()) {
                Some(root) => root.to_path_buf(),
                None => return Err(Error::NotConfigured),
            }
        },
    };
    let bytes = std::fs::read(root.join(database::PATH)).ok();
    let database = bytes.as_deref().and_then(|bytes| database::read(bytes).ok());
    let (input, configured, current) = match &database {
        Some(database) => {
            let built = root.join(database.build_dir).join("build.ninja").is_file();
            let configured = (!database.toolchain.is_empty()).then(|| database.toolchain.to_string());
            let current = built && toolchain.is_none_or(|toolchain| toolchain == database.toolchain);
            (database.input.to_string(), configured, current)
        },
        None => ("Jig".to_string(), None, false),
    };
    if current {
        return Ok(root);
    }
    // the configuration is generated from the project root, as by the rule which regenerates `build.ninja`
    if let Err(error) = std::env::set_current_dir(&root) {
        return Err(Error::Read(root, error));
    }
    crate::generate(PathBuf::from(input), toolchain.or(configured.as_deref()))?;
    Ok(root)
}

/// Returns the path of the running `jig`, which is the path it was run by if that is absolute, as a wrapper installed
/// by `jig install --proxy` runs it, rather than the path of the running executable, which `jig build --proxy` may
/// have replaced by building it.
pub fn jig() -> std::io::Result<PathBuf> {
    match std::env::args_os().next() {
        Some(path) if Path::new(&path).is_absolute() => Ok(path.into()),
        _ => std::env::current_exe(),
    }
}

/// Returns whether two paths are of the same file, following symbolic links, or are the same if one is missing.
pub fn same_file(lhs: &Path, rhs: &Path) -> bool {
    match (std::fs::canonicalize(lhs), std::fs::canonicalize(rhs)) {
        (Ok(lhs), Ok(rhs)) => lhs == rhs,
        _ => lhs == rhs,
    }
}

/// Report a diagnostic found once the project is configured, such as while building it, unless it is allowed, with the
/// severity recorded when configuring, failing if it is an error.
pub fn report(root: &Path, message: Message, severity: Severity) -> Result<(), Error> {
    use std::io::IsTerminal;
    if severity == Severity::Allow {
        return Ok(());
    }
    let id = message.id().to_string();
    let diagnostic = Diagnostic { span: None, message, severity };
    // a diagnostic without a span needs nothing from the configuration
    let empty = project::Project::new(root.to_path_buf(), Vec::new());
    let mut stderr = std::io::stderr().lock();
    let pretty = stderr.is_terminal();
    if let Err(error) = empty.write_diagnostic(&mut stderr, &diagnostic, pretty) {
        return Err(Error::Stderr(error));
    }
    match severity {
        Severity::Error | Severity::Fatal => Err(Error::Reported(id)),
        _ => Ok(()),
    }
}
