//! `jig test`, which runs the tests recorded in the database.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::Error;
use crate::configured::{Configured, Output};

/// Run tests, or all of them if no names are given, without evaluating the configuration.
///
/// What the tests need is built by ninja first, then each is run in turn from the project root with its output captured,
/// which is shown if it fails, with how long each took.
pub fn test(names: &[String]) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    let targets: Vec<_> = if names.is_empty() {
        vec!["test".to_string()]
    } else {
        names.iter().map(|name| format!("test/{name}")).collect()
    };
    let mut ninja = project.ninja();
    ninja.args(&targets);
    let built = match ninja.status() {
        Ok(status) => status.success(),
        Err(error) => return Err(Error::Exec(ninja, error)),
    };
    // read after ninja, which reconfigures if needed, so that it is current
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    // an unknown test also fails the build, as it has no target, but is the more useful error
    let mut tests = Vec::new();
    for name in names {
        match database.tests.iter().find(|test| test.name == name) {
            Some(test) => tests.push(test),
            None => return Err(Error::UnknownTest(name.clone())),
        }
    }
    if !built {
        return Err(Error::TestBuild);
    }
    if names.is_empty() {
        tests = database.tests.iter().collect();
    }

    let style = Style::new(std::io::stdout().is_terminal());
    let plural = if tests.len() == 1 { "" } else { "s" };
    // after what ninja printed
    println!("\nrunning {} test{plural}", tests.len());
    // the results line up after the longest name
    let width = tests.iter().map(|test| test.name.chars().count()).max().unwrap_or(0);
    let started = Instant::now();
    let mut failures = Vec::new();
    for test in &tests {
        // shown before the test runs, so that it is clear which one is running
        print!("test {:width$} ... ", test.name);
        let _ = std::io::stdout().flush();
        let mut command = std::process::Command::new(test.command[0]);
        // with the toolchain the project is configured with, as its commands are
        crate::toolchain::apply(&mut command, project.toolchain.as_deref());
        let test_started = Instant::now();
        let output = command
            .args(&test.command[1..])
            .current_dir(&project.root)
            .stdin(std::process::Stdio::null())
            .output();
        let elapsed = test_started.elapsed();
        let failure = match output {
            Ok(output) if output.status.success() => None,
            Ok(output) => {
                let mut report = format!("{}\n", output.status);
                report.push_str(&String::from_utf8_lossy(&output.stdout));
                report.push_str(&String::from_utf8_lossy(&output.stderr));
                Some(report)
            },
            Err(error) => Some(format!("failed to run {:?}: {error}\n", test.command[0])),
        };
        let result = if failure.is_some() { style.failed("FAILED") } else { style.ok("ok") };
        println!("{result} {}", style.dim(&format!("({})", duration(elapsed))));
        if let Some(report) = failure {
            failures.push((test.name, report));
        }
    }
    let elapsed = started.elapsed();
    for (name, report) in &failures {
        println!("\n{}\n{}", style.bold(&format!("---- {name} ----")), report.trim_end());
    }
    if !failures.is_empty() {
        println!("\nfailures:");
        for (name, _) in &failures {
            println!("    {name}");
        }
    }
    let result = if failures.is_empty() { style.ok("ok") } else { style.failed("FAILED") };
    let passed = tests.len() - failures.len();
    println!("\ntest result: {result}. {passed} passed; {} failed; finished in {}", failures.len(), duration(elapsed));
    if failures.is_empty() { Ok(()) } else { Err(Error::TestsFailed(failures.len())) }
}

/// Returns a duration as seconds, as libtest shows it.
fn duration(duration: Duration) -> String {
    format!("{:.2}s", duration.as_secs_f64())
}

/// How the results are styled, with colours when stdout is a terminal, as diagnostics are on stderr.
struct Style {
    pretty: bool,
}
impl Style {
    fn new(pretty: bool) -> Self {
        Self { pretty }
    }
    fn paint(&self, style: &str, text: &str) -> String {
        if self.pretty { format!("{style}{text}\x1b[0m") } else { text.into() }
    }
    fn ok(&self, text: &str) -> String {
        self.paint("\x1b[92m", text)
    }
    fn failed(&self, text: &str) -> String {
        self.paint("\x1b[91m\x1b[1m", text)
    }
    fn bold(&self, text: &str) -> String {
        self.paint("\x1b[1m", text)
    }
    fn dim(&self, text: &str) -> String {
        self.paint("\x1b[2m", text)
    }
}
