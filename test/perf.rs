//! Performance tests for `jig`.
//!
//! Each test writes a large project to a temporary directory and times `jig` on it, failing if it takes longer than a
//! budget, or, for those which should scale linearly, if four times the input takes much more than four times as long.
//! The budgets are far above the expected times, so that they catch pathological slowdowns rather than noise. They are
//! multiplied by the `JIG_PERF_SCALE` environment variable, for slow machines.
//!
//! As with the integration tests, the `jig` binary is found with the `CONFIGURE` environment variable, or as the `jig`
//! crate's output, in the target directory which contains the directory of this test harness.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Held by each test while it times `jig`, so that the tests, which run on several threads, do not slow each other.
static TIMING: Mutex<()> = Mutex::new(());

/// The number of times each command is run, of which the fastest is taken.
const RUNS: usize = 3;

/// A project in a temporary directory, removed when dropped.
struct Project {
    dir: PathBuf,
}
impl Project {
    /// Create a project from `(path, contents)` pairs. The first file should be the root `Jig`.
    fn new(name: &str, files: &[(String, String)]) -> Self {
        let dir = std::env::temp_dir().join(format!("jig-perf-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, contents) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }
    /// Returns the shortest time `jig` takes to run with the arguments, which must succeed.
    #[track_caller]
    fn time(&self, args: &[&str]) -> Duration {
        let jig = jig_path();
        (0..RUNS).map(|_| {
            let start = Instant::now();
            let output = Command::new(&jig)
                .args(args)
                .current_dir(&self.dir)
                .output()
                .unwrap_or_else(|error| panic!("failed to run {jig:?}: {error}"));
            let elapsed = start.elapsed();
            assert!(output.status.success(), "jig {args:?} failed:\n{}", String::from_utf8_lossy(&output.stderr));
            elapsed
        }).min().unwrap()
    }
    /// Returns the shortest time `jig generate` takes.
    #[track_caller]
    fn generate(&self) -> Duration {
        self.time(&["generate", "Jig"])
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn jig_path() -> PathBuf {
    match std::env::var_os("CONFIGURE") {
        Some(path) => std::path::absolute(path).unwrap(),
        None => std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jig"),
    }
}

/// Returns a budget scaled by `JIG_PERF_SCALE`.
fn budget(millis: u64) -> Duration {
    let scale = std::env::var("JIG_PERF_SCALE").ok().and_then(|scale| scale.parse::<f64>().ok()).unwrap_or(1.0);
    Duration::from_millis(millis).mul_f64(scale)
}

/// The project with only an empty `Jig`, whose time is that of starting `jig` and probing the compiler.
fn baseline() -> Duration {
    Project::new("baseline", &[("Jig".into(), String::new())]).generate()
}

/// Generate the project which `files` gives for `size`, failing if it takes longer than the budget.
#[track_caller]
fn assert_within(name: &str, size: usize, millis: u64, files: impl Fn(usize) -> Vec<(String, String)>) {
    let _timing = TIMING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let elapsed = Project::new(name, &files(size)).generate();
    println!("{name} {size}: {elapsed:?}");
    assert!(elapsed <= budget(millis), "{name} with {size} took {elapsed:?}, more than {:?}", budget(millis));
}

/// Generate the project which `files` gives for `size` and four times `size`, failing if, without the baseline, the
/// larger takes more than ten times as long, which a linear or `n log n` time would not, while a quadratic one would.
///
/// It is measured again before failing, in case another process slowed it. As the baseline varies, the smaller is taken
/// to take at least 20ms, and a difference too small to be measured reliably passes, so the sizes should be large
/// enough to take some time.
#[track_caller]
fn assert_linear(name: &str, size: usize, files: impl Fn(usize) -> Vec<(String, String)>) {
    let _timing = TIMING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let small_files = files(size);
    let large_files = files(size * 4);
    let mut ratio = 0.0;
    for _ in 0..2 {
        let base = baseline();
        let small = Project::new(name, &small_files).generate();
        let large = Project::new(name, &large_files).generate();
        println!("{name}: {base:?} baseline, {small:?} for {size}, {large:?} for {}", size * 4);
        let small = small.saturating_sub(base).max(Duration::from_millis(20));
        let large = large.saturating_sub(base);
        ratio = large.as_secs_f64() / small.as_secs_f64();
        if large < budget(50) || ratio <= 10.0 {
            return;
        }
    }
    panic!("{name} took {ratio:.1} times as long for 4 times the size");
}

fn root(contents: String) -> Vec<(String, String)> {
    vec![("Jig".into(), contents)]
}

/// A chain of properties, each set from the one before it.
fn properties(n: usize) -> Vec<(String, String)> {
    let mut jig = String::new();
    for i in 0..n {
        jig += &format!("declare p{i}\n    default {i}\n");
        if i > 0 {
            jig += &format!("set p{i} p{} + 1\n", i - 1);
        }
    }
    root(jig)
}

/// A statement repeated by `@for` for each of `n` values.
fn for_loop(n: usize) -> Vec<(String, String)> {
    let values: Vec<_> = (0..n).map(|i| i.to_string()).collect();
    root(format!("declare total\n    default 0\n@for i in {}\nset total total + i\n", values.join(" ")))
}

/// Statements whose expressions call functions and fill string templates.
fn expressions(n: usize) -> Vec<(String, String)> {
    let mut jig = String::from("declare a\n    default 1\ndeclare s\n");
    for i in 0..n {
        jig += &format!("set a (a * 3 + {i}) % 1000003\n");
        jig += &format!("set s '{{a}} {{upper('x{i}')}} {{len(split('a,b,c', ','))}} {{join(['x', a], '-')}}'\n");
    }
    root(jig)
}

/// Crates which each use the same crate.
fn crates_wide(n: usize) -> Vec<(String, String)> {
    let mut jig = String::from("crate base 'c.rs'\n");
    for i in 0..n {
        jig += &format!("crate c{i} 'c.rs'\n    use base\n");
    }
    vec![("Jig".into(), jig), ("c.rs".into(), String::new())]
}

/// Crates which each use the one before them.
fn crates_chain(n: usize) -> Vec<(String, String)> {
    let mut jig = String::from("crate c0 'c.rs'\n");
    for i in 1..n {
        jig += &format!("crate c{i} 'c.rs'\n    use c{}\n", i - 1);
    }
    vec![("Jig".into(), jig), ("c.rs".into(), String::new())]
}

/// Custom commands, each with an input and an output.
fn customs(n: usize) -> Vec<(String, String)> {
    let mut jig = String::new();
    for i in 0..n {
        jig += &format!("custom k{i}\n    input 'in{i}'\n    output 'out{i}'\n    command 'cp' input output\n");
    }
    root(jig)
}

/// Files which the root file uses, each in its own directory.
fn files(n: usize) -> Vec<(String, String)> {
    let mut files = vec![("Jig".into(), (0..n).map(|i| format!("use 'f{i}'\n")).collect())];
    files.extend((0..n).map(|i| (format!("f{i}/Jig"), format!("declare v{i}\n    default {i}\n"))));
    files
}

/// Files which each use the next, in the directory beside their own.
fn sibling_files(n: usize) -> Vec<(String, String)> {
    let mut files = vec![("Jig".into(), "use 'f0'\n".into())];
    files.extend((0..n).map(|i| {
        let next = if i + 1 < n { format!("use '../f{}'\n", i + 1) } else { String::new() };
        (format!("f{i}/Jig"), format!("declare v{i}\n    default {i}\n{next}"))
    }));
    files
}

/// Files which each use the file in the directory within their own.
fn nested_files(n: usize) -> Vec<(String, String)> {
    (0..n).map(|i| {
        let next = if i + 1 < n { "use 'd'\n" } else { "" };
        (format!("{}Jig", "d/".repeat(i)), format!("declare v{i}\n    default {i}\n{next}"))
    }).collect()
}

#[test]
fn empty_project() {
    let _timing = TIMING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let elapsed = baseline();
    println!("empty: {elapsed:?}");
    assert!(elapsed <= budget(2000), "an empty project took {elapsed:?}");
}

#[test]
fn many_properties() {
    assert_within("properties", 16000, 2000, properties);
    assert_linear("properties", 16000, properties);
}

#[test]
fn long_for_loop() {
    assert_within("for-loop", 25000, 2000, for_loop);
    assert_linear("for-loop", 25000, for_loop);
}

#[test]
fn many_expressions() {
    assert_within("expressions", 10000, 2000, expressions);
    assert_linear("expressions", 10000, expressions);
}

#[test]
fn many_crates() {
    assert_within("crates-wide", 2000, 3000, crates_wide);
    assert_linear("crates-wide", 1000, crates_wide);
}

#[test]
fn crate_chain() {
    assert_within("crates-chain", 4000, 3000, crates_chain);
    assert_linear("crates-chain", 1000, crates_chain);
}

#[test]
fn many_customs() {
    assert_within("customs", 4000, 3000, customs);
    assert_linear("customs", 4000, customs);
}

#[test]
fn many_files() {
    assert_within("files", 4000, 3000, files);
    assert_linear("files", 4000, files);
}

#[test]
fn deeply_nested_files() {
    // the path of each file is as long as its depth, which the kernel walks to open it, so only the time is checked,
    // at a depth whose paths fit within the 1024 bytes macOS allows
    let depth = if cfg!(target_os = "macos") { 300 } else { 1000 };
    assert_within("nested-files", depth, 3000, nested_files);
}

#[test]
fn chain_of_sibling_files() {
    // each path is resolved, rather than growing with each `..` until it is too long to open
    assert_within("sibling-files", 2000, 3000, sibling_files);
}

#[test]
fn up_to_date_build() {
    let mut jig = String::new();
    for i in 0..500 {
        jig += &format!("custom k{i}\n    output 'out{i}'\n    command 'touch' output\n    default\n");
    }
    let project = Project::new("up-to-date", &root(jig));
    let _timing = TIMING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    project.generate();
    project.time(&["build"]);
    // with nothing to do, neither the configuration nor ninja's work should take long
    let elapsed = project.time(&["build"]);
    println!("up-to-date build: {elapsed:?}");
    assert!(elapsed <= budget(1000), "an up-to-date build took {elapsed:?}");
}
