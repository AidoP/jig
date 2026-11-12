//! Differential tests of rig against a model of what a build must do, in the unit tests.
//!
//! Each test generates a random graph from a seed, writes it as rig's graph, and builds it with rig. A random sequence
//! of edits follows, each followed by a build with rig, and by the [`Model`], after which rig must have run the edges
//! the model did, with the same outcome, and the outputs must be as the model has them.
//!
//! The model is the oracle: a simulation of the graph, without files or processes, which says which edges must run by
//! rig's rules, and what each of them writes, small enough to be read in one sitting. ninja was the oracle before it,
//! and the model was checked against it, agreeing on every seed, before ninja was removed.
//!
//! The graphs have inputs, dependencies, what an edge is built after, edges of several outputs, aliases, edges which
//! discover inputs from their depfiles, and restat edges, which leave an output alone when it would not change. The
//! edits change the contents of sources, touch them without changing them, delete outputs, and build a few targets
//! rather than the defaults. They leave out what the model leaves out, as a source whose time goes back, or an output
//! changed by hand.
//!
//! `JIG_DIFFERENTIAL_SEEDS` sets the number of seeds, which is `16` by default.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use crate::rig_graph;

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
    extra_outputs: Vec<String>,
    inputs: Vec<String>,
    dependencies: Vec<String>,
    after: Vec<String>,
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
        let extra_outputs = if extra { vec![format!("o{i}.extra")] } else { Vec::new() };
        let mut inputs = Vec::new();
        for _ in 0..=random.below(3) {
            let input = random.pick(&files).clone();
            if !inputs.contains(&input) {
                inputs.push(input);
            }
        }
        let mut dependencies = Vec::new();
        if random.chance(30) {
            let input = random.pick(&files).clone();
            if !inputs.contains(&input) {
                dependencies.push(input);
            }
        }
        // an alias is only a dependency, as it is not a file which `cat` could read, and not of a restat edge, as
        // when the model was checked against ninja, which runs such an edge again the next time it builds after it left
        // its output alone, though nothing it depends on changed, which rig does not
        if rule != "gen" && !aliases.is_empty() && random.chance(25) {
            dependencies.push(random.pick(&aliases).0.clone());
        }
        let after = if !produced.is_empty() && random.chance(20) {
            let input = random.pick(&produced).clone();
            if inputs.contains(&input) || dependencies.contains(&input) { Vec::new() } else { vec![input] }
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
        files.extend(extra_outputs.iter().cloned());
        produced.push(output.clone());
        edges.push(Spec { rule, output, extra_outputs, inputs, dependencies, after, discovered });
        if random.chance(15) {
            let targets = vec![random.pick(&produced).clone()];
            aliases.push((format!("alias{}", aliases.len()), targets));
        }
    }
    sources.extend(headers);
    Generated { sources, edges, aliases }
}

/// Write a generated graph as rig's graph, whose commands are steps of the [test helper](rig::test_helper).
fn write(generated: &Generated, dir: &Path) {
    let mut graph = rig::Graph::new();
    let mut nodes = |paths: &[String]| paths.iter().map(|path| graph.file(path)).collect::<Vec<_>>();
    let mut edges = Vec::new();
    for (id, spec) in generated.edges.iter().enumerate() {
        let output = &spec.output;
        let words = |words: &[&str]| words.iter().map(|&word| word.to_string()).collect::<Vec<_>>();
        let mut steps = Vec::new();
        let (description, depfile) = match spec.rule {
            "step" => {
                steps.extend(words(&["cat"]).into_iter().chain(spec.inputs.iter().cloned()));
                steps.extend(words(&[">", output]));
                for copy in &spec.extra_outputs {
                    steps.extend(words(&["&&", "cp", output, copy]));
                }
                ("STEP", None)
            },
            "discover" => {
                let read = spec.inputs.iter().chain(&spec.discovered).cloned();
                steps.extend(words(&["cat"]).into_iter().chain(read));
                steps.extend(words(&[">", output, "&&", "echo", &format!("{output}:")]));
                steps.extend(spec.discovered.iter().cloned());
                steps.extend(words(&[">", &format!("{output}.d")]));
                ("DISCOVER", Some(format!("{output}.d")))
            },
            // the output is left alone if it would not change
            _ => {
                steps.extend(words(&["restat", output]).into_iter().chain(spec.inputs.iter().cloned()));
                ("GEN", None)
            },
        };
        steps.extend(words(&["&&", "echo", &id.to_string(), ">>", LOG]));
        edges.push(rig::Edge {
            kind: rig::EdgeKind::Command(rig::test_helper::exec(steps)),
            description: format!("{description} {output}"),
            outputs: nodes(&std::iter::once(output).chain(&spec.extra_outputs).cloned().collect::<Vec<_>>()),
            inputs: nodes(&spec.inputs),
            dependencies: nodes(&spec.dependencies),
            after: nodes(&spec.after),
            depfile,
            restat: spec.rule == "gen",
            ..rig::Edge::default()
        });
    }
    let aliases: Vec<_> = generated.aliases.iter().map(|(name, targets)| (name, nodes(targets))).collect();
    for edge in edges {
        graph.add_edge(edge).unwrap_or_else(|error| panic!("{error}"));
    }
    for (name, targets) in aliases {
        graph.add_alias(name, &targets).unwrap_or_else(|error| panic!("{error}"));
    }
    graph.save(&dir.join(rig_graph::FILE)).unwrap_or_else(|error| panic!("{error}"));
}

/// A model of a build of a generated graph, which says which edges must run, and what they write, without files or
/// processes: each file has its contents and the logical time it was last written, and each edge which has run, the
/// times of its inputs and outputs then, as rig records their stamps.
///
/// An edge which is wanted runs if it has not run, or one of its outputs is missing, or was written since by other than
/// it, or one of its inputs, with the targets of an alias in its place, and the headers it discovered when it last ran,
/// has another time than it had then. Its order-only inputs are built first, but do not run it. The edges are generated
/// in an order in which each comes after those it depends on, so a build runs them in that order.
struct Model<'a> {
    generated: &'a Generated,
    files: HashMap<String, (String, u64)>,
    clock: u64,
    /// For each edge which has run, the times of its inputs and outputs when it last did, and what it discovered.
    records: HashMap<usize, Record>,
}

struct Record {
    inputs: Vec<(String, Option<u64>)>,
    outputs: Vec<(String, Option<u64>)>,
    discovered: Vec<String>,
}

impl<'a> Model<'a> {
    fn new(generated: &'a Generated) -> Self {
        let mut model = Self { generated, files: HashMap::new(), clock: 0, records: HashMap::new() };
        for (i, source) in generated.sources.iter().enumerate() {
            model.write(source, format!("source {i}\n"));
        }
        model
    }
    fn write(&mut self, path: &str, contents: String) {
        self.clock += 1;
        self.files.insert(path.into(), (contents, self.clock));
    }
    fn time(&self, path: &str) -> Option<u64> {
        self.files.get(path).map(|&(_, time)| time)
    }
    fn contents(&self, path: &str) -> String {
        self.files.get(path).map(|(contents, _)| contents.clone()).unwrap_or_default()
    }
    fn apply(&mut self, edit: &Edit) {
        match edit {
            Edit::Change(path) => {
                let contents = self.contents(path) + "changed\n";
                self.write(path, contents);
            },
            Edit::Touch(path) => {
                let contents = self.contents(path);
                self.write(path, contents);
            },
            Edit::Delete(path) => {
                self.files.remove(path);
            },
            Edit::Nothing => (),
        }
    }
    /// Returns the inputs an edge depends on, with the targets of an alias in its place, and those it discovered.
    fn inputs(&self, id: usize) -> Vec<String> {
        let spec = &self.generated.edges[id];
        let aliases = |input: &String| match self.generated.aliases.iter().find(|(name, _)| name == input) {
            Some((_, targets)) => targets.clone(),
            None => vec![input.clone()],
        };
        let mut inputs: Vec<String> = spec.inputs.iter().chain(&spec.dependencies).flat_map(aliases).collect();
        inputs.extend(self.records.get(&id).into_iter().flat_map(|record| record.discovered.iter().cloned()));
        inputs
    }
    fn outputs(&self, id: usize) -> Vec<String> {
        let spec = &self.generated.edges[id];
        std::iter::once(&spec.output).chain(&spec.extra_outputs).cloned().collect()
    }
    /// Returns the edges which building targets wants, the edges which produce them and those they depend on, or every
    /// edge without targets.
    fn wanted(&self, targets: &[&str]) -> BTreeSet<usize> {
        let edges = &self.generated.edges;
        if targets.is_empty() {
            return (0..edges.len()).collect();
        }
        let writes = |spec: &Spec, path: &str| spec.output == path || spec.extra_outputs.iter().any(|o| o == path);
        let producer = |path: &str| edges.iter().position(|spec| writes(spec, path));
        let mut wanted = BTreeSet::new();
        let mut stack: Vec<usize> = targets.iter().filter_map(|target| producer(target)).collect();
        while let Some(id) = stack.pop() {
            if wanted.insert(id) {
                let after = edges[id].after.iter().cloned();
                let inputs = self.inputs(id).into_iter().chain(after);
                stack.extend(inputs.filter_map(|input| producer(&input)));
            }
        }
        wanted
    }
    /// Build targets, returning the edges which ran, in the order they did.
    fn build(&mut self, targets: &[&str]) -> BTreeSet<usize> {
        let mut ran = BTreeSet::new();
        for id in self.wanted(targets) {
            let inputs: Vec<(String, Option<u64>)> = self.inputs(id).into_iter().map(|input| {
                let time = self.time(&input);
                (input, time)
            }).collect();
            let outputs: Vec<(String, Option<u64>)> =
                self.outputs(id).into_iter().map(|output| (output.clone(), self.time(&output))).collect();
            let up_to_date = self.records.get(&id).is_some_and(|record| {
                outputs.iter().all(|(_, time)| time.is_some()) && record.outputs == outputs && record.inputs == inputs
            });
            if up_to_date {
                continue;
            }
            self.run(id);
            ran.insert(id);
        }
        ran
    }
    /// Run an edge, as its command does, recording what it read and wrote.
    fn run(&mut self, id: usize) {
        let spec = &self.generated.edges[id];
        // `cat` of its inputs, and the headers which an edge which discovers reads
        let read = spec.inputs.iter().chain(&spec.discovered).map(|input| self.contents(input)).collect::<String>();
        // a restat edge leaves its output alone, and its time, if it would not change
        if spec.rule != "gen" || self.files.get(&spec.output).is_none_or(|(contents, _)| *contents != read) {
            self.write(&spec.output, read.clone());
        }
        for output in &spec.extra_outputs {
            self.write(output, read.clone());
        }
        // recorded once it has run, as rig records it, with its inputs as it read them, and what it discovered
        let discovered = spec.discovered.clone();
        self.records.insert(id, Record { inputs: Vec::new(), outputs: Vec::new(), discovered });
        let inputs = self.inputs(id).into_iter().map(|input| {
            let time = self.time(&input);
            (input, time)
        }).collect();
        let outputs = self.outputs(id).into_iter().map(|output| (output.clone(), self.time(&output))).collect();
        if let Some(record) = self.records.get_mut(&id) {
            record.inputs = inputs;
            record.outputs = outputs;
        }
    }
    /// Returns the contents of the outputs, as [`outputs`] reads them from a build's directory.
    fn outputs_contents(&self) -> Vec<(String, Option<String>)> {
        let edges = self.generated.edges.iter();
        let paths = edges.flat_map(|spec| std::iter::once(&spec.output).chain(&spec.extra_outputs));
        paths.map(|path| (path.clone(), self.files.get(path).map(|(contents, _)| contents.clone()))).collect()
    }
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

fn build_rig(dir: &Path, targets: &[&str]) -> Ran {
    let graph = rig::Graph::load(&dir.join(rig_graph::FILE)).unwrap_or_else(|error| panic!("{error}"));
    let mut state = open_state(&dir.join(".rig"));
    let mut options = rig::exec::Options::new(dir);
    options.jobs = 4;
    options.keep_going = 0;
    let mut console = rig::exec::Console::new(Vec::new(), true, false, false);
    let report = rig::exec::build(&graph, &mut state, &options, targets, &mut console);
    let report = report.unwrap_or_else(|error| panic!("{error}"));
    Ran { succeeded: report.succeeded(), edges: take_log(dir) }
}

/// Opens the state of a build, which the test's other threads can hold for a moment after this one closed it: a
/// process which one of them is starting has a copy of every descriptor until it runs its program, the lock's
/// among them.
fn open_state(dir: &Path) -> rig::state::State {
    let mut tries = 0;
    loop {
        match rig::state::State::open(dir) {
            Err(rig::Error::Locked(_)) if tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            },
            state => return state.unwrap_or_else(|error| panic!("{error}")),
        }
    }
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
        // written again unchanged, so that its time is the file system's clock, as the outputs' are, rather than the
        // precise time now, which can be later than an output written after it within the file system's tick, which
        // would be older than its input
        Edit::Touch(path) => {
            let path = dir.join(path);
            let contents = std::fs::read(&path).unwrap();
            std::fs::write(&path, contents).unwrap();
        },
        Edit::Delete(path) => {
            let _ = std::fs::remove_file(dir.join(path));
        },
        Edit::Nothing => (),
    }
}

/// Returns the contents of the outputs of a build, which are the same if the same edges ran.
fn outputs(generated: &Generated, dir: &Path) -> Vec<(String, Option<String>)> {
    let paths = generated.edges.iter().flat_map(|spec| std::iter::once(&spec.output).chain(&spec.extra_outputs));
    paths.map(|path| (path.clone(), std::fs::read_to_string(dir.join(path)).ok())).collect()
}

/// Build a graph generated from a seed with rig, through a sequence of edits, comparing it with the model after each.
///
/// With `verbose`, each step is shown, with what rig and the model did.
fn compare(seed: u64, verbose: bool) {
    let mut random = Random::new(seed);
    let generated = generate(&mut random);
    let dir = std::env::temp_dir().join(format!("jig-differential-{}-{seed}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (i, source) in generated.sources.iter().enumerate() {
        std::fs::write(dir.join(source), format!("source {i}\n")).unwrap();
    }
    write(&generated, &dir);
    let mut model = Model::new(&generated);
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
        let expected = Ran { succeeded: true, edges: model.build(&targets) };
        let rig = build_rig(&dir, &targets);
        let context = || format!("seed {seed}, step {step}, after {edit:?}, building {targets:?}");
        if verbose {
            eprintln!("{}:\n  model: {expected:?}\n  rig:   {rig:?}", context());
        }
        assert_eq!(rig, expected, "{}: rig (left), the model (right)", context());
        assert_eq!(outputs(&generated, &dir), model.outputs_contents(), "{}", context());
        edit = choose_edit(&mut random, &generated);
        // past the granularity of the file system's clock, so that a changed file has a new time
        std::thread::sleep(Duration::from_millis(15));
        model.apply(&edit);
        apply(&edit, &dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rig_builds_as_the_model_says() {
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
