//! `jig test`, which runs the tests recorded in the database.

use std::fmt::Write as _;
use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::Error;
use crate::database::{self, Database};
use crate::configured::{Configured, Output, Request};

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
    /// The most tests which run here at once.
    pub jobs: usize,
    /// How long a test may run here before it is killed, and fails.
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
        jobs: build.jobs.unwrap_or(1),
        timeout: timeout.transpose()?,
        args: args.passed.iter().map(|arg| arg.to_string_lossy().into_owned()).collect(),
        build,
    };
    Ok((selection, options))
}

/// What running a test gave: whether it passed, what it printed, which for a failure starts with how it ended, and
/// how long it took.
type Outcome = (bool, String, Duration);

/// The outcomes of the tests which run here, by their index among the tests chosen, each set once its test has
/// finished, which the thread that shows the results waits for in turn.
#[derive(Default)]
struct Finished {
    outcomes: Mutex<Vec<Option<Outcome>>>,
    changed: Condvar,
}
impl Finished {
    fn set(&self, index: usize, outcome: Outcome) {
        self.outcomes.lock().unwrap_or_else(PoisonError::into_inner)[index] = Some(outcome);
        self.changed.notify_all();
    }
    /// Wait for the outcome of a test, and take it.
    fn take(&self, index: usize) -> Outcome {
        let mut outcomes = self.outcomes.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            match outcomes[index].take() {
                Some(outcome) => return outcome,
                None => outcomes = self.changed.wait(outcomes).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }
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
/// What the tests need is built first. Each test is then run in turn, from the project root, with its output
/// captured, and its result is shown with how long it took. The output of a failed test is shown at the end. With
/// `verbose`, the output of every test is shown below its result, such as the results of the individual tests in a
/// crate's test harness.
pub fn test(selection: &Selection, options: &Options) -> Result<(), Error> {
    let verbose = options.verbose;
    let project = Configured::find(Output::Stdout)?;
    // the configuration is up to date, so the tests are chosen before what they need is built, which is only theirs
    let bytes = project.read()?;
    let database = project.parse(&bytes)?;
    let targets: Vec<String> = selection.choose(&database)?.iter().map(|test| format!("test/{}", test.name)).collect();
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
    let built = targets.is_empty() || project.builder().build(&Request { targets: &targets, build: options.build, ..Default::default() })?;
    if !built {
        return Err(Error::TestBuild);
    }
    // a build which a test starts may need the state, which is closed first
    crate::rig_builder::release();
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
    // the tests which run on workers are sent at once, and run while those here do
    let workers = Workers::new(&project, tests.iter().any(|test| !test.on_workers.is_empty()))?;
    let here: Vec<usize> = (0..tests.len()).filter(|&index| tests[index].on_workers.is_empty()).collect();
    let finished = Finished::default();
    finished.outcomes.lock().unwrap_or_else(PoisonError::into_inner).resize_with(tests.len(), || None);
    // the next of the tests which run here to start, which each job takes in turn, so that they start in order
    let next = AtomicUsize::new(0);
    let job = || {
        while let Some(&index) = here.get(next.fetch_add(1, Ordering::SeqCst)) {
            let outcome = run_here(tests[index], &project, &database.library_path, style.pretty, options);
            finished.set(index, outcome);
        }
    };
    crate::timings::step("test", || std::thread::scope(|scope| {
        let mut sent: Vec<_> = tests
            .iter()
            .map(|test| (!test.on_workers.is_empty()).then(|| scope.spawn(|| workers.run(test, style.pretty, options))))
            .collect();
        for _ in 0..options.jobs.min(here.len()) {
            scope.spawn(job);
        }
        // The results are shown in the order the tests were chosen, each once its test has finished, whichever
        // finished first, so that what is shown does not depend on the number of jobs.
        for (index, (test, sent)) in tests.iter().zip(&mut sent).enumerate() {
            // shown before the test finishes, so that it is clear which one is waited for
            print!("test {:width$} ... ", test.name);
            let _ = std::io::stdout().flush();
            let (passed, report, elapsed) = match sent.take() {
                Some(sent) => {
                    let failed = || (false, "the test could not be sent\n".into(), Duration::ZERO);
                    sent.join().unwrap_or_else(|_| failed())
                },
                None => finished.take(index),
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
    }));
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

/// Run a test here, from the project root, with its output captured, and with the directories of the project's
/// shared libraries added to the platform's search path for them. It is given the arguments of `jig test` after its
/// own, and is killed, and fails, if it runs for longer than the timeout.
fn run_here(
    test: &database::Test,
    project: &Configured,
    library_path: &[&str],
    pretty: bool,
    options: &Options,
) -> Outcome {
    let mut command = std::process::Command::new(test.command[0]);
    // finding the shared libraries of C which what it runs links
    crate::platform::add_library_path(&mut command, &project.root, library_path);
    // with the toolchain the project is configured with, as its commands are
    crate::toolchain::apply(&mut command, project.toolchain.as_deref());
    command.args(&test.command[1..]).args(&options.args).envs(test.env.iter().copied());
    // a test harness colours its results as jig does, though its output is captured
    if test.libtest {
        command.arg(if pretty { "--color=always" } else { "--color=never" });
    }
    command.current_dir(&project.root).stdin(std::process::Stdio::null());
    match run_captured(&mut command, options.timeout) {
        Ok(Captured { ended, stdout, stderr, elapsed }) => {
            let mut report = String::new();
            match ended {
                Ended::Exited(status) if status.success() => (),
                Ended::Exited(status) => _ = writeln!(report, "{status}"),
                Ended::TimedOut(timeout) => _ = writeln!(report, "timed out after {}", duration(timeout)),
            }
            report.push_str(&String::from_utf8_lossy(&stdout));
            report.push_str(&String::from_utf8_lossy(&stderr));
            (matches!(ended, Ended::Exited(status) if status.success()), report, elapsed)
        },
        Err(error) => (false, format!("failed to run {:?}: {error}\n", test.command[0]), Duration::ZERO),
    }
}

/// How a command which was run ended.
#[derive(Clone, Copy)]
enum Ended {
    Exited(std::process::ExitStatus),
    /// It ran for longer than this, and was killed.
    TimedOut(Duration),
}

/// A command which was run, with what it wrote.
struct Captured {
    ended: Ended,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// How long it ran, until it exited or was killed.
    elapsed: Duration,
}

/// How long what a command wrote is waited for once it has been killed. A process it started may still hold its
/// pipes open, which would otherwise be waited for as long as that process runs.
const DRAIN: Duration = Duration::from_millis(200);

/// Run a command with its stdout and stderr captured, killing it if it runs for longer than a timeout.
///
/// Only the command's own process is killed, not the processes it started. What it wrote before it was killed is
/// returned.
fn run_captured(command: &mut std::process::Command, timeout: Option<Duration>) -> std::io::Result<Captured> {
    use std::process::Stdio;
    let started = Instant::now();
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Each pipe is read on a thread of its own as it is written, so that the command never blocks on a full pipe,
    // and what was read is there to take even if the pipe is never closed.
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (done, ended) = std::sync::mpsc::channel();
        let shared = Arc::clone(&written);
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            let mut pipe = pipe;
            while let Some(Ok(len @ 1..)) = pipe.as_mut().map(|pipe| pipe.read(&mut buffer)) {
                shared.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(&buffer[..len]);
            }
            let _ = done.send(());
        });
        (written, ended)
    };
    let stdout = read(child.stdout.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as Box<dyn Read + Send>));
    let ended = match timeout {
        None => Ended::Exited(child.wait()?),
        Some(timeout) => {
            let deadline = Instant::now() + timeout;
            // polled, more often at first, so that a test which is quick is not held up
            let mut pause = Duration::from_millis(1);
            loop {
                if let Some(status) = child.try_wait()? {
                    break Ended::Exited(status);
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Ended::TimedOut(timeout);
                }
                std::thread::sleep(pause.min(left));
                pause = (pause * 2).min(Duration::from_millis(20));
            }
        },
    };
    let elapsed = started.elapsed();
    let drained = Instant::now() + DRAIN;
    let drain = |(written, done): (Arc<Mutex<Vec<u8>>>, std::sync::mpsc::Receiver<()>)| {
        match ended {
            Ended::Exited(_) => _ = done.recv(),
            Ended::TimedOut(_) => _ = done.recv_timeout(drained.saturating_duration_since(Instant::now())),
        }
        std::mem::take(&mut *written.lock().unwrap_or_else(PoisonError::into_inner))
    };
    Ok(Captured { ended, stdout: drain(stdout), stderr: drain(stderr), elapsed })
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
    fn run(&self, test: &database::Test, pretty: bool, options: &Options) -> Outcome {
        let started = Instant::now();
        let (passed, report) = match self.try_run(test, pretty, &options.args) {
            Ok(ran) => ran,
            Err(why) => (false, format!("{why}\n")),
        };
        (passed, report, started.elapsed())
    }
    fn try_run(&self, test: &database::Test, pretty: bool, given: &[String]) -> Result<(bool, String), String> {
        let target = test.on_workers;
        let cache = self.cache.as_ref().ok_or("a test runs on a worker only with rig's cache, which is off")?;
        // a statically linked harness runs on any Linux of its architecture, as it needs no loader
        let linked_statically = test.inputs.first().is_some_and(|harness| linked_statically(&self.root.join(harness)));
        // each is asked what it is in turn, which a build asks once
        let mut hosts = Vec::new();
        let mut chosen = None;
        for (name, worker) in &self.named {
            match worker.capabilities() {
                Ok(capabilities) if runs(&capabilities.host, target, linked_statically) => {
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
            let executable = crate::platform::is_program(&full, &metadata);
            inputs.push(rig::worker::Input { path: (*path).to_string(), blob, executable });
        }
        let mut args: Vec<String> = test.command[1..].iter().map(|&arg| arg.to_string()).collect();
        args.extend(given.iter().cloned());
        if test.libtest {
            args.push(if pretty { "--color=always" } else { "--color=never" }.into());
        }
        let program = test.command[0].to_string();
        let action = rig::Exec { program, args, env: Vec::new(), dir: None, stderr: None };
        // it runs without a toolchain, with the worker's tools
        let request = rig::worker::Request {
            identity: String::new(),
            release: String::new(),
            install: String::new(),
            target: String::new(),
            action,
            env: test.env.iter().map(|&(name, value)| (name.into(), value.into())).collect(),
            locating: ["PATH", "HOME", "TMPDIR"].map(String::from).to_vec(),
            inputs,
            outputs: Vec::new(),
            early: Vec::new(),
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

/// Whether a worker which runs on a host runs what is built for a target: its own, or, for what is linked statically
/// for Linux, any Linux of the same architecture, such as a static binary of musl on a host of glibc.
fn runs(host: &str, target: &str, linked_statically: bool) -> bool {
    let arch = |triple: &str| triple.split('-').next().map(String::from);
    let linux = |triple: &str| triple.split('-').any(|part| part == "linux");
    host == target || (linked_statically && linux(host) && linux(target) && arch(host) == arch(target))
}

/// Whether a file is an ELF executable which is linked statically: one without an interpreter, the dynamic loader,
/// which runs on any kernel of its architecture, as a static PIE does, though it is relocated by itself.
fn linked_statically(path: &std::path::Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| elf_without_interpreter(&bytes))
}

/// Whether bytes are an ELF executable without a program header of its interpreter, `PT_INTERP`.
fn elf_without_interpreter(bytes: &[u8]) -> bool {
    const PT_INTERP: u32 = 3;
    if bytes.get(..4) != Some(b"\x7fELF") {
        return false;
    }
    let (wide, little) = (bytes.get(4) == Some(&2), bytes.get(5) == Some(&1));
    let number = |at: usize, len: usize| -> Option<u64> {
        let field = bytes.get(at..at.checked_add(len)?)?;
        let mut value = [0; 8];
        if little {
            value[..len].copy_from_slice(field);
            Some(u64::from_le_bytes(value))
        } else {
            value[8 - len..].copy_from_slice(field);
            Some(u64::from_be_bytes(value))
        }
    };
    // where the program headers are, and how many and how large they are, which differ in place for 32 and 64 bits
    let headers = if wide {
        (number(0x20, 8), number(0x36, 2), number(0x38, 2))
    } else {
        (number(0x1c, 4), number(0x2a, 2), number(0x2c, 2))
    };
    let (Some(offset), Some(size), Some(count)) = headers else {
        return false;
    };
    let Ok(offset) = usize::try_from(offset) else {
        return false;
    };
    // the table of program headers is whole, so that one which is cut short is not taken as static
    let end = offset.checked_add((count as usize).saturating_mul(size as usize));
    if count == 0 || end.is_none_or(|end| end > bytes.len()) {
        return false;
    }
    (0..count as usize).all(|index| {
        let at = offset.saturating_add(index.saturating_mul(size as usize));
        number(at, 4).is_some_and(|kind| kind != u64::from(PT_INTERP))
    })
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

    /// Returns the bytes of an ELF executable of 64 or 32 bits, little-endian, with program headers of these kinds.
    fn elf(wide: bool, kinds: &[u32]) -> Vec<u8> {
        let (header, entry) = if wide { (64, 56) } else { (52, 32) };
        let mut bytes = vec![0; header];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = if wide { 2 } else { 1 };
        bytes[5] = 1;
        let (offset, size, count) = if wide { (0x20, 0x36, 0x38) } else { (0x1c, 0x2a, 0x2c) };
        let len = if wide { 8 } else { 4 };
        bytes[offset..offset + len].copy_from_slice(&(header as u64).to_le_bytes()[..len]);
        bytes[size..size + 2].copy_from_slice(&(entry as u16).to_le_bytes());
        bytes[count..count + 2].copy_from_slice(&(kinds.len() as u16).to_le_bytes());
        for &kind in kinds {
            let mut program = vec![0; entry];
            program[..4].copy_from_slice(&kind.to_le_bytes());
            bytes.extend(program);
        }
        bytes
    }

    #[test]
    fn finds_what_is_linked_statically() {
        // a program header of the loadable segment, of the dynamic section, and of the interpreter
        let (load, dynamic, interp) = (1, 2, 3);
        for wide in [true, false] {
            assert!(elf_without_interpreter(&elf(wide, &[load, load])));
            // as a static PIE has a dynamic section, but no interpreter
            assert!(elf_without_interpreter(&elf(wide, &[load, dynamic])));
            assert!(!elf_without_interpreter(&elf(wide, &[load, interp, dynamic])));
        }
        // what is not ELF, or is cut short, is not
        assert!(!elf_without_interpreter(b"#!/bin/sh\n"));
        assert!(!elf_without_interpreter(&elf(true, &[load])[..70]));
        // jig itself is linked as its target is
        let jig = std::env::current_exe().unwrap();
        let _ = linked_statically(&jig);
    }

    #[test]
    fn parses_arguments() {
        let parse = |args: &[&str]| options(&crate::Args::parse("test", args.iter().map(OsString::from))?);
        let ok = |args: &[&str]| parse(args).unwrap_or_else(|error| panic!("{error}"));
        let (selection, options) = ok(&["-v", "-j", "3", "a", "--tag", "slow", "b", "--", "-x", "--all"]);
        assert_eq!((selection.names, selection.tags, selection.all), (vec!["a".into(), "b".into()], vec!["slow".into()], false));
        assert_eq!((options.verbose, options.jobs, options.timeout), (true, 3, None));
        // what follows `--` is for the tests, whatever it is
        assert_eq!(options.args, ["-x", "--all"]);
        let (selection, options) = ok(&["-j8", "--timeout", "1.5", "--all"]);
        assert!(selection.all && selection.names.is_empty());
        assert_eq!((options.jobs, options.timeout), (8, Some(Duration::from_millis(1500))));
        // one job at a time, and no timeout, by default
        let (_, options) = ok(&[]);
        assert_eq!((options.verbose, options.jobs, options.timeout), (false, 1, None));
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

    #[test]
    fn waits_for_outcomes_in_any_order() {
        let finished = Finished::default();
        finished.outcomes.lock().unwrap().resize_with(2, || None);
        std::thread::scope(|scope| {
            // the second finishes first, and the first is still waited for
            scope.spawn(|| {
                finished.set(1, (false, "second".into(), Duration::ZERO));
                std::thread::sleep(Duration::from_millis(20));
                finished.set(0, (true, "first".into(), Duration::ZERO));
            });
            assert_eq!(finished.take(0).1, "first");
            assert_eq!(finished.take(1).1, "second");
        });
    }

    #[test]
    fn finds_which_workers_run_a_target() {
        let (gnu, musl) = ("riscv64gc-unknown-linux-gnu", "riscv64gc-unknown-linux-musl");
        assert!(runs(gnu, gnu, false));
        assert!(!runs(gnu, musl, false));
        // what is linked statically runs on any Linux of its architecture, but no other
        assert!(runs(gnu, musl, true));
        assert!(runs("x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu", true));
        assert!(!runs("aarch64-unknown-linux-gnu", musl, true));
        assert!(!runs("riscv64gc-unknown-freebsd", musl, true));
    }
}
