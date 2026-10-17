//! `jig install`, which installs the files recorded in the database, and `jig build --proxy`, which the wrappers it
//! installs with `--proxy` build with.

use std::io::IsTerminal;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::configured::{Configured, Output};
use crate::generate::shell_quote;
use crate::project::{self, Diagnostic, Message, Severity};
use crate::{Error, database};

/// Install the files of the crates and custom commands with `install` in a prefix, or the `install-prefix` property if
/// none is given, without evaluating the configuration.
///
/// What is installed is built by ninja first, then each file is copied into its directory within the prefix, replacing
/// any file there, with its permissions, followed by a summary. With `proxy`, a program is installed as a wrapper which
/// builds it if needed then runs it, and any other file as a link to it, so that what is installed follows the build.
pub fn install(prefix: Option<PathBuf>, proxy: bool) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    build(&project, &["install"])?;
    // read after ninja, which reconfigures if needed, so that it is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    // a prefix which is given is relative to the working directory, and the property to the project root
    let given = prefix.unwrap_or_else(|| project.root.join(database.install_prefix));
    // a wrapper finds its program by its prefix, so it is absolute
    let prefix = match std::path::absolute(&given) {
        Ok(prefix) => prefix,
        Err(error) => return Err(Error::Create(given, error)),
    };
    let (mut copies, mut wrappers, mut links) = (0, 0, 0);
    for file in &database.installs {
        let source = project.root.join(file.path);
        let dir = prefix.join(file.dir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            return Err(Error::Create(dir, error));
        }
        let destination = destination(&prefix, file);
        let installed = match (proxy, file.program) {
            (false, _) => copy(&source, &destination, file.mode).map(|()| copies += 1),
            (true, true) => {
                write_proxy(&project.root, &prefix, &destination, &source, file.mode).map(|_| wrappers += 1)
            },
            (true, false) => link(&source, &destination).map(|()| links += 1),
        };
        if let Err(error) = installed {
            return Err(Error::Install(source, destination, error));
        }
        println!("INSTALL {}", destination.display());
    }
    println!("{}", summary(copies, wrappers, links, &given));
    Ok(())
}

/// Returns the message which summarizes what was installed in a prefix, as files, wrappers and links.
fn summary(copies: usize, wrappers: usize, links: usize, prefix: &Path) -> String {
    let mut counts = Vec::new();
    for (count, noun) in [(copies, "file"), (wrappers, "wrapper"), (links, "link")] {
        if count > 0 {
            counts.push(format!("{count} {noun}{}", if count == 1 { "" } else { "s" }));
        }
    }
    let installed = match counts.as_slice() {
        [] => return "nothing to install".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!("installed {installed} in {}", prefix.display())
}

/// Remove the file at the destination, if there is one, so that it is replaced rather than written to, as it may be
/// running.
fn remove(destination: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(destination) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Copy a file with permissions `mode`.
fn copy(source: &Path, destination: &Path, mode: u32) -> std::io::Result<()> {
    remove(destination)?;
    std::fs::copy(source, destination)?;
    std::fs::set_permissions(destination, std::fs::Permissions::from_mode(mode))
}

/// Link to a file, which is absolute, so that the link is to what was last built.
fn link(source: &Path, destination: &Path) -> std::io::Result<()> {
    remove(destination)?;
    std::os::unix::fs::symlink(std::path::absolute(source)?, destination)
}

/// The exit status of `jig build --proxy` when it has reinstalled the wrapper which ran it, which then runs the new
/// wrapper instead, as what it runs may have changed.
pub const REINSTALLED: u8 = 75;

/// Returns the wrapper for a program, which builds it with `jig build --proxy` from the project root, then runs it with
/// the wrapper's arguments, or runs the wrapper again if `jig` reinstalls it.
///
/// The paths are absolute, and the wrapper is identified by its prefix and its own path, which `jig` finds its program
/// by. The build's output is written to stderr, so that the program's output is its own.
fn proxy_script(root: &Path, prefix: &Path, wrapper: &Path, source: &Path) -> std::io::Result<String> {
    let quote = |path: &Path| shell_quote(&path.display().to_string());
    Ok(format!(
        "#!/bin/sh\n\
         # installed by `jig install --proxy`, which builds the program if needed, then runs it,\n\
         # or this wrapper again if it is reinstalled\n\
         (cd {root} && exec {jig} build --proxy {prefix} {wrapper}) >&2\n\
         case $? in\n\
         \x20   0) exec {source} \"$@\" ;;\n\
         \x20   {REINSTALLED}) exec {wrapper} \"$@\" ;;\n\
         \x20   *) exit 1 ;;\n\
         esac\n",
        root = quote(root),
        jig = quote(&jig()?),
        prefix = quote(prefix),
        wrapper = quote(wrapper),
        source = quote(source),
    ))
}

/// Returns the path of `jig` for a wrapper to run, which is the path it was run by if that is absolute, as a wrapper runs
/// it, rather than the path of the running executable, which `jig build --proxy` may have replaced by building it.
fn jig() -> std::io::Result<PathBuf> {
    match std::env::args_os().next() {
        Some(path) if Path::new(&path).is_absolute() => Ok(path.into()),
        _ => std::env::current_exe(),
    }
}

/// Write a wrapper for a program, with permissions `mode`, unless it is already as it would be written.
///
/// Returns whether it was written.
fn write_proxy(root: &Path, prefix: &Path, wrapper: &Path, source: &Path, mode: u32) -> std::io::Result<bool> {
    let script = proxy_script(&std::path::absolute(root)?, prefix, wrapper, &std::path::absolute(source)?)?;
    let current = std::fs::read_to_string(wrapper).ok();
    let current_mode = std::fs::symlink_metadata(wrapper).ok().map(|metadata| metadata.permissions().mode() & 0o7777);
    if current.as_ref() == Some(&script) && current_mode == Some(mode) {
        return Ok(false);
    }
    remove(wrapper)?;
    std::fs::write(wrapper, script)?;
    std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(mode))?;
    Ok(true)
}

/// Returns where a file is installed in a prefix.
fn destination(prefix: &Path, file: &database::Install) -> PathBuf {
    prefix.join(file.dir).join(Path::new(file.path).file_name().unwrap_or_default())
}

/// Build the program of a wrapper installed by `jig install --proxy` in a prefix, which is found by the wrapper's path
/// once the configuration is current, then reinstall the wrapper if it would be written differently, such as when its
/// program has moved or `jig` has changed.
///
/// The `proxy-build` diagnostic is reported first if the configuration or the program is out of date, before the
/// configuration is regenerated, so that it precedes any diagnostics from configuring. Its severity is that recorded
/// when last configured, and as an error, nothing is configured or built. Ninja is quiet, so that only the diagnostic
/// and the output of what it runs are shown.
///
/// Returns whether the wrapper was reinstalled, as it should then be run again.
pub fn build_proxy(prefix: &Path, wrapper: &Path) -> Result<bool, Error> {
    let mut project = Configured::locate()?;
    {
        let bytes = project.read()?;
        let database = project.parse(&bytes)?;
        let severity = project::severity_override(database.proxy_build).unwrap_or(Severity::Warning);
        // the program as last configured, which is named by the wrapper's path if it is not installed there
        let file = find_program(&database, prefix, wrapper);
        let program = match file {
            Some(file) => format!("{:?}", file.target),
            None => format!("{:?}", wrapper.display().to_string()),
        };
        let message = if !up_to_date(&project, "build.ninja")? {
            Some(Message::ProxyBuild { program, configuration: true })
        } else if let Some(file) = file && !up_to_date(&project, file.target)? {
            Some(Message::ProxyBuild { program, configuration: false })
        } else {
            None
        };
        if let Some(message) = message && severity != Severity::Allow {
            let diagnostic = Diagnostic { span: None, message, severity };
            // a diagnostic without a span needs nothing from the configuration
            let empty = project::Project::new(project.root.clone(), Vec::new());
            let mut stderr = std::io::stderr().lock();
            let pretty = stderr.is_terminal();
            if let Err(error) = empty.write_diagnostic(&mut stderr, &diagnostic, pretty) {
                return Err(Error::Stderr(error));
            }
            if severity == Severity::Error {
                return Err(Error::ProxyOutOfDate);
            }
        }
    }
    // what is installed at the wrapper may change with the configuration
    project.reconfigure(Output::Quiet)?;
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let Some(file) = find_program(&database, prefix, wrapper) else {
        return Err(Error::ProxyNotInstalled(wrapper.into()));
    };
    build(&project, &["--quiet", file.target])?;
    let source = project.root.join(file.path);
    match write_proxy(&project.root, prefix, wrapper, &source, file.mode) {
        Ok(reinstalled) => Ok(reinstalled),
        Err(error) => Err(Error::Install(source, wrapper.into(), error)),
    }
}

/// Returns the program which is installed at a wrapper in a prefix.
fn find_program<'a>(
    database: &'a database::Database,
    prefix: &Path,
    wrapper: &Path,
) -> Option<&'a database::Install<'a>> {
    database.installs.iter().find(|file| file.program && destination(prefix, file) == wrapper)
}

/// Returns whether ninja has no work to do for a target, from a dry run, which does not regenerate `build.ninja`.
fn up_to_date(project: &Configured, target: &str) -> Result<bool, Error> {
    let mut dry_run = project.ninja();
    dry_run.args(["-n", target]);
    let output = match dry_run.output() {
        Ok(output) => output,
        Err(error) => return Err(Error::Exec(dry_run, error)),
    };
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("ninja: no work to do."))
}

/// Run ninja in the build directory with arguments.
fn build(project: &Configured, args: &[&str]) -> Result<(), Error> {
    let mut ninja = project.ninja();
    ninja.args(args);
    match ninja.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(Error::BuildFailed),
        Err(error) => Err(Error::Exec(ninja, error)),
    }
}
