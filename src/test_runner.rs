//! `jig test`, which runs the tests recorded in the database.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::Error;
use crate::database::{self, Database};
use crate::configured::{Configured, Output, Request};
use crate::rig_builder::Tests;

/// Which tests `jig test` runs.
#[derive(Default)]
pub struct Selection {
    /// The tests named, which run whatever their tags.
    pub names: Vec<String>,
    /// The tests with any of these tags.
    pub tags: Vec<String>,
    /// Every test.
    pub all: bool,
}

/// How `jig test` runs the tests it chooses.
pub struct Options {
    /// Show what every test printed, rather than only what the failed tests did.
    pub verbose: bool,
    /// How long a test may run here before it is killed, and fails, in place of its own timeout.
    pub timeout: Option<Duration>,
    /// The arguments each test is given after its own.
    pub args: Vec<String>,
    /// How what the tests need is built.
    pub build: crate::rig_builder::Options,
}

/// Returns what the arguments of `jig test` choose, and how: its options, the names of tests, and after `--`, the
/// arguments of the tests. `-j` is the number of jobs of both the build and the tests, while `-v` shows what the
/// tests print, rather than the commands of the build.
pub fn options(args: &crate::Args) -> Result<(Selection, Options), Error> {
    let selection = Selection { names: args.names(), tags: args.values("tag").map(String::from).collect(), all: args.flag("all") };
    let timeout = args.value("timeout").map(|value| {
        let seconds = value.parse::<f64>().ok().filter(|seconds| seconds.is_finite() && *seconds > 0.0);
        let timeout = seconds.and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        timeout.ok_or_else(|| Error::InvalidTimeout(value.into()))
    });
    let build = crate::rig_builder::Options { verbose: false, ..args.build()? };
    let options = Options {
        verbose: args.flag("verbose"),
        timeout: timeout.transpose()?,
        args: args.passed.iter().map(|arg| arg.to_string_lossy().into_owned()).collect(),
        build,
    };
    Ok((selection, options))
}

impl Selection {
    /// Returns the tests chosen, failing for a name or tag which no test has.
    ///
    /// Without names, tags or `all`, the tests are those without tags and those with a tag in `test-tags`.
    fn choose<'a, 'b>(&self, database: &'b Database<'a>) -> Result<Vec<&'b database::Test<'a>>, Error> {
        if let Some(name) = self.names.iter().find(|name| !database.tests.iter().any(|test| test.name == *name)) {
            return Err(Error::UnknownTest(name.clone()));
        }
        let tagged = |tag: &&String| database.tests.iter().any(|test| test.tags.contains(&tag.as_str()));
        if let Some(tag) = self.tags.iter().find(|tag| !tagged(tag)) {
            return Err(Error::UnknownTag(tag.clone()));
        }
        let default = !self.all && self.names.is_empty() && self.tags.is_empty();
        let chosen = |test: &&database::Test| {
            self.all
                || self.tags.iter().any(|tag| test.tags.contains(&tag.as_str()))
                || default && (test.tags.is_empty() || test.tags.iter().any(|tag| database.test_tags.contains(tag)))
        };
        // the tests named, in the order they were, then the others chosen, in the order they were declared
        let named = |name: &String| database.tests.iter().find(|test| test.name == name);
        let mut tests: Vec<_> = self.names.iter().filter_map(named).collect();
        for test in database.tests.iter().filter(chosen) {
            if !tests.iter().any(|chosen| chosen.name == test.name) {
                tests.push(test);
            }
        }
        Ok(tests)
    }
}

/// Run the tests a selection chooses, without evaluating the configuration.
///
/// A test is an edge of rig's graph which runs always, so the build which builds what it needs runs it too, as soon as
/// what it needs is built: in the build's jobs, or, for a test which runs on workers, on a worker whose host runs its
/// target. It is given the arguments `jig test` was given, the colour of its results, the timeout of `jig test`, if
/// there is one, and if it runs here, the search path of the project's shared libraries. What it printed, and how it
/// finished, are collected rather than shown by the build.
///
/// The results are shown once the build is done, in the order the tests were chosen, each with how long it took. The
/// output of a failed test is shown at the end. With `verbose`, the output of every test is shown below its result,
/// such as the results of the individual tests in a crate's test harness.
pub fn test(selection: &Selection, options: &Options) -> Result<(), Error> {
    let verbose = options.verbose;
    // which brings the configuration up to date, so the tests chosen are those the build has
    let project = Configured::find(Output::Stdout)?;
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let tests = selection.choose(&database)?;
    let style = Style::new(std::io::stdout().is_terminal());
    let library_path = crate::platform::library_path(&project.root, &database.library_path);
    let mut given = HashMap::new();
    let mut targets = Vec::new();
    for test in &tests {
        let mut args = options.args.clone();
        // a test harness colours its results as jig does, though its output is captured
        if test.libtest {
            args.push(if style.pretty { "--color=always" } else { "--color=never" }.into());
        }
        // which a test on a worker, which is never here, does not need, though it is given it
        let library_path = library_path.as_ref().map(|path| path.to_string_lossy().into_owned());
        let env = library_path.map(|path| (crate::platform::LIBRARY_PATH.to_string(), path)).into_iter().collect();
        let run = crate::generate::test_run(test.name);
        given.insert(run.clone(), rig::exec::Given { args, env, timeout: options.timeout });
        targets.push(run);
    }
    let started = Instant::now();
    let collected = RefCell::new(Tests { given, ..Tests::default() });
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
    let request = Request { targets: &targets, build: options.build, keep_going: true, tests: Some(&collected), ..Default::default() };
    let built = targets.is_empty() || project.builder().build(&request)?;
    let collected = collected.into_inner();
    // nothing ran, as the build could not start, or what every test needs failed to build
    if !built && collected.finished.is_empty() {
        return Err(Error::TestBuild);
    }
    let plural = if tests.len() == 1 { "" } else { "s" };
    // after what the build printed
    println!("\nrunning {} test{plural}", tests.len());
    // the results line up after the longest name
    let width = tests.iter().map(|test| test.name.chars().count()).max().unwrap_or(0);
    let mut failures = Vec::new();
    crate::timings::step("test", || {
        for test in &tests {
            print!("test {:width$} ... ", test.name);
            let _ = std::io::stdout().flush();
            let (passed, report, elapsed) = match collected.finished.get(&crate::generate::test_run(test.name)) {
                Some((outcome, output, took)) => {
                    let (passed, report) = reported(outcome, output);
                    (passed, report, *took)
                },
                None => (false, "not run, as what it needs was not built\n".to_string(), Duration::ZERO),
            };
            let result = if passed { style.ok("ok") } else { style.failed("FAILED") };
            println!("{result} {}", style.dim(&format!("({})", duration(elapsed))));
            if verbose {
                print!("{}", indent(&report));
            }
            if !passed {
                failures.push((test.name, report));
            }
        }
    });
    let elapsed = started.elapsed();
    // the output of each failure, unless it was shown already
    for (name, report) in failures.iter().filter(|_| !verbose) {
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

/// Returns whether a test passed, and its report: how it ended, unless it passed, then what it printed.
fn reported(outcome: &rig::exec::Outcome, output: &[u8]) -> (bool, String) {
    use rig::exec::Outcome;
    let mut report = String::new();
    match outcome {
        Outcome::Succeeded => (),
        Outcome::Failed(status) => _ = writeln!(report, "{status}"),
        Outcome::TimedOut(timeout) => _ = writeln!(report, "timed out after {}", duration(*timeout)),
        Outcome::Error(error) => _ = writeln!(report, "{error}"),
        Outcome::Interrupted => _ = writeln!(report, "interrupted"),
    }
    report.push_str(&String::from_utf8_lossy(output));
    (outcome.succeeded(), report)
}


/// Returns the output of a test indented below its result, without the blank lines around it.
fn indent(output: &str) -> String {
    let mut indented = String::new();
    for line in output.trim_matches('\n').lines() {
        if line.trim().is_empty() {
            indented.push('\n');
        } else {
            let _ = writeln!(indented, "    {line}");
        }
    }
    indented
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn parses_arguments() {
        let parse = |args: &[&str]| options(&crate::Args::parse("test", args.iter().map(OsString::from))?);
        let ok = |args: &[&str]| parse(args).unwrap_or_else(|error| panic!("{error}"));
        let (selection, options) = ok(&["-v", "-j", "3", "a", "--tag", "slow", "b", "--", "-x", "--all"]);
        assert_eq!((selection.names, selection.tags, selection.all), (vec!["a".into(), "b".into()], vec!["slow".into()], false));
        assert_eq!((options.verbose, options.build.jobs, options.timeout), (true, Some(3), None));
        // what follows `--` is for the tests, whatever it is
        assert_eq!(options.args, ["-x", "--all"]);
        let (selection, options) = ok(&["-j8", "--timeout", "1.5", "--all"]);
        assert!(selection.all && selection.names.is_empty());
        assert_eq!((options.build.jobs, options.timeout), (Some(8), Some(Duration::from_millis(1500))));
        // the build's jobs, and no timeout, by default
        let (_, options) = ok(&[]);
        assert_eq!((options.verbose, options.build.jobs, options.timeout), (false, None, None));
        // as `jig build` takes them
        let (_, options) = ok(&["-k", "0", "-q", "-v", "a"]);
        assert_eq!((options.build.keep_going, options.build.quiet), (Some(0), true));
        // where `-v` is about the tests
        assert!(options.verbose && !options.build.verbose);
        // an option it does not have is not the name of a test
        assert!(matches!(parse(&["--bogus"]), Err(Error::UnknownOption("test", option)) if option == "--bogus"));
        assert!(matches!(parse(&["-n"]), Err(Error::UnknownOption("test", option)) if option == "-n"));
        assert!(matches!(parse(&["-j0"]), Err(Error::InvalidJobs(_))));
        assert!(matches!(parse(&["-j", "many"]), Err(Error::InvalidJobs(_))));
        assert!(matches!(parse(&["-j"]), Err(Error::MissingArg("jobs"))));
        for timeout in ["0", "-1", "soon", "inf", "NaN"] {
            assert!(matches!(parse(&["--timeout", timeout]), Err(Error::InvalidTimeout(_))), "{timeout}");
        }
    }
}
