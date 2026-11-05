//! `jig test`, which runs the tests recorded in the database.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::Error;
use crate::builder::Request;
use crate::database::{self, Database};
use crate::configured::{Configured, Output};

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
/// What the tests need is built first, then each is run in turn from the project root with its output captured,
/// which is shown if it fails, with how long each took. With `verbose`, the output of every test is shown below its
/// result, such as the results of the tests in a crate's test harness, rather than only those which fail at the end.
pub fn test(selection: &Selection, verbose: bool) -> Result<(), Error> {
    let project = Configured::find(Output::Stdout)?;
    // the configuration is up to date, so the tests are chosen before what they need is built, which is only theirs
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let targets: Vec<String> = selection.choose(&database)?.iter().map(|test| format!("test/{}", test.name)).collect();
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
    let built = targets.is_empty() || project.builder().build(&Request { targets: &targets, ..Default::default() })?;
    if !built {
        return Err(Error::TestBuild);
    }
    // read after building, in case it reconfigured
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let tests = selection.choose(&database)?;

    let style = Style::new(std::io::stdout().is_terminal());
    let plural = if tests.len() == 1 { "" } else { "s" };
    // after what the build printed
    println!("\nrunning {} test{plural}", tests.len());
    // the results line up after the longest name
    let width = tests.iter().map(|test| test.name.chars().count()).max().unwrap_or(0);
    let started = Instant::now();
    let mut failures = Vec::new();
    // the tests which run on workers are sent at once, and run while those here run in turn
    let workers = Workers::new(&project, tests.iter().any(|test| !test.on_workers.is_empty()))?;
    std::thread::scope(|scope| {
        let mut sent: Vec<_> = tests
            .iter()
            .map(|test| (!test.on_workers.is_empty()).then(|| scope.spawn(|| workers.run(test, style.pretty))))
            .collect();
        for (test, sent) in tests.iter().zip(&mut sent) {
            // shown before the test runs, so that it is clear which one is running
            print!("test {:width$} ... ", test.name);
            let _ = std::io::stdout().flush();
            let (passed, report, elapsed) = match sent.take() {
                Some(sent) => {
                    let failed = || (false, "the test could not be sent\n".into(), Duration::ZERO);
                    sent.join().unwrap_or_else(|_| failed())
                },
                None => run_here(test, &project, style.pretty),
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

/// Run a test here, from the project root, with its output captured, returning whether it passed, what it printed,
/// with how it ended first if it failed, and how long it took.
fn run_here(test: &database::Test, project: &Configured, pretty: bool) -> (bool, String, Duration) {
    let mut command = std::process::Command::new(test.command[0]);
    // with the toolchain the project is configured with, as its commands are
    crate::toolchain::apply(&mut command, project.toolchain.as_deref());
    let test_started = Instant::now();
    command.args(&test.command[1..]);
    // a test harness colours its results as jig does, though its output is captured
    if test.libtest {
        command.arg(if pretty { "--color=always" } else { "--color=never" });
    }
    let output = command.current_dir(&project.root).stdin(std::process::Stdio::null()).output();
    let elapsed = test_started.elapsed();
    let (passed, report) = match output {
        Ok(output) => {
            let mut report = String::new();
            if !output.status.success() {
                let _ = writeln!(report, "{}", output.status);
            }
            report.push_str(&String::from_utf8_lossy(&output.stdout));
            report.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.success(), report)
        },
        Err(error) => (false, format!("failed to run {:?}: {error}\n", test.command[0])),
    };
    (passed, report, elapsed)
}

/// The workers tests are sent to, with the cache what they print is fetched into, and the project root, which the
/// paths of their files are relative to.
struct Workers {
    /// Each worker, by how it is given.
    named: Vec<(String, std::sync::Arc<rig::remote::Remote>)>,
    cache: Option<rig::cache::Cache>,
    root: std::path::PathBuf,
}

impl Workers {
    /// The workers of the project, if a test needs them.
    fn new(project: &Configured, needed: bool) -> Result<Self, Error> {
        let (named, cache) = if needed {
            let named = project.cache.workers.iter().cloned().zip(project.cache.workers()?).collect();
            (named, project.cache.open(false)?)
        } else {
            (Vec::new(), None)
        };
        Ok(Self { named, cache, root: project.root.clone() })
    }
    /// Run a test on the first worker which runs its target, sending it the test's files, returning whether it passed,
    /// what it printed, with why it failed first if it did, and how long it took, from when it was sent.
    fn run(&self, test: &database::Test, pretty: bool) -> (bool, String, Duration) {
        let started = Instant::now();
        let (passed, report) = match self.try_run(test, pretty) {
            Ok(ran) => ran,
            Err(why) => (false, format!("{why}\n")),
        };
        (passed, report, started.elapsed())
    }
    fn try_run(&self, test: &database::Test, pretty: bool) -> Result<(bool, String), String> {
        use std::os::unix::fs::PermissionsExt;
        let target = test.on_workers;
        let cache = self.cache.as_ref().ok_or("a test runs on a worker only with rig's cache, which is off")?;
        // each is asked what it is in turn, which a build asks once
        let mut hosts = Vec::new();
        let mut chosen = None;
        for (name, worker) in &self.named {
            match worker.capabilities() {
                Ok(capabilities) if runs(&capabilities.host, target) => {
                    chosen = Some((name, worker));
                    break;
                },
                Ok(capabilities) => hosts.push(format!("{name} runs {}", capabilities.host)),
                Err(error) => hosts.push(format!("{name} failed: {error}")),
            }
        }
        let Some((name, worker)) = chosen else {
            let hosts = if hosts.is_empty() { "there are none".to_string() } else { hosts.join(", ") };
            return Err(format!("no worker runs {target}, as {hosts}"));
        };
        let mut inputs = Vec::new();
        for path in &test.inputs {
            let full = self.root.join(path);
            let metadata = std::fs::metadata(&full).map_err(|error| format!("failed to read {full:?}: {error}"))?;
            let blob = rig::blake3::hash_file(&full).map_err(|error| format!("failed to read {full:?}: {error}"))?;
            let executable = metadata.permissions().mode() & 0o111 != 0;
            inputs.push(rig::worker::Input { path: (*path).to_string(), blob, executable });
        }
        let mut args: Vec<String> = test.command[1..].iter().map(|&arg| arg.to_string()).collect();
        if test.libtest {
            args.push(if pretty { "--color=always" } else { "--color=never" }.into());
        }
        let program = test.command[0].to_string();
        let action = rig::Action::Exec(rig::Exec { program, args, env: Vec::new(), dir: None, stderr: None });
        // it runs without a toolchain, with the worker's tools
        let request = rig::worker::Request {
            identity: String::new(),
            release: String::new(),
            install: String::new(),
            target: String::new(),
            action,
            env: Vec::new(),
            locating: ["PATH", "HOME", "TMPDIR"].map(String::from).to_vec(),
            inputs,
            outputs: Vec::new(),
        };
        let executed = worker.execute(&self.root, &request).map_err(|refused| match refused {
            rig::worker::Refused::Toolchain => format!("{name} refused the test"),
            rig::worker::Refused::Error(error) => format!("{name} failed to run the test: {error}"),
        })?;
        worker.fetch_blobs(cache, &[executed.entry.printed]);
        let printed = cache.read_blob(&executed.entry.printed);
        let printed = printed.map_err(|error| format!("failed to fetch what it printed: {error}"))?;
        let mut report = String::new();
        if !executed.succeeded {
            let _ = writeln!(report, "failed on {name}");
        }
        report.push_str(&String::from_utf8_lossy(&printed));
        Ok((executed.succeeded, report))
    }
}

/// Whether a worker which runs on a host runs what is built for a target: its own.
fn runs(host: &str, target: &str) -> bool {
    host == target
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
