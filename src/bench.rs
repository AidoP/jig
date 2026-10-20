//! Micro-benchmarks, which time parts of `jig` in its unit tests, without starting it as a process.
//!
//! Each benchmark times an operation on inputs of a size and of four times the size, failing if the larger takes more
//! than ten times as long, which a linear or `n log n` time would not, while a quadratic one would. The times are
//! printed, which `--nocapture` shows. Unlike the `perf` tests, there is no budget for the time itself, as it varies
//! too much between machines to be checked.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::project::{File, Project};

/// Held while timing, so that the benchmarks, which run on several threads, do not slow each other.
static TIMING: Mutex<()> = Mutex::new(());

/// The shortest a sample may take, so that the clock and the loop are a small part of it.
const SAMPLE: Duration = Duration::from_millis(10);

/// The number of samples, of which the fastest is taken.
const SAMPLES: usize = 5;

/// Returns the time `run` takes, as the fastest of several samples, each of as many calls as take at least
/// [`SAMPLE`].
pub fn time(mut run: impl FnMut()) -> Duration {
    // the number of calls in a sample, doubled until a sample is long enough
    let mut calls = 1u32;
    loop {
        let start = Instant::now();
        for _ in 0..calls {
            run();
        }
        if start.elapsed() >= SAMPLE || calls >= 1 << 20 {
            break;
        }
        calls *= 2;
    }
    let fastest = (0..SAMPLES).map(|_| {
        let start = Instant::now();
        for _ in 0..calls {
            run();
        }
        start.elapsed()
    }).min().unwrap();
    fastest / calls
}

/// Time `run` on the input `setup` gives for `size` and for four times `size`, failing if the larger takes more than
/// ten times as long. It is timed again before failing, in case another process slowed it.
#[track_caller]
pub fn assert_linear<T>(name: &str, size: usize, setup: impl Fn(usize) -> T, mut run: impl FnMut(&T)) {
    let small_input = setup(size);
    let large_input = setup(size * 4);
    let _timing = TIMING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut ratio = 0.0;
    for _ in 0..2 {
        let small = time(|| run(std::hint::black_box(&small_input)));
        let large = time(|| run(std::hint::black_box(&large_input)));
        println!("{name}: {small:?} for {size}, {large:?} for {}", size * 4);
        ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
        if ratio <= 10.0 {
            return;
        }
    }
    panic!("{name} took {ratio:.1} times as long for 4 times the size");
}

/// The host target of the projects configured by [`configure`], which need not be the machine's.
pub const HOST: &str = "x86_64-unknown-linux-gnu";

/// Read a project from the source of its root file, `Jig`, and check it, as `jig generate` does, but with a toolchain
/// whose only target is [`HOST`], so that rustc is not run.
///
/// Statements which run programs, such as `run` or a crate with `test`, which probes whether its target can run, are
/// best avoided, as they would be timed too.
pub fn configure(source: &str) -> Project {
    let mut project = Project::new(std::env::temp_dir(), vec![HOST.into()]);
    crate::declare_builtins(&mut project, "/sysroot", HOST, "1.95.0".into(), &[HOST.into()], None);
    project.parse(File::new("Jig".into(), source.into()));
    if !project.fatal {
        project.check();
    }
    project
}
