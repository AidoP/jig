//! Differential tests of rig against ninja, in the unit tests.
//!
//! Each test generates a random graph from a seed, writes it through jig's [`Graph`] trait as both `build.ninja` and
//! rig's graph, and builds it with ninja in one directory and rig in another, of the same files. A random sequence of
//! edits follows, each followed by a build with both, after which the same edges must have run, with the same
//! outcome, and the outputs must be the same.
//!
//! The graphs have explicit, implicit and order-only inputs, implicit outputs, aliases, edges which discover inputs
//! from their depfiles, and restat edges, which leave an output alone when it would not change. The edits change the
//! contents of sources, touch them without changing them, delete outputs, and build a few targets rather than the
//! defaults. They leave out what rig rebuilds on purpose where ninja does not, such as a source whose time goes back,
//! or an output changed by hand.
//!
//! The tests are skipped where ninja is not installed. `JIG_DIFFERENTIAL_SEEDS` sets the number of seeds, which is
//! `16` by default.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::graph::{Edge, Graph, Rule, Tee};
use crate::ninja::NinjaFile;
use crate::rig_graph::{self, RigGraph};

/// A pseudo-random number generator, xorshift64*, which is enough to choose graphs and edits from a seed.
struct Random(u64);
impl Random {
    fn new(seed: u64) -> Self {
        // the state must not be zero
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// Returns a number below `n`, which must not be zero.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// Returns whether a chance of `percent` in a hundred came up.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// An edge of a generated graph.
struct Spec {
    rule: &'static str,
    output: String,
    implicit_outputs: Vec<String>,
    inputs: Vec<String>,
    implicit: Vec<String>,
    order_only: Vec<String>,
    /// The headers the edge discovers, which its depfile lists, for the `discover` rule.
    discovered: Vec<String>,
}

/// A generated graph: its sources, which include the headers edges discover, its edges and its aliases.
struct Generated {
    sources: Vec<String>,
    edges: Vec<Spec>,
    aliases: Vec<(String, Vec<String>)>,
}

/// Every edge logs its id to this file when it runs, which the test reads to find what ran.
const LOG: &str = "runs.log";

fn generate(random: &mut Random) -> Generated {
    let mut sources: Vec<String> = (0..3 + random.below(5)).map(|i| format!("s{i}.txt")).collect();
    let headers: Vec<String> = (0..2 + random.below(3)).map(|i| format!("h{i}.h")).collect();
    // what an edge may take as an explicit input, which must be a file
    let mut files = sources.clone();
    let mut produced: Vec<String> = Vec::new();
    let mut aliases: Vec<(String, Vec<String>)> = Vec::new();
    let mut edges = Vec::new();
    for i in 0..3 + random.below(10) {
        let rule = match random.below(100) {
            0..60 => "step",
            60..85 => "discover",
            _ => "gen",
        };
        let output = if random.chance(30) { format!("out/o{i}") } else { format!("o{i}") };
        let extra = rule == "step" && random.chance(20);
        let implicit_outputs = if extra { vec![format!("o{i}.extra")] } else { Vec::new() };
        let mut inputs = Vec::new();
        for _ in 0..=random.below(3) {
            let input = random.pick(&files).clone();
            if !inputs.contains(&input) {
                inputs.push(input);
            }
        }
        let mut implicit = Vec::new();
        if random.chance(30) {
            let input = random.pick(&files).clone();
            if !inputs.contains(&input) {
                implicit.push(input);
            }
        }
        // an alias is only an implicit input, as it is not a file which `$in` could name, and not of a restat edge, as
        // ninja runs such an edge again the next time it builds after it left its output alone, though nothing it
        // depends on changed, which rig does not
        if rule != "gen" && !aliases.is_empty() && random.chance(25) {
            implicit.push(random.pick(&aliases).0.clone());
        }
        let order_only = if !produced.is_empty() && random.chance(20) {
            let input = random.pick(&produced).clone();
            if inputs.contains(&input) || implicit.contains(&input) { Vec::new() } else { vec![input] }
        } else {
            Vec::new()
        };
        let discovered = if rule == "discover" {
            let mut discovered = vec![random.pick(&headers).clone()];
            discovered.dedup();
            discovered
        } else {
            Vec::new()
        };
        files.push(output.clone());
        files.extend(implicit_outputs.iter().cloned());
        produced.push(output.clone());
        edges.push(Spec { rule, output, implicit_outputs, inputs, implicit, order_only, discovered });
        if random.chance(15) {
            let targets = vec![random.pick(&produced).clone()];
            aliases.push((format!("alias{}", aliases.len()), targets));
        }
    }
    sources.extend(headers);
    Generated { sources, edges, aliases }
}

/// Write a generated graph through jig's [`Graph`] trait, as `build.ninja` in one directory and rig's graph in
/// another.
fn write(generated: &Generated, ninja_dir: &Path, rig_dir: &Path) {
    let mut ninja = NinjaFile::new(ninja_dir.join(crate::ninja::FILE)).unwrap_or_else(|error| panic!("{error}"));
    let mut rig = RigGraph::new(rig_dir.join(rig_graph::FILE));
    let mut graph = Tee(&mut ninja, &mut rig);
    let log = format!(" && echo $id >> {LOG}");
    let step = format!("cat $in /dev/null > $out$extra{log}");
    let discover = format!("cat $in $headers /dev/null > $out && echo \"$out: $headers\" > $out.d{log}");
    let restat = format!("cat $in > $out.tmp && (cmp -s $out.tmp $out || mv $out.tmp $out); rm -f $out.tmp{log}");
    let rules = [
        Rule { name: "step", command: &step, description: "STEP $out", ..Default::default() },
        Rule {
            name: "discover",
            command: &discover,
            description: "DISCOVER $out",
            depfile: Some("$out.d"),
            ..Default::default()
        },
        Rule { name: "gen", command: &restat, description: "GEN $out", ..Default::default() },
    ];
    for rule in &rules {
        graph.rule(rule).unwrap_or_else(|error| panic!("{error}"));
    }
    let paths = |paths: &[String]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    for (id, spec) in generated.edges.iter().enumerate() {
        let id = id.to_string();
        // the variables of an edge are evaluated where it is defined, before `$out` is, so the output is named
        let extra = spec.implicit_outputs.iter().fold(String::new(), |mut extra, output| {
            let _ = write!(extra, " && cp {} {output}", spec.output);
            extra
        });
        let headers = spec.discovered.join(" ");
        let output = [spec.output.clone()];
        let lists = [&output[..], &spec.implicit_outputs, &spec.inputs, &spec.implicit, &spec.order_only];
        let [outputs, implicit_outputs, inputs, implicit, order_only] = lists.map(paths);
        let mut variables = vec![("id", id.as_str()), ("extra", extra.as_str()), ("headers", headers.as_str())];
        if spec.rule == "gen" {
            variables.push(("restat", "1"));
        }
        graph
            .edge(&Edge {
                rule: spec.rule,
                outputs: &as_paths(&outputs),
                implicit_outputs: &as_paths(&implicit_outputs),
                inputs: &as_paths(&inputs),
                implicit: &as_paths(&implicit),
                order_only: &as_paths(&order_only),
                variables: &variables,
                // the commands are shell commands, which both engines run with `sh`
                exec: None,
                hashed: false,
            })
            .unwrap_or_else(|error| panic!("{error}"));
    }
    for (name, targets) in &generated.aliases {
        let targets = paths(targets);
        let targets: Vec<&Path> = targets.iter().map(PathBuf::as_path).collect();
        graph.alias(Path::new(name), &targets).unwrap_or_else(|error| panic!("{error}"));
    }
    graph.finish().unwrap_or_else(|error| panic!("{error}"));
}

fn as_paths(paths: &[PathBuf]) -> Vec<&Path> {
    paths.iter().map(PathBuf::as_path).collect()
}

/// What a build did: whether it succeeded, and the ids of the edges which ran.
#[derive(Debug, PartialEq, Eq)]
struct Ran {
    succeeded: bool,
    edges: BTreeSet<usize>,
}

/// Returns the ids of the edges which logged that they ran, clearing the log.
fn take_log(dir: &Path) -> BTreeSet<usize> {
    let log = std::fs::read_to_string(dir.join(LOG)).unwrap_or_default();
    let _ = std::fs::remove_file(dir.join(LOG));
    log.lines().map(|line| line.parse().unwrap()).collect()
}

/// Build with ninja, returning what it did and what it showed.
fn build_ninja(dir: &Path, targets: &[&str]) -> (Ran, String) {
    let output = Command::new("ninja").arg("-C").arg(dir).args(["-j4", "-k0"]).args(targets).output().unwrap();
    let shown = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    (Ran { succeeded: output.status.success(), edges: take_log(dir) }, shown)
}

fn build_rig(dir: &Path, targets: &[&str]) -> Ran {
    let graph = rig::Graph::load(&dir.join(rig_graph::FILE)).unwrap_or_else(|error| panic!("{error}"));
    let mut state = rig::state::State::open(&dir.join(".rig")).unwrap_or_else(|error| panic!("{error}"));
    let mut options = rig::exec::Options::new(dir);
    options.jobs = 4;
    options.keep_going = 0;
    let mut console = rig::exec::Console { out: Vec::new(), quiet: true, verbose: false };
    let report = rig::exec::build(&graph, &mut state, &options, targets, &mut console);
    let report = report.unwrap_or_else(|error| panic!("{error}"));
    Ran { succeeded: report.succeeded(), edges: take_log(dir) }
}

/// An edit to the files of a build, made to both directories.
#[derive(Debug)]
enum Edit {
    /// Append to a source, changing its contents.
    Change(String),
    /// Give a source a new modification time without changing it, after which a restat edge leaves its output alone.
    Touch(String),
    /// Delete an output, which is built again.
    Delete(String),
    /// Change nothing.
    Nothing,
}

fn choose_edit(random: &mut Random, generated: &Generated) -> Edit {
    match random.below(100) {
        0..40 => Edit::Change(random.pick(&generated.sources).clone()),
        40..65 => Edit::Touch(random.pick(&generated.sources).clone()),
        65..85 => Edit::Delete(random.pick(&generated.edges).output.clone()),
        _ => Edit::Nothing,
    }
}

fn apply(edit: &Edit, dir: &Path) {
    use std::io::Write;
    match edit {
        Edit::Change(path) => {
            let mut file = std::fs::File::options().append(true).open(dir.join(path)).unwrap();
            writeln!(file, "changed").unwrap();
        },
        Edit::Touch(path) => {
            let file = std::fs::File::options().write(true).open(dir.join(path)).unwrap();
            file.set_modified(std::time::SystemTime::now()).unwrap();
        },
        Edit::Delete(path) => {
            let _ = std::fs::remove_file(dir.join(path));
        },
        Edit::Nothing => (),
    }
}

/// Returns the contents of the outputs of a build, which are the same if the same edges ran.
fn outputs(generated: &Generated, dir: &Path) -> Vec<(String, Option<String>)> {
    let paths = generated.edges.iter().flat_map(|spec| std::iter::once(&spec.output).chain(&spec.implicit_outputs));
    paths.map(|path| (path.clone(), std::fs::read_to_string(dir.join(path)).ok())).collect()
}

/// Build a graph generated from a seed with both ninja and rig, through a sequence of edits, comparing them after each.
///
/// With `verbose`, each step is shown, with what each engine did, and what ninja showed.
fn compare(seed: u64, verbose: bool) {
    let mut random = Random::new(seed);
    let generated = generate(&mut random);
    let base = std::env::temp_dir().join(format!("jig-differential-{}-{seed}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (ninja_dir, rig_dir) = (base.join("ninja"), base.join("rig"));
    for dir in [&ninja_dir, &rig_dir] {
        std::fs::create_dir_all(dir).unwrap();
        for (i, source) in generated.sources.iter().enumerate() {
            std::fs::write(dir.join(source), format!("source {i}\n")).unwrap();
        }
    }
    write(&generated, &ninja_dir, &rig_dir);
    let describe = || std::fs::read_to_string(ninja_dir.join(crate::ninja::FILE)).unwrap_or_default();
    let mut edit = Edit::Nothing;
    for step in 0..12 {
        // a few targets, rather than the defaults
        let targets: Vec<String> = if random.chance(20) {
            let spec = random.pick(&generated.edges);
            vec![spec.output.clone()]
        } else {
            Vec::new()
        };
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
        let (ninja, shown) = build_ninja(&ninja_dir, &targets);
        let rig = build_rig(&rig_dir, &targets);
        if verbose {
            eprintln!("step {step}, after {edit:?}, building {targets:?}:");
            eprintln!("  ninja: {ninja:?}\n  rig:   {rig:?}\n{shown}");
        }
        assert_eq!(
            rig,
            ninja,
            "seed {seed}, step {step}, after {edit:?}, building {targets:?}: rig (left), ninja (right), in\n{}",
            describe(),
        );
        assert_eq!(outputs(&generated, &rig_dir), outputs(&generated, &ninja_dir), "seed {seed}, step {step}");
        edit = choose_edit(&mut random, &generated);
        // past the granularity of the file system's clock, so that ninja finds a changed file is newer
        std::thread::sleep(Duration::from_millis(15));
        apply(&edit, &ninja_dir);
        apply(&edit, &rig_dir);
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn rig_builds_as_ninja_does() {
    if !Command::new("ninja").arg("--version").output().is_ok_and(|output| output.status.success()) {
        eprintln!("ninja is not installed, so rig is not compared with it");
        return;
    }
    // the builds would slow the micro-benchmarks, which run beside the test
    let _timing = crate::bench::TIMING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // one seed, to look into a difference
    if let Ok(seed) = std::env::var("JIG_DIFFERENTIAL_SEED") {
        compare(seed.parse().unwrap(), true);
        return;
    }
    let seeds = std::env::var("JIG_DIFFERENTIAL_SEEDS").map_or(16, |seeds| seeds.parse().unwrap());
    // a few seeds run at once, on threads of their own, as each waits on its commands
    let next = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                loop {
                    let seed = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if seed >= seeds {
                        break;
                    }
                    compare(seed, false);
                }
            });
        }
    });
}
