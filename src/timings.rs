//! What `--timings` shows on stderr once a command has finished: how long each of its steps took, configuring the
//! project, building it, running a program and running tests, and the commands of the builds which took longest.
//!
//! A step which is taken more than once, such as building, which `jig test` does before it runs the tests, is shown
//! once, as the total of each time it was taken.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most commands of the builds which are shown.
const SLOWEST: usize = 10;

/// What has been timed, once `--timings` turned timing on.
struct Timings {
    started: Instant,
    /// Each step, by its name, in the order they were first taken, with how long they took in all.
    steps: Vec<(&'static str, Duration)>,
    /// Each command of the builds, by its description, with how long it took.
    commands: Vec<(String, Duration)>,
}

static TIMINGS: Mutex<Option<Timings>> = Mutex::new(None);

/// Turn timing on, from now.
pub fn enable() {
    if let Ok(mut timings) = TIMINGS.lock() {
        *timings = Some(Timings { started: Instant::now(), steps: Vec::new(), commands: Vec::new() });
    }
}

/// Whether timing is on.
pub fn enabled() -> bool {
    TIMINGS.lock().is_ok_and(|timings| timings.is_some())
}

/// Take a step, adding how long it took to the step's time, if timing is on.
pub fn step<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = f();
    if let Ok(mut timings) = TIMINGS.lock()
        && let Some(timings) = timings.as_mut()
    {
        let took = started.elapsed();
        match timings.steps.iter_mut().find(|(step, _)| *step == name) {
            Some((_, total)) => *total += took,
            None => timings.steps.push((name, took)),
        }
    }
    result
}

/// A command of a build took this long, if timing is on.
pub fn command(description: String, took: Duration) {
    if let Ok(mut timings) = TIMINGS.lock()
        && let Some(timings) = timings.as_mut()
    {
        timings.commands.push((description, took));
    }
}

/// Returns what is shown of what was timed, or `None` if timing is off.
pub fn report() -> Option<String> {
    let timings = TIMINGS.lock().ok()?;
    let timings = timings.as_ref()?;
    Some(format(timings, timings.started.elapsed()))
}

fn format(timings: &Timings, total: Duration) -> String {
    use std::fmt::Write;
    let seconds = |duration: Duration| format!("{:.2}s", duration.as_secs_f64());
    let mut shown = String::from("\ntimings:\n");
    for (step, took) in &timings.steps {
        let _ = writeln!(shown, "  {step:<10} {:>8}", seconds(*took));
    }
    let _ = writeln!(shown, "  {:<10} {:>8}", "total", seconds(total));
    if !timings.commands.is_empty() {
        let mut commands: Vec<&(String, Duration)> = timings.commands.iter().collect();
        commands.sort_by(|(lhs, lhs_took), (rhs, rhs_took)| rhs_took.cmp(lhs_took).then_with(|| lhs.cmp(rhs)));
        let count = timings.commands.len();
        let plural = if count == 1 { "" } else { "s" };
        let shown_count = count.min(SLOWEST);
        let _ = writeln!(shown, "slowest {shown_count} of {count} command{plural}:");
        for (description, took) in commands.into_iter().take(SLOWEST) {
            let _ = writeln!(shown, "  {:>8}  {description}", seconds(*took));
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_steps_and_the_slowest_commands() {
        let timings = Timings {
            started: Instant::now(),
            steps: vec![("configure", Duration::from_millis(210)), ("build", Duration::from_millis(3400))],
            commands: (0..12).map(|i| (format!("RUSTC lib{i}.rlib"), Duration::from_millis(i * 100))).collect(),
        };
        let shown = format(&timings, Duration::from_millis(3700));
        let expected = "\ntimings:\n  configure     0.21s\n  build         3.40s\n  total         3.70s\n\
                        slowest 10 of 12 commands:\n     1.10s  RUSTC lib11.rlib\n     1.00s  RUSTC lib10.rlib\n";
        assert!(shown.starts_with(expected), "{shown}");
        assert!(shown.ends_with("     0.20s  RUSTC lib2.rlib\n"), "{shown}");
    }
}
