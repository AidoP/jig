//! `jig test`, which runs the tests recorded in the database.

use crate::Error;
use crate::configured::{Configured, Output};

/// Run tests, or all of them if no names are given, without evaluating the configuration.
///
/// What the tests need is built by ninja first, then each is run in turn from the project root with its output captured,
/// which is shown if it fails.
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

    let mut failures = Vec::new();
    for test in &tests {
        let output = std::process::Command::new(test.command[0])
            .args(&test.command[1..])
            .current_dir(&project.root)
            .stdin(std::process::Stdio::null())
            .output();
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
        println!("test {} ... {}", test.name, if failure.is_some() { "FAILED" } else { "ok" });
        if let Some(report) = failure {
            failures.push((test.name, report));
        }
    }
    for (name, report) in &failures {
        println!("\n---- {name} ----\n{}", report.trim_end());
    }
    let result = if failures.is_empty() { "ok" } else { "FAILED" };
    println!("\ntest result: {result}. {} passed; {} failed", tests.len() - failures.len(), failures.len());
    if failures.is_empty() { Ok(()) } else { Err(Error::TestsFailed(failures.len())) }
}
