//! Generation of the files which build a project.
//!
//! - In the build directory: the specifications of the project's custom targets, and rig's graph, `graph`, with its
//!   depfile.
//! - At the project root: `.rust-project.json`, and the database which `jig build`, `jig check`, `jig test` and
//!   `jig install` read.
//!
//! rig builds in the build directory, so the paths in the graph are relative to it. The project's own paths are
//! relative to the project root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::project::rust::{Crate, Kind};
use crate::project::{
    CustomKind, GeneratedFile, Installed, Native, NativeKind, Project, Severity, Target, TargetKind, has_line_break,
};
use crate::rig_graph::{self, Edge, RigGraph};
use crate::{Error, database, rust_analyzer, target_spec};

/// Write the files generated from a project, which has been read and checked.
///
/// `input` is the root build file.
pub fn write(mut project: Project, input: &Path, rustc: &crate::rustc::Answers) -> Result<(), Error> {
    let sysroot = &rustc.sysroot;
    let build_dir = project.build_dir();
    if let Err(error) = std::fs::create_dir_all(&build_dir) {
        return Err(Error::Create(build_dir, error));
    }
    let at = Rebase::new(&build_dir);
    write_target_specs(&project.targets, &build_dir, project.toolchain.as_deref())?;
    write_generated_files(&project.generated)?;
    forget_stale_archives(&project)?;

    let installed = project.installed();
    let mut crates: Vec<_> = std::mem::take(&mut project.crates).into_values().collect();
    crates.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
    // identifying the linkers adds to what the configuration depends on, so it comes before writing the depfile
    identify_linkers(&mut project, &crates);
    write_depfile(&project, &at, &build_dir)?;
    let units = units(&project, &crates, sysroot, &build_dir);
    let graph = RigGraph::new(build_dir.join(rig_graph::FILE));
    write_graph(graph, &project, &crates, &units, &installed, input, &at)?;
    write_rust_project(sysroot, &units)?;
    write_database(&project, &units, &installed, input, &at, &build_dir, rustc)
}

/// Rebases the paths of a project, which are relative to its root, for rig, which runs in the build directory.
///
/// Every rebased path is relative to the build directory, so that what rustc, which also runs there, is given and
/// writes does not depend on where the project is. A path within the build directory is relative to it. Any other
/// path leads out of it with `..`, starting from where the build directory really is once symbolic links are
/// followed, so that it reaches the same file when the build directory is a link.
///
/// rustc writes the paths of sources in its dep-info exactly as it is given them, which is what rig needs to read.
/// What it writes elsewhere, such as in diagnostics and debug info, is remapped to be relative to the project root,
/// by [`Rebase::remap`].
struct Rebase {
    /// The project root, which is absolute.
    root: PathBuf,
    /// The build directory, which is absolute.
    build_dir: PathBuf,
    /// The build directory with symbolic links followed, which is the directory `..` actually leaves.
    physical: PathBuf,
}
impl Rebase {
    fn new(build_dir: &Path) -> Self {
        let root = normalize(Path::new(""), &std::env::current_dir().unwrap_or_default());
        let build_dir = normalize(&root, build_dir);
        let physical = rig::canonicalize(&build_dir).unwrap_or_else(|_| build_dir.clone());
        Self { root, build_dir, physical }
    }
    fn path(&self, path: &Path) -> PathBuf {
        let path = normalize(&self.root, path);
        match path.strip_prefix(&self.build_dir) {
            Ok(relative) if relative.as_os_str().is_empty() => PathBuf::from("."),
            Ok(relative) => relative.to_path_buf(),
            Err(_) => relative_to(&self.physical, &path),
        }
    }
    /// Returns the arguments which make rustc, rustdoc and clippy write paths relative to the project root, rather
    /// than the build directory. They apply to everything but dep-info. `root_module` is the root module of the crate,
    /// rebased.
    ///
    /// - The build directory, which rustc writes as the working directory in debug info, becomes the project root, `.`.
    /// - The path from the build directory to the root is removed from the paths of sources outside the build
    ///   directory.
    /// - A root module inside the build directory, such as a generated one, gets the build directory put before it.
    ///   The prefix remapped is the first component of its path, which its modules share.
    ///
    /// rustc applies the last prefix which matches a path.
    fn remap(&self, root_module: &Path) -> Vec<String> {
        let mut remap = vec![format!("--remap-path-prefix={}=.", self.build_dir.display())];
        let up = self.path(&self.root);
        if up != Path::new(".") {
            remap.push(format!("--remap-path-prefix={}=", up.display()));
            let first = root_module.components().next();
            if let Some(std::path::Component::Normal(first)) = first {
                let build_dir = relative_to(&self.root, &self.build_dir).join(first);
                let first = first.to_string_lossy();
                remap.push(format!("--remap-path-prefix={first}={}", build_dir.display()));
            }
        }
        remap
    }
    fn paths<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
        paths.into_iter().map(|path| self.path(path)).collect()
    }
}

/// Returns `path` relative to `dir`, which must both be absolute and normalized: a `..` for each component of `dir`
/// which the path does not share, then the rest of the path. Returns `.` if they are the same.
fn relative_to(dir: &Path, path: &Path) -> PathBuf {
    let (mut dir, mut path) = (dir.components(), path.components());
    // skip the components they share
    loop {
        let (mut dir_rest, mut path_rest) = (dir.clone(), path.clone());
        match (dir_rest.next(), path_rest.next()) {
            (Some(lhs), Some(rhs)) if lhs == rhs => (dir, path) = (dir_rest, path_rest),
            _ => break,
        }
    }
    let mut relative = PathBuf::with_capacity(dir.as_path().as_os_str().len() + path.as_path().as_os_str().len());
    relative.extend(dir.map(|_| std::path::Component::ParentDir));
    relative.extend(path);
    if relative.as_os_str().is_empty() { PathBuf::from(".") } else { relative }
}

/// Returns a path, joined to `base` if it is relative, with its `.` and `..` components resolved, for rig's graph.
/// `base` must already be resolved.
///
/// A `..` removes the component before it. If that component is a symbolic link, the link is followed first, as `..`
/// leaves the directory the link points to, not the one which contains the link. The path is otherwise left as it
/// is, so it need not exist yet.
fn normalize(base: &Path, path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normal = PathBuf::with_capacity(base.as_os_str().len() + path.as_os_str().len() + 1);
    normal.push(base);
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                if normal.symlink_metadata().is_ok_and(|metadata| metadata.is_symlink())
                    && let Ok(target) = rig::canonicalize(&normal)
                {
                    normal = target;
                }
                normal.pop();
            },
            component => normal.push(component),
        }
    }
    normal
}

/// A crate which can be checked by rust-analyzer's flycheck.
pub struct Unit {
    /// The label of the flycheck, which is unique.
    pub label: String,
    pub name: String,
    pub root: PathBuf,
    pub target: String,
    /// The specification of the target, if it is a custom target.
    pub spec: Option<PathBuf>,
    /// Whether the crate is compiled as a test.
    pub test: bool,
    pub kind: Kind,
    /// The library or binary built from the crate.
    pub output: PathBuf,
    /// The indices of the crates passed with `--extern`.
    pub deps: Vec<usize>,
    /// Whether the crate is edited, rather than part of the toolchain.
    pub member: bool,
    /// The file which declares the crate.
    pub build_file: PathBuf,
    /// The codegen options of the crate, which can change its `cfg`, so it is checked with them.
    pub codegen: Vec<String>,
    /// The configuration options of the crate, given with `--cfg`.
    pub cfg: Vec<String>,
    /// The environment variables rustc is run with for the crate.
    pub env: Vec<(String, String)>,
    /// The environment variables rustc inherits for the crate.
    pub pass_env: Vec<String>,
    /// The tags a worker must have for rig to send it the crate's commands.
    pub worker_tags: Vec<String>,
    /// Whether rig stamps the crate's files by their contents, by its `change-detection` or the property's.
    pub hashed: bool,
    /// Whether rig caches what is built of the crate, when it caches.
    pub cached: bool,
    /// The directory of the outputs for the target.
    pub dir: PathBuf,
    /// The name of the crate's documentation, `doc/<name>`, if it is documented.
    pub doc: Option<String>,
    /// Whether the crate is built by `jig build` without targets, which `jig run` prefers.
    pub default: bool,
    /// The name of the crate's lint, `lint/<name>`, if it is edited, so that it is linted.
    pub lint: Option<String>,
    /// The flags which give clippy's lints their levels.
    pub lints: Vec<String>,
    /// Whether the crate has a test harness. If so it is linted as the harness, so that its tests are linted too. A
    /// library is still checked without its tests, as the crates which use it use the metadata of that check.
    pub harness: bool,
}
impl Unit {
    fn check_dir(&self) -> PathBuf {
        self.dir.join("check")
    }
    fn check_rmeta(&self) -> PathBuf {
        self.check_dir().join(format!("lib{}.rmeta", self.name))
    }
    /// The directory of the documentation of the crates for the target. Each crate's documentation is in a directory
    /// named after it, beside the files rustdoc shares between crates.
    fn doc_dir(&self) -> PathBuf {
        self.dir.join("doc")
    }
    fn lint_dir(&self) -> PathBuf {
        self.dir.join("lint")
    }
    fn lint_rmeta(&self) -> PathBuf {
        self.lint_dir().join(format!("lib{}.rmeta", self.name))
    }
    /// The file of the diagnostics from linting the crate, as clippy shows them.
    pub fn lint_diagnostics(&self) -> PathBuf {
        self.lint_dir().join(format!("{}.txt", self.name))
    }
    /// The first page of the crate's documentation.
    fn doc_index(&self) -> PathBuf {
        self.doc_dir().join(&self.name).join("index.html")
    }
    /// The file of JSON diagnostics from checking the crate.
    pub fn diagnostics(&self) -> PathBuf {
        self.check_dir().join(format!("{}.json", self.name))
    }
}

/// How rustc is told the target of a crate.
struct TargetFlag {
    target: String,
    /// The specification of a custom target, which the crate depends on.
    spec: Option<PathBuf>,
}
impl TargetFlag {
    fn new(target: &str, project: &Project, build_dir: &Path) -> Self {
        // a builtin target, even if it is overridden, keeps its specification
        let custom = project.target(target).is_some_and(Target::is_custom);
        let spec = custom.then(|| target_spec::path(build_dir, target));
        Self { target: target.into(), spec }
    }
}

/// A crate passed to another with `--extern`.
struct Extern<'a> {
    name: &'a str,
    /// The library, or metadata, of the crate.
    path: PathBuf,
}

/// Write the specification of each custom target.
fn write_target_specs(targets: &[Target], build_dir: &Path, toolchain: Option<&str>) -> Result<(), Error> {
    for target in targets {
        let TargetKind::Custom { inherit, fields, .. } = &target.kind else {
            continue;
        };
        let inherited = match inherit {
            Some(base) => Some(target_spec_json(&target.name, base, toolchain)?),
            None => None,
        };
        let path = target_spec::path(build_dir, &target.name);
        let spec = match target_spec::spec(fields, inherited.as_deref()) {
            Ok(spec) => spec,
            Err(error) => return Err(Error::TargetSpec(target.name.clone(), error)),
        };
        // only written when it changes, as every crate for the target depends on it
        if std::fs::read_to_string(&path).is_ok_and(|current| current == spec) {
            continue;
        }
        let written = std::fs::create_dir_all(path.parent().unwrap()).and_then(|()| std::fs::write(&path, spec));
        if let Err(error) = written {
            return Err(Error::Write(path, error));
        }
    }
    Ok(())
}

/// Write the files generated by `file` statements. Each is only written when its contents change, so that what
/// uses it is not rebuilt needlessly. Each is given its permissions, which are set again if they were changed.
fn write_generated_files(files: &[GeneratedFile]) -> Result<(), Error> {
    for file in files {
        let text = file.text();
        if !std::fs::read(&file.output).is_ok_and(|current| current == text.as_bytes()) {
            if let Some(dir) = file.output.parent() && let Err(error) = std::fs::create_dir_all(dir) {
                return Err(Error::Create(dir.into(), error));
            }
            if let Err(error) = std::fs::write(&file.output, text) {
                return Err(Error::Write(file.output.clone(), error));
            }
        }
        if std::fs::metadata(&file.output).is_ok_and(|metadata| crate::platform::has_mode(&metadata, file.mode)) {
            continue;
        }
        if let Err(error) = crate::platform::set_mode(&file.output, file.mode) {
            return Err(Error::Write(file.output.clone(), error));
        }
    }
    Ok(())
}

/// Write the depfile of the graph, `graph.d`, which says when the graph must be regenerated: when a file the
/// configuration read changes, when a file it found missing is created, or when an environment variable it read
/// changes. Variables are listed as `# env-dep:` lines, as rustc writes them, and rig treats them all as inputs of
/// the edge which generates the graph.
fn write_depfile(project: &Project, at: &Rebase, build_dir: &Path) -> Result<(), Error> {
    let graph = rig_graph::FILE;
    let path = build_dir.join(format!("{graph}.d"));
    // a line break cannot be escaped in a depfile
    let files = project.files.iter().map(|file| &file.path);
    if let Some(broken) = files.chain(&project.dependencies).find(|path| has_line_break(&path.to_string_lossy())) {
        return Err(Error::LineBreak(path, broken.display().to_string()));
    }
    let mut file = match std::fs::File::create(&path) {
        Ok(file) => file,
        Err(error) => return Err(Error::Create(path, error)),
    };
    if let Err(error) = write_dependencies(&mut file, graph, project, at) {
        return Err(Error::Write(path, error));
    }
    Ok(())
}

fn write_dependencies(
    w: &mut dyn std::io::Write,
    graph: &str,
    project: &Project,
    at: &Rebase,
) -> Result<(), std::io::Error> {
    write!(w, "{graph}:")?;
    // Builtin files are left out, as they only change with jig, which the configuration already depends on. A file
    // which was processed more than once is listed once.
    let mut seen = std::collections::HashSet::new();
    let files = project.files.iter().filter(|file| !file.builtin && seen.insert(&file.path)).map(|file| &file.path);
    // the files generated by `file` statements are listed too, so that one which is removed is written again
    let generated = project.generated.iter().map(|file| &file.output);
    let paths = files.chain(&project.dependencies).chain(generated).chain(&project.missing);
    for path in at.paths(paths) {
        // escape the characters which are special in a depfile
        let path = path.display().to_string().replace(' ', "\\ ").replace('#', "\\#").replace('$', "$$");
        write!(w, " {path}")?;
    }
    writeln!(w)?;
    for (name, _) in &project.env {
        writeln!(w, "# env-dep:{name}")?;
    }
    Ok(())
}

/// Returns the crates as units for rust-analyzer, in the same order.
fn units(project: &Project, crates: &[Crate], sysroot: &str, build_dir: &Path) -> Vec<Unit> {
    let host = project.host();
    // a value which is neither is reported where the property was set
    let hashed = project.change_detection() == "hash";
    let mut units = Vec::new();
    for (c, deps) in crates.iter().zip(externs(crates)) {
        // crates from the toolchain, such as `core`, are not edited
        let member = !rust_analyzer::absolute(&c.path).starts_with(sysroot);
        units.push(Unit {
            label: format!("{}@{}", c.name, c.target),
            name: c.name.clone(),
            root: c.path.clone(),
            target: c.target.clone(),
            spec: TargetFlag::new(&c.target, project, build_dir).spec,
            // a library is checked without `--test`, as its metadata is used by the crates which depend on it
            test: c.test && !matches!(c.kind, Kind::Lib | Kind::ProcMacro | Kind::StaticLib | Kind::CDylib),
            kind: c.kind,
            output: c.output(),
            deps,
            member,
            build_file: project.files[c.name_span.file].path.clone(),
            codegen: codegen(c, project),
            cfg: c.cfg.clone(),
            env: c.env.clone(),
            pass_env: c.pass_env.clone(),
            worker_tags: c.worker_tags.clone(),
            hashed: c.change_detection.as_deref().map_or(hashed, |detection| detection == "hash"),
            cached: c.cached,
            dir: c.dir.clone(),
            doc: c.doc.then(|| c.short_name(&host)),
            default: c.default,
            lint: member.then(|| c.short_name(&host)),
            lints: c.lints.clone(),
            harness: c.test,
        });
    }
    units
}

/// Write the build graph of a project.
fn write_graph(
    mut graph: RigGraph,
    project: &Project,
    crates: &[Crate],
    units: &[Unit],
    installed: &[Installed],
    input: &Path,
    at: &Rebase,
) -> Result<(), Error> {
    // the file which the graph is written to, which its generator edge regenerates
    let generated = rig_graph::FILE;
    // custom targets' specifications are in the build directory, and their paths are rebased from it
    let build_dir = &at.build_dir;
    // whether rig stamps the files of an edge by their contents
    let hashed = match project.change_detection().as_str() {
        "timestamp" => false,
        "hash" => true,
        other => return Err(Error::ChangeDetection(other.into())),
    };
    // When caching is on, rig caches the results of rustc and clippy, by the identity of the toolchain. It does not
    // cache rustdoc, which writes a directory.
    let cache = project.cache.as_deref();
    // The graph is regenerated by the same `jig` which generated it, and again when that `jig` changes. A project
    // which builds its own `jig`, as this one does, therefore has it built first.
    let exe = std::env::current_exe().ok();
    let configure = exe.clone().unwrap_or_else(|| "jig".into());
    // it runs with the toolchain it was configured with, from the project root, as its arguments are relative to that
    let toolchain = project.toolchain.as_deref();
    let exe = at.paths(&exe);
    let mut args: Vec<String> = toolchain.map(|toolchain| format!("+{toolchain}")).into_iter().collect();
    args.extend(["generate".to_string(), input.display().to_string()]);
    let generated = Path::new(generated);
    graph.edge(Edge {
        description: describe("CONFIGURE", generated),
        outputs: &[generated],
        inputs: &as_paths(&[at.path(input)].into_iter().chain(exe).collect::<Vec<_>>()),
        exec: program(&configure.display().to_string(), args, Some(&at.root), Vec::new()),
        depfile: Some(&depfile_of(generated)),
        console: true,
        generator: true,
        hashed,
        ..Default::default()
    })?;

    // the targets built when none are named; if there are none of these, `everything` is built
    let mut defaults: Vec<PathBuf> = Vec::new();
    // every target but the checks, which are only for rust-analyzer
    let mut everything: Vec<PathBuf> = Vec::new();
    // the documentation of the crates
    let mut docs: Vec<PathBuf> = Vec::new();
    // the lints of the crates, which like the checks are not built unless they are named
    let mut lints: Vec<PathBuf> = Vec::new();

    let mut tests = Vec::new();
    for custom in &project.customs {
        // A test has an alias of its inputs and program, and an edge which runs always, after them, which `jig test`
        // builds. One which runs on workers is placed on a worker whose host runs its target, never here.
        let CustomKind::Command { outputs, default, edge, .. } = &custom.kind else {
            let alias = Path::new("test").join(&custom.name);
            let inputs = custom.inputs(&project.runners);
            let inputs = at.paths(inputs.iter().chain(&custom.program).chain(&custom.depends));
            graph.alias(&alias, &as_paths(&inputs))?;
            if let Some(run) = custom.test_run() {
                // from the project root, with the toolchain if it runs here, and its own variables after it
                let mut command = custom.command(&project.runners);
                let on_workers = custom.on_workers();
                // A program in the build directory is given by its absolute path, which rig sends to a worker with
                // the worker's build directory in its place, as that is not beside the project root there.
                if on_workers.is_some() && is_path(&command[0]) {
                    let program = normalize(&at.root, Path::new(&command[0]));
                    if program.starts_with(&at.build_dir) {
                        command[0] = program.display().to_string();
                    }
                }
                let mut env = if on_workers.is_some() { Vec::new() } else { toolchain_env(toolchain) };
                env.extend(custom.running.env.iter().cloned());
                let placement = rig::Placement {
                    runs: on_workers.map(String::from),
                    tags: if on_workers.is_some() { custom.running.worker_tags.clone() } else { Vec::new() },
                    ..Default::default()
                };
                let exec = program(&command[0], command[1..].to_vec(), Some(&at.path(&at.root)), env);
                graph.edge(Edge {
                    description: format!("TEST {}", custom.name),
                    outputs: &[Path::new(&test_run(&custom.name))],
                    inputs: &[&alias],
                    exec,
                    serial: run.serial,
                    always: true,
                    timeout: run.timeout,
                    placement,
                    ..Default::default()
                })?;
            }
            tests.push(alias);
            continue;
        };
        let outputs = at.paths(outputs);
        // It runs from the project root, or its `dir`, given as a path relative to the build directory, so that the
        // action does not depend on where the project is, which matters for its cache key.
        let root = at.path(edge.dir.as_deref().unwrap_or(&at.root));
        let running = &custom.running;
        let mut env = toolchain_env(toolchain);
        env.extend(running.env.iter().cloned());
        let exec = program(&custom.command[0], custom.command[1..].to_vec(), Some(&root), env);
        let depfile = edge.depfile.as_ref().map(|path| at.path(path));
        graph.edge(Edge {
            description: edge.description.clone().unwrap_or_else(|| format!("CUSTOM {}", custom.name)),
            depfile: depfile.as_deref(),
            restat: edge.restat,
            console: edge.console,
            outputs: &as_paths(&outputs),
            inputs: &as_paths(&at.paths(custom.inputs.iter().chain(&custom.program))),
            after: &as_paths(&at.paths(&custom.depends)),
            exec,
            hashed: running.hashed(hashed),
            cache: custom.cache.as_deref(),
            pass_env: &running.pass_env,
            placement: rig::Placement { tags: running.worker_tags.clone(), ..Default::default() },
            ..Default::default()
        })?;
        // an alias, so that the outputs can be built by the command's name
        let alias = Path::new("custom").join(&custom.name);
        graph.alias(&alias, &as_paths(&outputs))?;
        everything.push(alias.clone());
        if *default {
            defaults.push(alias);
        }
    }

    // the entries of `compile_commands.json`, which clangd reads as rust-analyzer reads `.rust-project.json`
    let mut compile_commands = Vec::new();
    for library in project.natives() {
        let built = native_edges(&mut graph, project, crates, library, at, &mut compile_commands)?;
        everything.extend(built.iter().cloned());
        if library.default {
            defaults.extend(built);
        }
    }

    // A library built with `panic=abort` cannot be linked into test harnesses and procedural macros, which unwind, so
    // it is built a second time without it for them.
    let unwind = unwind_copies(crates, units);
    // the native libraries each crate links by its own `use`s, found once, as every crate which links it needs them
    let own_libraries: Vec<Vec<&Native>> = crates.iter().map(|c| project.link_libraries(&c.uses, &c.target)).collect();
    for (index, (c, unit)) in crates.iter().zip(units).enumerate() {
        // as the crate's own `change-detection` and `cache` say
        let (hashed, cache) = (unit.hashed, cache.filter(|_| unit.cached));
        // Returns the file of each dependency to pass with `--extern`: its copy which unwinds, if `unwinds` and it has
        // one; its metadata, if `metadata` and it is a library, so that this crate can start as soon as the metadata is
        // written; and otherwise its output.
        let externs = |unwinds: bool, metadata: bool| -> Vec<Extern> {
            let mut deps = Vec::new();
            for &index in &unit.deps {
                let d = &crates[index];
                let path = match () {
                    () if unwinds && unwind[index] => d.unwind_output(),
                    () if metadata && d.kind == Kind::Lib => d.metadata(),
                    () => d.output(),
                };
                deps.push(Extern { name: &d.name, path: at.path(&path) });
            }
            deps
        };
        let target = TargetFlag::new(&c.target, project, build_dir);
        let spec = at.paths(&target.spec);
        let input = at.path(&c.path);
        let codegen = codegen(c, project);
        // when a copy is linked, the crates the copies depend on are searched for among the copies
        let unwind_search = format!("dependency={}", at.path(&c.dir.join("unwind")).display());
        let copies = unit.deps.iter().any(|&index| unwind[index]);
        let flags_with = |deps: &[Extern], codegen: &[String], unwinds: bool| {
            let mut flags = rustc_flags(&target, &at.path(&c.dir), c.kind, deps, codegen, &input, at);
            if unwinds && copies {
                flags.plain(&["-L", &unwind_search]);
            }
            flags.cfg(&c.cfg);
            flags
        };
        let deps_of = |externs: &[Extern]| -> Vec<PathBuf> { externs.iter().map(|d| d.path.clone()).collect() };
        // a procedural macro unwinds, so it links the copies too
        let proc_macro = c.kind == Kind::ProcMacro;
        let externs_built = externs(proc_macro, c.kind == Kind::Lib);
        let flags = flags_with(&externs_built, &codegen, proc_macro);
        let built_deps = deps_of(&externs_built);
        let deps: Vec<_> = as_paths(&built_deps);
        let harness_externs = externs(true, false);
        let mut harness_flags = flags_with(&harness_externs, &harness_codegen(&codegen), true);
        // Only a crate which links, as a program or a test harness does, needs the libraries of the crates it uses
        // through others: a library is compiled against metadata alone. Finding them for every library would take
        // time quadratic in the length of a chain of crates.
        let needs_libraries = c.kind != Kind::Lib || c.test;
        let transitive = if needs_libraries { transitive_libraries(crates, units, index) } else { Vec::new() };
        let native = NativeLinks::new(&own_libraries, &transitive, index, at);
        let mut flags = flags;
        if c.kind == Kind::Lib {
            flags.extend(&native.recorded);
        } else {
            flags.extend(&native.linked);
        }
        let harness_deps = deps_of(&harness_externs);
        // The linker script of the crate, or failing that of its target. It is passed to rustc wherever it links, which
        // is for the test harness and for any crate but a library, and what links then depends on it.
        let link_script = c.link_script.as_ref().or_else(|| project.target(&c.target)?.link_script.as_ref());
        let link_script = link_script.map(|path| at.path(path));
        // Apple's linker writes the absolute paths of the object files into its output, as the map by which debug info
        // is found in them. `--remap-path-prefix` does not reach those, so `-oso_prefix .` makes them relative to the
        // linker's working directory, the build directory. It is passed in the form `cc`, the host's linker, takes.
        let host = project.host();
        let default_linker = !codegen.iter().any(|option| option.starts_with("linker="));
        let link = |flags: &Flags| {
            let mut linked_flags = flags.clone();
            if let Some(path) = &link_script {
                linked_flags.codegen(&format!("link-arg=-T{}", path.display()));
            }
            if c.target == host && host.contains("-apple-") && default_linker {
                linked_flags.codegen("link-arg=-Wl,-oso_prefix,.");
            }
            // On ELF, give a shared library for C a soname: what links it then records the soname, rather than the path
            // it
            // was given, which is relative to the build directory. On Apple's targets rustc gives it an install name
            // itself.
            if c.kind == Kind::CDylib && !c.target.contains("-apple-") && !c.target.contains("-windows") {
                linked_flags.codegen(&format!("link-arg=-Wl,-soname,lib{}.so", c.name));
            }
            linked_flags
        };
        let linked_flags = link(&flags);
        // Linking depends on the library of every crate used, directly or not: a library only waits for the metadata
        // of the crates it uses, so their libraries may not have been written when it finishes.
        let libraries = |unwinds: bool| -> Vec<PathBuf> {
            let path = |&index: &usize| {
                let d = &crates[index];
                at.path(&if unwinds && unwind[index] { d.unwind_output() } else { d.output() })
            };
            transitive.iter().map(path).collect()
        };
        let (libraries, harness_libraries) = (libraries(proc_macro), libraries(true));
        let link_deps: Vec<_> = deps.iter().copied().chain(as_paths(&libraries)).chain(as_paths(&native.archives)).collect();
        let harness_link_deps: Vec<_> = as_paths(&harness_deps)
            .into_iter()
            .chain(as_paths(&harness_libraries))
            .chain(as_paths(&native.archives))
            .collect();
        // What rustc reads: the crate's root, from which it finds the other sources, its target's specification, and
        // where it links, its linker script.
        let inputs: Vec<&Path> = [input.as_path()].into_iter().chain(as_paths(&spec)).collect();
        let linked_inputs: Vec<&Path> = inputs.iter().copied().chain(link_script.as_deref()).collect();
        let depends = at.paths(&c.depends);
        let crate_type = c.kind.crate_type();
        let output = at.path(&c.output());
        let tmpdir = Path::new("tmp").join(&c.target).join(&c.name);
        let linked = c.kind != Kind::Lib;
        let crate_flags = if linked { &linked_flags } else { &flags };
        let rust = Rust { crate_type, name: &c.name, input: &input, env: &c.env, pass_env: &c.pass_env, toolchain };
        // a library also writes its metadata, as an early output, which rustc reports to rig in JSON as it writes it
        let metadata = (c.kind == Kind::Lib).then(|| at.path(&c.metadata()));
        let mut leading = rustc_emit(&tmpdir, &output).to_vec();
        if let Some(path) = &metadata {
            // rustc takes no `--color` with JSON, and its rendered diagnostics are coloured by `--json` instead
            leading.retain(|arg| arg != "--color=always");
            leading.push(format!("--emit=metadata={}", path.display()));
            leading.extend(["--error-format=json", "--json=artifacts,diagnostic-rendered-ansi"].map(String::from));
        }
        // a static library for C also writes the list of system libraries it needs, which whatever links it reads
        let native_static_libs = (c.kind == Kind::StaticLib).then(|| at.path(&c.native_static_libs()));
        if let Some(path) = &native_static_libs {
            leading.push(format!("--print=native-static-libs={}", path.display()));
        }
        let mut exec = rust.exec("rustc", &leading, crate_flags, None);
        if linked {
            exec.env.extend(linker_env(&codegen, &c.target, project));
        }
        let placement = placement(unit, units, project, linked);
        let early: Vec<&Path> = metadata.iter().map(PathBuf::as_path).collect();
        let outputs: Vec<&Path> =
            [output.as_path()].into_iter().chain(early.iter().copied()).chain(native_static_libs.as_deref()).collect();
        graph.edge(Edge {
            description: describe("RUSTC", &output),
            outputs: &outputs,
            early: &early,
            inputs: if linked { &linked_inputs } else { &inputs },
            dependencies: if linked { &link_deps } else { &deps },
            after: &as_paths(&depends),
            depfile: Some(&depfile_of(&output)),
            discover: Some(discovery(&exec)),
            exec,
            hashed,
            pass_env: rust.pass_env,
            cache,
            placement: placement.clone(),
            ..Default::default()
        })?;
        everything.push(output.clone());
        if c.test {
            everything.push(at.path(&c.test_harness()));
            let tmpdir = Path::new("tmp").join(&c.target).join("test").join(&c.name);
            let mut test_flags = link(&harness_flags);
            test_flags.extend(&native.linked);
            test_flags.plain(&["--test"]);
            let harness = at.path(&c.test_harness());
            let mut exec = rust.exec("rustc", &rustc_emit(&tmpdir, &harness), &test_flags, None);
            exec.env.extend(linker_env(&codegen, &c.target, project));
            graph.edge(Edge {
                description: describe("RUSTC", &harness),
                outputs: &[&harness],
                inputs: &linked_inputs,
                dependencies: &harness_link_deps,
                after: &as_paths(&depends),
                depfile: Some(&depfile_of(&harness)),
                discover: Some(discovery(&exec)),
                exec,
                hashed,
                pass_env: rust.pass_env,
                cache,
                // the test harness links, as a binary does
                placement: self::placement(unit, units, project, true),
                ..Default::default()
            })?;
        }
        if unwind[index] {
            let tmpdir = Path::new("tmp").join(&c.target).join("unwind").join(&c.name);
            let copy = at.path(&c.unwind_output());
            harness_flags.extend(&native.recorded);
            let exec = rust.exec("rustc", &rustc_emit(&tmpdir, &copy), &harness_flags, None);
            graph.edge(Edge {
                description: describe("RUSTC", &copy),
                outputs: &[&copy],
                inputs: &inputs,
                dependencies: &as_paths(&harness_deps),
                after: &as_paths(&depends),
                depfile: Some(&depfile_of(&copy)),
                exec,
                hashed,
                pass_env: rust.pass_env,
                cache,
                ..Default::default()
            })?;
        }
        if c.default {
            defaults.push(output);
        }
    }

    // Each check is an ordinary edge which saves its diagnostics to a file. rust-analyzer only shows the diagnostics
    // of the latest flycheck, so when a check is up to date, the flycheck replays the saved ones.
    for unit in units {
        let (hashed, cache) = (unit.hashed, cache.filter(|_| unit.cached));
        // rustc runs a procedural macro even when checking, so its real output is passed rather than its metadata
        let mut deps = Vec::new();
        for &index in &unit.deps {
            let dep = &units[index];
            let path = match dep.kind {
                Kind::ProcMacro => &dep.output,
                _ => &dep.check_rmeta(),
            };
            deps.push(Extern { name: &dep.name, path: at.path(path) });
        }
        let target = TargetFlag::new(&unit.target, project, build_dir);
        let spec = at.paths(&target.spec);
        let outdir = at.path(&unit.check_dir());
        let root = at.path(&unit.root);
        // a crate with tests is checked or linted with `--test`, so it takes its test harness's codegen options
        let codegen = if unit.harness { harness_codegen(&unit.codegen) } else { unit.codegen.clone() };
        let mut flags = rustc_flags(&target, &outdir, unit.kind, &deps, &codegen, &root, at);
        flags.cfg(&unit.cfg);
        let (crate_type, name, env) = (unit.kind.crate_type(), &unit.name, &unit.env);
        let rust = Rust { crate_type, name, input: &root, env, pass_env: &unit.pass_env, toolchain };
        // a check writes metadata, rather than linking
        let placement = placement(unit, units, project, false);
        let deps: Vec<_> = deps.iter().map(|d| d.path.as_path()).collect();
        let inputs: Vec<&Path> = [root.as_path()].into_iter().chain(as_paths(&spec)).collect();
        let output = at.path(&unit.check_rmeta());
        // the crate is documented as it is built, not as its tests, against the metadata of the crates it uses
        if let Some(name) = &unit.doc {
            let index = at.path(&unit.doc_index());
            let dependencies: Vec<_> = deps.iter().copied().chain([output.as_path()]).collect();
            let doc_dir = at.path(&unit.doc_dir()).display().to_string();
            let leading = ["--color=always".to_string(), format!("--out-dir={doc_dir}")];
            let exec = rust.exec("rustdoc", &leading, &flags, None);
            // rustdoc only writes dep-info with unstable options. So the documentation depends on the crate's check
            // instead, which is rebuilt when the crate's sources change. The crates for a target are documented into
            // one directory, as rustdoc locks the files they share.
            graph.edge(Edge {
                description: describe("RUSTDOC", &index),
                outputs: &[&index],
                inputs: &inputs,
                dependencies: &dependencies,
                exec,
                hashed,
                pass_env: rust.pass_env,
                ..Default::default()
            })?;
            let alias = Path::new("doc").join(name);
            graph.alias(&alias, &[&index])?;
            everything.push(alias.clone());
            docs.push(alias);
        }
        if unit.test {
            flags.plain(&["--test"]);
        }
        // it is linted with the flags it is checked with, against the metadata from the checks of the crates it uses
        if let Some(name) = &unit.lint {
            let output = at.path(&unit.lint_rmeta());
            let diagnostics = at.path(&unit.lint_diagnostics());
            let outdir = at.path(&unit.lint_dir());
            // each flag is a level and a lint, such as `-W clippy::pedantic`, which are two arguments
            let mut lint_flags = flags.clone();
            for flag in &unit.lints {
                lint_flags.plain(&flag.split(' ').collect::<Vec<_>>());
            }
            if unit.harness && !unit.test {
                lint_flags.plain(&["--test"]);
            }
            let emit = metadata_emit(&outdir, &output, true);
            let mut exec = rust.exec("clippy-driver", &emit, &lint_flags, Some(&diagnostics));
            // clippy only suggests what the minimum supported Rust version has; it reads the version as cargo gives it
            if let Some(version) = project.rust_version() {
                exec.env.push(("CARGO_PKG_RUST_VERSION".into(), version));
            }
            // A lint is a check run by clippy. Its diagnostics are saved, so that `jig lint` can show them when the
            // lint
            // is up to date.
            graph.edge(Edge {
                description: describe("CLIPPY", &output),
                outputs: &[&output, &diagnostics],
                inputs: &inputs,
                dependencies: &deps,
                depfile: Some(&depfile_of(&output)),
                discover: Some(discovery(&exec)),
                exec,
                hashed,
                pass_env: rust.pass_env,
                cache,
                placement: placement.clone(),
                ..Default::default()
            })?;
            let alias = Path::new("lint").join(name);
            graph.alias(&alias, &[&output])?;
            lints.push(alias);
        }
        let diagnostics = at.path(&unit.diagnostics());
        let exec = rust.exec("rustc", &metadata_emit(&outdir, &output, false), &flags, Some(&diagnostics));
        graph.edge(Edge {
            description: describe("CHECK", &output),
            outputs: &[&output, &diagnostics],
            inputs: &inputs,
            dependencies: &deps,
            depfile: Some(&depfile_of(&output)),
            discover: Some(discovery(&exec)),
            exec,
            hashed,
            pass_env: rust.pass_env,
            cache,
            placement,
            ..Default::default()
        })?;
        graph.alias(&Path::new("check").join(&unit.label), &[&output])?;
    }

    // lints every crate which is edited
    graph.alias(Path::new("lint"), &as_paths(&lints))?;
    // builds the documentation of every crate with `doc`
    graph.alias(Path::new("doc"), &as_paths(&docs))?;
    // builds what all of the tests need
    graph.alias(Path::new("test"), &as_paths(&tests))?;
    // builds what `jig install` installs
    let installed = at.paths(installed.iter().map(|file| &file.path));
    graph.alias(Path::new("install"), &as_paths(&installed))?;

    // every target but the checks, if no crate or command is built by default
    let defaults = if defaults.is_empty() { everything } else { defaults };
    graph.default(&as_paths(&defaults));

    write_compile_commands(&at.build_dir, &compile_commands)?;
    graph.finish()
}

/// Returns the entry of `compile_commands.json` for a source, as clangd reads it: the absolute build directory,
/// where the compiler runs, the absolute path of the source, the object, and the program with its arguments.
fn compile_command(at: &Rebase, program: &str, args: &[String], source: &Path, object: &Path) -> String {
    use crate::rust_analyzer::string;
    let directory = at.build_dir.display().to_string();
    let file = normalize(&at.build_dir, source).display().to_string();
    let arguments: Vec<String> = std::iter::once(program).chain(args.iter().map(String::as_str)).map(string).collect();
    format!(
        "  {{\"directory\": {}, \"file\": {}, \"output\": {}, \"arguments\": [{}]}}",
        string(&directory),
        string(&file),
        string(&object.display().to_string()),
        arguments.join(", "),
    )
}

/// Write `compile_commands.json`, for the sources of C, C++ and assembly, to the build directory, where clangd looks
/// for it when the build directory is named `build`. If there are no such sources, any old file is removed.
fn write_compile_commands(build_dir: &Path, commands: &[String]) -> Result<(), Error> {
    let path = build_dir.join("compile_commands.json");
    if commands.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let contents = format!("[\n{}\n]\n", commands.join(",\n"));
    // only written when it changes, as clangd reloads it whenever it is written
    if std::fs::read_to_string(&path).is_ok_and(|written| written == contents) {
        return Ok(());
    }
    std::fs::write(&path, contents).map_err(|error| Error::Write(path, error))
}

/// The arguments of rustc which give a crate the static libraries it uses, and the archives it depends on.
struct NativeLinks {
    /// The arguments for a library crate, `-l static:-bundle=<name>`. They record in its metadata that whatever links
    /// it needs the libraries, without copying them into its rlib, so the crate does not depend on the archives.
    recorded: Vec<String>,
    /// The arguments for a crate which links. Its own libraries are given as `-l static=<name>`, or
    /// `-l dylib=<name>` for a shared library. The directory of each library which it, or a library crate it links,
    /// uses is given as `-L native=<dir>`. Each library has a directory of its own, so that a library is only found
    /// where it is used.
    linked: Vec<String>,
    /// The archives of the libraries used by a crate which links, whether directly or through the crates it links.
    archives: Vec<PathBuf>,
}
impl NativeLinks {
    /// `own_libraries` is what each crate links by its own `use`s, and `transitive` the library crates this one links.
    fn new(own_libraries: &[Vec<&Native>], transitive: &[usize], index: usize, at: &Rebase) -> Self {
        let own = &own_libraries[index];
        let link = |bundle: bool| -> Vec<String> {
            let mut args = Vec::new();
            for library in own {
                // A library linked as its objects is not recorded. rustc has no kind of native library for objects, so
                // they are passed with `link-arg` instead.
                if library.link_objects {
                    continue;
                }
                // a shared library is linked as itself, wherever it is used
                if library.kind == NativeKind::SharedLibrary {
                    args.extend(["-l".to_string(), format!("dylib={}", library.name)]);
                    continue;
                }
                let mut modifiers = Vec::new();
                if !bundle {
                    modifiers.push("-bundle");
                }
                if library.whole_archive {
                    modifiers.push("+whole-archive");
                }
                let kind = if modifiers.is_empty() { "static".to_string() } else { format!("static:{}", modifiers.join(",")) };
                args.extend(["-l".to_string(), format!("{kind}={}", library.name)]);
            }
            args
        };
        // add the libraries of each library crate which is linked, in the order of the crates, each library once
        let mut libraries: Vec<&Native> = own.clone();
        for &dep in transitive {
            for &library in &own_libraries[dep] {
                if !libraries.iter().any(|known| std::ptr::eq(*known, library)) {
                    libraries.push(library);
                }
            }
        }
        let mut linked = link(true);
        // rustc's linker does not link the C++ runtime, so name it if a static library being linked has C++ sources
        let cxx = libraries.iter().filter(|library| library.kind == NativeKind::StaticLibrary).find(|library| library.has_cxx());
        if let Some(cxx) = cxx {
            linked.extend(["-l".to_string(), format!("dylib={}", cxx.cxx_runtime)]);
        }
        for library in &libraries {
            // the objects of a library linked as its objects; rustc passes `link-arg`s to the linker after what it
            // links
            if library.link_objects {
                for object in library.linked_files() {
                    linked.extend(["-C".to_string(), format!("link-arg={}", at.path(&object).display())]);
                }
                continue;
            }
            linked.extend(["-L".to_string(), format!("native={}", at.path(&library.dir).display())]);
        }
        let archives = libraries.iter().flat_map(|library| library.linked_files()).map(|path| at.path(&path)).collect();
        Self { recorded: link(false), linked, archives }
    }
}

/// Add the edges which compile each source of a library or an executable to an object, and then archive or link
/// the objects. Returns what is built, rebased: the archive, the objects of a static library which is linked as its
/// objects, or the linked output.
fn native_edges(
    graph: &mut RigGraph,
    project: &Project,
    crates: &[Crate],
    library: &Native,
    at: &Rebase,
    compile_commands: &mut Vec<String>,
) -> Result<Vec<PathBuf>, Error> {
    // its include directories: its own, then the public ones of the libraries it uses, directly or not
    let mut include: Vec<PathBuf> = library.include.iter().chain(&library.public_include).cloned().collect();
    for used in project.reachable_libraries(&library.uses, &library.target) {
        include.extend(used.public_include.iter().cloned());
    }
    let mut seen = std::collections::HashSet::new();
    include.retain(|dir| seen.insert(dir.clone()));
    // Make the paths the compiler writes, in debug info and `__FILE__`, relative to the project root, as rustc's
    // are: the build directory, where it runs, maps to the root, and the path from there to the root is removed.
    let mut remap = vec![format!("-ffile-prefix-map={}=.", at.build_dir.display())];
    let up = at.path(&at.root);
    if up != Path::new(".") {
        remap.push(format!("-ffile-prefix-map={}/=", up.display()));
    }
    let program = |tool: &[String]| -> String {
        if is_path(&tool[0]) { at.path(Path::new(&tool[0])).display().to_string() } else { tool[0].clone() }
    };
    let depends = at.paths(&library.depends);
    let mut objects = Vec::new();
    for source in &library.sources {
        let object = at.path(&library.object(&source.path));
        let input = at.path(&source.path);
        let depfile = depfile_of(&object);
        // C++ has its own compiler, which has its own identity in the cache
        let (compiler, cache, language_flags, action) = match source.language {
            crate::project::Language::C => (&library.compiler, &library.cache, &library.c_flags, "CC"),
            crate::project::Language::Assembly => (&library.compiler, &library.cache, &library.asm_flags, "AS"),
            crate::project::Language::Cxx => (&library.cxx, &library.cxx_cache, &library.cxx_flags, "CXX"),
        };
        let mut args: Vec<String> = compiler[1..].to_vec();
        if library.position_independent() {
            args.push("-fPIC".into());
        }
        args.extend(language_flags.iter().cloned());
        args.extend(library.flags.iter().cloned());
        for dir in &include {
            args.push(format!("-I{}", at.path(dir).display()));
        }
        for define in &library.defines {
            args.push(format!("-D{define}"));
        }
        args.extend(remap.iter().cloned());
        args.extend(["-MD".to_string(), "-MF".to_string(), depfile.display().to_string()]);
        args.extend(["-c".to_string(), input.display().to_string(), "-o".to_string(), object.display().to_string()]);
        compile_commands.push(compile_command(at, &program(compiler), &args, &input, &object));
        graph.edge(Edge {
            description: describe(action, &object),
            outputs: &[&object],
            inputs: &[&input],
            after: &as_paths(&depends),
            depfile: Some(&depfile),
            exec: program_in_build_dir(&program(compiler), args, library.running.env.clone()),
            // stamped by contents by default, so that a header which is touched but not changed rebuilds nothing
            hashed: library.running.hashed(true),
            // Cached by the identity of the compiler, as rustc's edges are by the toolchain's. That is sound because
            // the depfile lists every header which was read.
            cache: cache.as_deref(),
            pass_env: &library.running.pass_env,
            placement: native_placement(library),
            ..Default::default()
        })?;
        objects.push(object);
    }
    if library.kind.is_linked() {
        return link_native(graph, project, crates, library, &objects, at).map(|output| vec![output]);
    }
    // a static library linked as its objects is not archived, unless it is installed
    if library.link_objects && !library.install.enabled {
        return Ok(objects);
    }
    let archive = at.path(&library.output());
    let mut args: Vec<String> = library.archiver[1..].to_vec();
    args.extend([ARCHIVE_FLAGS.to_string(), archive.display().to_string()]);
    args.extend(objects.iter().map(|object| object.display().to_string()));
    graph.edge(Edge {
        description: describe("AR", &archive),
        outputs: &[&archive],
        inputs: &as_paths(&objects),
        exec: program_in_build_dir(&program(&library.archiver), args, archive_env(&library.running.env)),
        hashed: library.running.hashed(true),
        pass_env: &library.running.pass_env,
        placement: native_placement(library),
        ..Default::default()
    })?;
    Ok(vec![archive])
}

/// Add the edge which links an executable or a shared library from its objects and the libraries it uses, with the
/// compiler, returning its output, rebased.
///
/// The arguments are, in order:
///
/// - the objects;
/// - each library, as the path of its archive, its shared library, or on Windows its import library. A library
///   comes before those it uses, as a linker which reads each archive once needs. A static library which is
///   `whole-archive` is wrapped in the flags which link every object of it: `-force_load` on Apple's targets, and
///   `--whole-archive` elsewhere;
/// - the link flags;
/// - the directories of `rpath`;
/// - the linker script, of the executable or failing that of its target;
/// - for a shared library, what names it: its soname, `lib<name>.so`, by which whatever links it records and loads
///   it; on Apple's targets its install name, `@rpath/lib<name>.dylib`; or on Windows its import library.
fn link_native(
    graph: &mut RigGraph,
    project: &Project,
    crates: &[Crate],
    native: &Native,
    objects: &[PathBuf],
    at: &Rebase,
) -> Result<PathBuf, Error> {
    let output = at.path(&native.output());
    let libraries = project.link_libraries(&native.uses, &native.target);
    // the files of each library, which are its objects if it is linked as them
    let linked: Vec<Vec<PathBuf>> = libraries
        .iter()
        .map(|library| library.linked_files().iter().map(|path| at.path(path)).collect())
        .collect();
    let link_script = native.link_script.as_ref().or_else(|| project.target(&native.target)?.link_script.as_ref());
    let link_script = link_script.map(|path| at.path(path));
    let apple = native.target.contains("-apple-");
    let windows = native.target.contains("-windows");
    let mut args: Vec<String> = native.compiler[1..].to_vec();
    let mut outputs = vec![output.clone()];
    if native.kind == NativeKind::SharedLibrary {
        args.push(if apple { "-dynamiclib" } else { "-shared" }.into());
    }
    args.extend(objects.iter().map(|object| object.display().to_string()));
    for (library, paths) in libraries.iter().zip(&linked) {
        if library.link_objects {
            args.extend(paths.iter().map(|path| path.display().to_string()));
            continue;
        }
        let path = paths[0].display().to_string();
        match () {
            () if !library.whole_archive || library.kind == NativeKind::SharedLibrary => args.push(path),
            () if apple => args.push(format!("-Wl,-force_load,{path}")),
            () => args.extend(["-Wl,--whole-archive".to_string(), path, "-Wl,--no-whole-archive".to_string()]),
        }
    }
    // Rust's libraries for C, each after what uses it; a static one is followed by the system libraries it needs
    let mut rust_inputs = Vec::new();
    let for_c = project.linked_crates(native).into_iter().filter_map(|name| {
        crates.iter().find(|c| c.name == name && c.target == native.target && c.kind.is_for_c())
    });
    for c in for_c {
        let output = at.path(&c.output());
        args.push(output.display().to_string());
        rust_inputs.push(output);
        if c.kind == Kind::StaticLib {
            let libs = at.path(&c.native_static_libs());
            args.push(format!("@{}", libs.display()));
            rust_inputs.push(libs);
        }
    }
    // The C compiler links here, and does not link the C++ runtime, so name it, after what needs it, if this or a
    // static library it links has C++ sources.
    let static_cxx = libraries.iter().filter(|library| library.kind == NativeKind::StaticLibrary).find(|library| library.has_cxx());
    if let Some(runtime) = native.has_cxx().then_some(native).or(static_cxx.copied()).map(|cxx| &cxx.cxx_runtime) {
        args.push(format!("-l{runtime}"));
    }
    // these come last, as `-l` only resolves what is needed by something before it
    args.extend(native.link_flags.iter().cloned());
    for dir in &native.rpath {
        args.push(format!("-Wl,-rpath,{dir}"));
    }
    if let Some(path) = &link_script {
        args.push(format!("-T{}", path.display()));
    }
    if native.kind == NativeKind::SharedLibrary {
        let file_name = native.output().file_name().unwrap().to_string_lossy().into_owned();
        match () {
            () if windows => {
                let import = at.path(&native.link_output());
                args.push(format!("-Wl,--out-implib,{}", import.display()));
                outputs.push(import);
            },
            () if apple => args.extend(["-install_name".to_string(), format!("@rpath/{file_name}")]),
            () => args.push(format!("-Wl,-soname,{file_name}")),
        }
    }
    args.extend(["-o".to_string(), output.display().to_string()]);
    let inputs: Vec<&Path> = as_paths(objects)
        .into_iter()
        .chain(linked.iter().flatten().map(PathBuf::as_path))
        .chain(as_paths(&rust_inputs))
        .chain(link_script.as_deref())
        .collect();
    let program = if is_path(&native.compiler[0]) {
        at.path(Path::new(&native.compiler[0])).display().to_string()
    } else {
        native.compiler[0].clone()
    };
    graph.edge(Edge {
        description: describe("LINK", &output),
        outputs: &as_paths(&outputs),
        inputs: &inputs,
        after: &as_paths(&at.paths(&native.depends)),
        exec: program_in_build_dir(&program, args, native.running.env.clone()),
        hashed: native.running.hashed(true),
        // cached by the identity of the compiler, which does the linking as it did the compiling
        cache: native.cache.as_deref(),
        pass_env: &native.running.pass_env,
        placement: native_placement(native),
        ..Default::default()
    })?;
    Ok(output)
}

/// The flags of the archiver: create the archive, add the objects to it, replacing any of the same name, and write
/// its index. On all but Apple's hosts, `D` also zeroes the time, owner and mode of each object, as llvm-ar does by
/// default, so that the archive depends only on the objects' contents. Apple's `ar` does not take `D`, but does the
/// same when `ZERO_AR_DATE` is set, which [`archive_env`] does.
const ARCHIVE_FLAGS: &str = if cfg!(target_vendor = "apple") { "crs" } else { "crsD" };

/// Returns the environment of the archiver, which on Apple's hosts makes `ar` deterministic, followed by the
/// library's own.
fn archive_env(own: &[(String, String)]) -> Vec<(String, String)> {
    let apple = cfg!(target_vendor = "apple").then(|| ("ZERO_AR_DATE".to_string(), "1".to_string()));
    apple.into_iter().chain(own.iter().cloned()).collect()
}

/// Returns which workers rig may send the commands of a library or executable of C to: those with its `worker-tag`s,
/// and its host, as the compiler is found on it.
fn native_placement(native: &Native) -> rig::Placement {
    rig::Placement { tags: native.running.worker_tags.clone(), ..Default::default() }
}

/// Remove the archive of each static library whose list of objects has changed since it was archived. The list is
/// recorded beside the archive, in `lib<name>.a.members`. `ar` adds to an archive which already exists, so without
/// this, an archive would keep the object of a source which was removed.
fn forget_stale_archives(project: &Project) -> Result<(), Error> {
    let archived = |native: &&Native| {
        native.kind == NativeKind::StaticLibrary && (!native.link_objects || native.install.enabled)
    };
    for library in project.natives().into_iter().filter(archived) {
        let archive = library.output();
        let mut record = archive.clone().into_os_string();
        record.push(".members");
        let record = PathBuf::from(record);
        let mut members = String::new();
        for source in &library.sources {
            members.push_str(&library.object(&source.path).display().to_string());
            members.push('\n');
        }
        if std::fs::read_to_string(&record).is_ok_and(|recorded| recorded == members) {
            continue;
        }
        let _ = std::fs::remove_file(&archive);
        std::fs::create_dir_all(&library.dir).map_err(|error| Error::Create(library.dir.clone(), error))?;
        std::fs::write(&record, members).map_err(|error| Error::Create(record.clone(), error))?;
    }
    Ok(())
}

/// Returns what is shown while an edge which writes `output` runs, as `RUSTC <output>`.
fn describe(action: &str, output: &Path) -> String {
    format!("{action} {}", rig_graph::path_string(output))
}

/// Returns the depfile of an edge which writes `output`, beside it.
fn depfile_of(output: &Path) -> PathBuf {
    let mut depfile = output.as_os_str().to_owned();
    depfile.push(".d");
    depfile.into()
}

/// Returns paths borrowed as the `Path`s of an edge.
fn as_paths(paths: &[PathBuf]) -> Vec<&Path> {
    paths.iter().map(PathBuf::as_path).collect()
}

fn write_rust_project(sysroot: &str, units: &[Unit]) -> Result<(), Error> {
    let path = Path::new(".rust-project.json");
    let mut rust_project = Vec::new();
    let configure = std::env::current_exe().unwrap_or_else(|_| "jig".into());
    // writing to a buffer cannot fail
    let _ = rust_analyzer::write(&mut rust_project, Path::new(sysroot), &configure, units);
    if let Err(error) = std::fs::write(path, rust_project) {
        return Err(Error::Write(path.into(), error));
    }
    Ok(())
}

fn write_database(
    project: &Project,
    units: &[Unit],
    installed: &[Installed],
    input: &Path,
    at: &Rebase,
    build_dir: &Path,
    rustc: &crate::rustc::Answers,
) -> Result<(), Error> {
    let input = input.display().to_string();
    let build_dir = build_dir.display().to_string();
    let install_prefix = project.install_prefix();
    let settings = &project.cache_settings;
    let cache_dir = settings.dir.as_ref().map(|dir| dir.display().to_string()).unwrap_or_default();
    let cache_size = settings.size.to_string();
    let token_file = settings.token_file.as_ref().map(|path| path.display().to_string()).unwrap_or_default();
    let proxy_build = project.deferred_severity("proxy-build", Severity::Warning).to_string();
    let other_jig = project.deferred_severity("other-jig", Severity::Warning).to_string();
    // the `jig` which regenerates the graph, as its generator edge runs it
    let jig = std::env::current_exe().map(|jig| jig.display().to_string()).unwrap_or_default();
    // the database borrows the paths and commands
    let diagnostics: Vec<_> = units.iter().map(|unit| unit.diagnostics().display().to_string()).collect();
    let lint_diagnostics: Vec<_> = units.iter().map(|unit| unit.lint_diagnostics().display().to_string()).collect();

    // the binary crates, by the name of the crate with its target, as a test is named
    let host = project.host();
    let mut programs = Vec::new();
    for unit in units.iter().filter(|unit| unit.kind == Kind::Bin) {
        let name = if unit.target == host { unit.name.clone() } else { format!("{}@{}", unit.name, unit.target) };
        let runner = project.target(&unit.target).and_then(|target| target.runner);
        let runner = runner.map(|runner| project.runners[runner].command.clone()).unwrap_or_default();
        // the path as the graph has it, which is the target to build
        let (path, target) = (unit.output.display().to_string(), rig_graph::path_string(&at.path(&unit.output)));
        programs.push((name, path, target, runner, unit.default));
    }
    // and the executables of C, as they are
    for native in project.natives().into_iter().filter(|native| native.kind == NativeKind::Executable) {
        let runner = project.target(&native.target).and_then(|target| target.runner);
        let runner = runner.map(|runner| project.runners[runner].command.clone()).unwrap_or_default();
        let output = native.output();
        let (path, target) = (output.display().to_string(), rig_graph::path_string(&at.path(&output)));
        programs.push((native.short_name(&host), path, target, runner, native.default));
    }

    // the directories of the shared libraries, where programs run by `jig run` and `jig test` find them
    let shared = project.natives().into_iter().filter(|native| native.kind == NativeKind::SharedLibrary);
    let mut library_path: Vec<String> = shared.map(|native| native.dir.display().to_string()).collect();
    // and those of Rust's shared libraries for C, which are the directories of their targets
    for unit in units.iter().filter(|unit| unit.kind == Kind::CDylib) {
        let dir = unit.dir.display().to_string();
        if !library_path.contains(&dir) {
            library_path.push(dir);
        }
    }
    let test_tags = project.test_tags();
    let install_paths: Vec<_> = installed.iter().map(|file| file.path.display().to_string()).collect();
    let install_targets = installed.iter().map(|file| rig_graph::path_string(&at.path(&file.path)));
    let install_targets: Vec<_> = install_targets.collect();
    let mut database = database::Database {
        input: &input,
        toolchain: project.toolchain.as_deref().unwrap_or_default(),
        build_dir: &build_dir,
        test_tags: test_tags.iter().map(String::as_str).collect(),
        install_prefix: &install_prefix,
        proxy_build: &proxy_build,
        other_jig: &other_jig,
        jig: &jig,
        cache_dir: &cache_dir,
        cache_size: &cache_size,
        remote_cache: settings.remote.as_deref().unwrap_or_default(),
        push: settings.push,
        token_file: &token_file,
        workers: settings.workers.iter().map(String::as_str).collect(),
        workers_only: settings.workers_only,
        download_targets: settings.download_targets,
        toolchain_identity: settings.release.as_ref().map_or("", |(identity, _)| identity),
        toolchain_release: settings.release.as_ref().map_or("", |(_, release)| &release.identity),
        toolchain_install: settings.release.as_ref().map_or("", |(_, release)| &release.install),
        rustc_key: &rustc.key,
        rustc_sysroot: &rustc.sysroot,
        rustc_host: &rustc.host,
        rustc_targets: rustc.targets.iter().map(String::as_str).collect(),
        rustc_verbose_version: &rustc.verbose_version,
        library_path: library_path.iter().map(String::as_str).collect(),
        ..Default::default()
    };
    for (unit, diagnostics) in units.iter().zip(&diagnostics) {
        let deps = unit.deps.iter().map(|&index| index as u32).collect();
        let (label, member) = (&unit.label, unit.member);
        database.flychecks.push(database::Flycheck { label, diagnostics, member, deps });
    }
    for (unit, diagnostics) in units.iter().zip(&lint_diagnostics) {
        if let Some(name) = &unit.lint {
            database.lints.push(database::Lint { name, diagnostics });
        }
    }
    for custom in project.customs.iter().filter(|custom| custom.is_test()) {
        let tags = custom.tags.iter().map(String::as_str).collect();
        database.tests.push(database::Test { name: &custom.name, libtest: custom.is_libtest(), tags });
    }
    for (name, path, target, runner, default) in &programs {
        let runner = runner.iter().map(String::as_str).collect();
        database.programs.push(database::Program { name, path, target, runner, default: *default });
    }
    for ((file, path), target) in installed.iter().zip(&install_paths).zip(&install_targets) {
        let (dir, mode, program) = (&file.dir, file.mode, file.program);
        database.installs.push(database::Install { path, target, dir, mode, program });
    }
    let mut bytes = Vec::new();
    let _ = database::write(&mut bytes, &database);
    let path = Path::new(database::PATH);
    if let Err(error) = std::fs::write(path, bytes) {
        return Err(Error::Write(path.into(), error));
    }
    Ok(())
}

/// Returns the flags for compiling a crate of a kind with the flag for its target, the crates passed to it and its
/// codegen options.
///
/// `search` is the directory the outputs for the target share, where rustc finds the dependencies of the `--extern`
/// crates. A linker given as a path is relative to the project root, and is rebased like the other paths.
fn rustc_flags(
    target: &TargetFlag,
    search: &Path,
    kind: Kind,
    deps: &[Extern],
    codegen: &[String],
    root_module: &Path,
    at: &Rebase,
) -> Flags {
    let mut flags = Flags::default();
    flags.extend(&at.remap(root_module));
    match &target.spec {
        Some(spec) => flags.plain(&[&format!("--target={}", at.path(spec).display()), "-Zunstable-options"]),
        None => flags.plain(&[&format!("--target={}", target.target)]),
    }
    flags.plain(&["-L", &format!("dependency={}", search.display())]);
    for option in codegen {
        let option = match option.strip_prefix("linker=") {
            Some(linker) if is_path(linker) => format!("linker={}", at.path(Path::new(linker)).display()),
            _ => option.clone(),
        };
        flags.codegen(&option);
    }
    // the toolchain's `proc_macro` crate is not in the extern prelude otherwise
    if kind == Kind::ProcMacro {
        flags.plain(&["--extern", "proc_macro"]);
    }
    for dep in deps {
        flags.plain(&["--extern", &format!("{}={}", dep.name, dep.path.display())]);
    }
    flags
}

/// Returns whether each crate is built a second time without `panic=abort`, as `<target>/unwind/lib<name>.rlib`.
///
/// Test harnesses and procedural macros unwind, and a library built with `panic=abort` cannot be linked into them.
/// So such a library is copied. A library which depends on a copied one is copied too: `panic` changes the hash of a
/// crate, so a crate built against the original cannot use the copy. Only libraries which a test harness or a
/// procedural macro actually links are copied.
fn unwind_copies(crates: &[Crate], units: &[Unit]) -> Vec<bool> {
    let aborts = library_aborts(crates, units);
    let mut copied = vec![false; crates.len()];
    let needs_copy = |index: &&usize| aborts[**index];
    let mut stack: Vec<usize> = Vec::new();
    for (c, unit) in crates.iter().zip(units) {
        if c.test || c.kind == Kind::ProcMacro {
            stack.extend(unit.deps.iter().filter(needs_copy));
        }
    }
    while let Some(index) = stack.pop() {
        if !copied[index] {
            copied[index] = true;
            stack.extend(units[index].deps.iter().filter(needs_copy));
        }
    }
    copied
}

/// Returns the indices of the library crates a crate uses, directly or through other libraries, each once, in
/// ascending order. Procedural macros are left out, with what only they use, as they are not linked.
fn transitive_libraries(crates: &[Crate], units: &[Unit], index: usize) -> Vec<usize> {
    let mut found = Vec::new();
    let mut seen = vec![false; crates.len()];
    let mut stack: Vec<usize> = units[index].deps.clone();
    while let Some(dep) = stack.pop() {
        if seen[dep] || crates[dep].kind != Kind::Lib {
            continue;
        }
        seen[dep] = true;
        found.push(dep);
        stack.extend(units[dep].deps.iter().copied());
    }
    found.sort_unstable();
    found
}

/// Returns whether each crate is a library built with `panic=abort`, or which uses one which is.
fn library_aborts(crates: &[Crate], units: &[Unit]) -> Vec<bool> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unvisited,
        Visiting,
        Done,
    }
    let mut state = vec![State::Unvisited; crates.len()];
    let mut aborts = vec![false; crates.len()];
    // depth first, so that each crate is decided after the crates it uses, which may be declared after it
    let mut stack = Vec::new();
    for root in 0..crates.len() {
        stack.push((root, false));
        while let Some((index, used)) = stack.pop() {
            if used {
                let c = &crates[index];
                // a crate still being visited is reached through a cycle, and is not counted as aborting here
                aborts[index] = c.kind == Kind::Lib
                    && (c.codegen.iter().any(|option| option == "panic=abort")
                        || units[index].deps.iter().any(|&d| state[d] == State::Done && aborts[d]));
                state[index] = State::Done;
            } else if state[index] == State::Unvisited {
                state[index] = State::Visiting;
                stack.push((index, true));
                stack.extend(units[index].deps.iter().filter(|&&d| state[d] == State::Unvisited).map(|&d| (d, false)));
            }
        }
    }
    aborts
}

/// Returns the codegen options of a crate for its test harness: the same, without `panic`. On stable rustc the
/// harness must unwind, as it catches the panic of each test.
fn harness_codegen(codegen: &[String]) -> Vec<String> {
    codegen.iter().filter(|option| !option.starts_with("panic=")).cloned().collect()
}

/// The flags of rustc, rustdoc or clippy, as arguments. rig runs the program directly, so they need no quoting.
#[derive(Clone, Default)]
struct Flags {
    args: Vec<String>,
}
impl Flags {
    fn plain(&mut self, args: &[&str]) {
        self.args.extend(args.iter().map(|arg| (*arg).to_string()));
    }
    fn extend(&mut self, args: &[String]) {
        self.args.extend_from_slice(args);
    }
    /// Add a codegen option, `-C <option>=<value>`.
    fn codegen(&mut self, option: &str) {
        self.args.extend(["-C".to_string(), option.to_string()]);
    }
    /// Add the `--cfg` flags of configuration options.
    fn cfg(&mut self, cfg: &[String]) {
        for option in cfg {
            self.args.extend(["--cfg".to_string(), option.clone()]);
        }
    }
}

/// What is the same for each program which compiles, checks or documents a crate.
struct Rust<'a> {
    crate_type: &'a str,
    name: &'a str,
    /// The root of the crate, rebased.
    input: &'a Path,
    /// The environment variables of the crate.
    env: &'a [(String, String)],
    /// The environment variables it inherits.
    pass_env: &'a [String],
    toolchain: Option<&'a str>,
}
impl Rust<'_> {
    /// Returns the command which runs `program` (rustc, rustdoc or clippy) for the crate. Its arguments are `leading`,
    /// then the crate's flags, then its root. stderr is written to `stderr`, if that is given.
    fn exec(&self, program: &str, leading: &[String], flags: &Flags, stderr: Option<&Path>) -> rig::Exec {
        let mut args = leading.to_vec();
        args.extend([format!("--crate-type={}", self.crate_type), format!("--crate-name={}", self.name)]);
        args.push("--edition=2024".into());
        args.extend(flags.args.iter().cloned());
        args.push(self.input.display().to_string());
        let mut env = toolchain_env(self.toolchain);
        env.extend(self.env.iter().cloned());
        let mut exec = program_in_build_dir(program, args, env);
        exec.stderr = stderr.map(|path| path.display().to_string());
        exec
    }
}

/// Returns a variant of a rustc or clippy command which only writes its dep-info, without compiling the crate. rig
/// uses it to find the sources of a crate which has not been built, in order to send the crate to a worker.
fn discovery(exec: &rig::Exec) -> rig::Exec {
    let emits = |arg: &&String| {
        (arg.starts_with("--emit=") && !arg.starts_with("--emit=dep-info=")) || arg.starts_with("--print=")
    };
    let args = exec.args.iter().filter(|arg| !emits(arg)).cloned().collect();
    rig::Exec { args, stderr: None, ..exec.clone() }
}

/// Returns the arguments of rustc which compile a crate to `output`, with its dep-info beside it.
fn rustc_emit(tmpdir: &Path, output: &Path) -> [String; 4] {
    [
        "--color=always".into(),
        format!("--out-dir={}", tmpdir.display()),
        format!("--emit=link={}", output.display()),
        format!("--emit=dep-info={}.d", output.display()),
    ]
}

/// Returns the arguments which make rustc or clippy check a crate, writing its metadata to `output`. Diagnostics
/// are JSON, unless the crate is being linted, when they are coloured text, as clippy shows them.
fn metadata_emit(outdir: &Path, output: &Path, lint: bool) -> [String; 4] {
    [
        if lint { "--color=always".into() } else { "--error-format=json".into() },
        format!("--out-dir={}", outdir.display()),
        format!("--emit=metadata={}", output.display()),
        format!("--emit=dep-info={}.d", output.display()),
    ]
}

/// Returns the first output of the edge which runs a test, which is no file, as the test writes none.
pub fn test_run(name: &str) -> String {
    format!("test-run/{name}")
}

/// Returns the environment which selects the toolchain, if there is one.
fn toolchain_env(toolchain: Option<&str>) -> Vec<(String, String)> {
    toolchain.map(|toolchain| ("RUSTUP_TOOLCHAIN".to_string(), toolchain.to_string())).into_iter().collect()
}

/// Returns a program run from `dir`, or from the build directory if `dir` is `None`.
fn program(program: &str, args: Vec<String>, dir: Option<&Path>, env: Vec<(String, String)>) -> rig::Exec {
    let dir = dir.map(|dir| dir.display().to_string());
    rig::Exec { program: program.into(), args, env, dir, stderr: None }
}

/// Returns a program run from the build directory, as rustc is, whose paths are relative to it.
fn program_in_build_dir(name: &str, args: Vec<String>, env: Vec<(String, String)>) -> rig::Exec {
    program(name, args, None, env)
}

/// Returns which workers rig may send a crate's commands to, beside those which have its exact toolchain.
///
/// - A worker must have the tags the crate gives.
/// - If the crate's target is builtin, a worker must have the standard library for it.
/// - A worker on another host, with the same release of the toolchain, may run the commands unless what they write
///   depends on the host.
///
/// What a crate writes depends on the host if any of these holds: it is a procedural macro, which is linked for the
/// host; it uses a procedural macro, which rustc loads, and which only loads on the host it was built on; or it is
/// linked, as `linked` says, by the host's linker rather than by `rust-lld`, which rustc has on every host.
fn placement(unit: &Unit, units: &[Unit], project: &Project, linked: bool) -> rig::Placement {
    let uses_proc_macro = unit.deps.iter().any(|&dep| units[dep].kind == Kind::ProcMacro);
    let lld = unit.codegen.iter().filter_map(|option| option.strip_prefix("linker=")).any(|linker| {
        Path::new(linker).file_name().is_some_and(|name| name == "rust-lld")
    });
    let any_host = unit.kind != Kind::ProcMacro && !uses_proc_macro && (!linked || lld);
    let builtin = project.target(&unit.target).is_some_and(|target| !target.is_custom());
    let target = builtin.then(|| unit.target.clone());
    rig::Placement { any_host, target, tags: unit.worker_tags.clone(), runs: None }
}

/// The environment variable through which a command which links is given its linker's identity. rustc does not read
/// it: it is there to be part of the command, and so of its cache key, so that the result of one linker is never
/// fetched for another.
const LINKER_VARIABLE: &str = "JIG_LINKER";

/// Whether a program is given as a path, relative to the file which gives it, rather than as a name to find. It is a
/// path if it has a separator, which on Windows may be `\` as well as `/`.
fn is_path(program: &str) -> bool {
    program.contains('/') || (cfg!(windows) && program.contains('\\'))
}

/// Returns the name of the linker a crate is linked by: the one its codegen options give, or the default for its
/// target. Returns `None` for `rust-lld`, which is part of the toolchain, and so already part of its identity.
fn linker_of(codegen: &[String], target: &str) -> Option<String> {
    let given = codegen.iter().rev().find_map(|option| option.strip_prefix("linker="));
    let linker = given.unwrap_or(if target.contains("-msvc") { "link.exe" } else { "cc" });
    let lld = Path::new(linker).file_stem().is_some_and(|name| name == "rust-lld");
    (!lld).then(|| linker.to_string())
}

/// Find the identity of the linker of each crate which links, as a program or a test harness. The identity is made
/// from the linker's file and what it prints as its version. The configuration is made to depend on the file, so
/// that the project is configured again when the linker changes.
fn identify_linkers(project: &mut Project, crates: &[Crate]) {
    for c in crates.iter().filter(|c| c.kind != Kind::Lib || c.test) {
        let Some(linker) = linker_of(&codegen(c, project), &c.target) else {
            continue;
        };
        if project.linkers.contains_key(&linker) {
            continue;
        }
        let path = if is_path(&linker) {
            Some(PathBuf::from(&linker))
        } else {
            // searched for here, without recording `PATH` as read, which would reconfigure the project whenever it
            // changes
            let search = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&search).find_map(|dir| crate::platform::program_in(&dir, &linker))
        };
        let identity = match path.and_then(|path| rig::canonicalize(&path).ok()) {
            Some(path) => {
                project.depend(path.clone());
                let version = std::process::Command::new(&path).arg("--version").output();
                let version = version.map(|output| [output.stdout, output.stderr].concat()).unwrap_or_default();
                let mut key = rig::cache::Key::new("linker");
                key.part(path.display().to_string().as_bytes()).part(&version);
                rig::cache::hex(&key.finish())[..16].to_string()
            },
            None => "missing".into(),
        };
        project.linkers.insert(linker, identity);
    }
}

/// Returns the environment variable which gives a linking command its linker's identity, if the linker is not part
/// of the toolchain.
fn linker_env(codegen: &[String], target: &str, project: &Project) -> Option<(String, String)> {
    let linker = linker_of(codegen, target)?;
    let identity = project.linkers.get(&linker)?;
    Some((LINKER_VARIABLE.to_string(), format!("{linker} {identity}")))
}

/// Returns the codegen options of a crate, followed by the linker options of its target. The target's are left out
/// if the crate sets its own linker or linker flavor, as the two go together.
fn codegen(c: &Crate, project: &Project) -> Vec<String> {
    let mut codegen = c.codegen.clone();
    let own_linker = c.codegen.iter().any(|option| option.starts_with("linker=") || option.starts_with("linker-flavor="));
    if !own_linker && let Some(target) = project.target(&c.target) {
        codegen.extend(target.codegen());
    }
    codegen
}

/// Returns, for each crate, the indices in `crates` of the crates passed to it with `--extern`, in the order of
/// `crates`.
///
/// Builtin crates are passed to every other crate for the same target, but not to each other as that would be a cycle,
/// so a builtin crate must `use` the others it depends on.
///
/// The crates are looked up by name, and the builtin crates by target. Comparing every crate with every other
/// would take time quadratic in their number.
fn externs(crates: &[Crate]) -> Vec<Vec<usize>> {
    let mut named: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut builtins: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, d) in crates.iter().enumerate() {
        named.entry(&d.name).or_default().push(index);
        if d.builtin {
            builtins.entry(&d.target).or_default().push(index);
        }
    }
    let mut all = Vec::new();
    for c in crates {
        let mut externs = Vec::new();
        if !c.builtin {
            externs.extend(builtins.get(c.target.as_str()).into_iter().flatten());
        }
        for used in &c.uses {
            for &index in named.get(used.name.as_str()).into_iter().flatten() {
                let d = &crates[index];
                // a procedural macro is for the host target, but can be used by a crate for any target
                if (d.target == c.target || d.kind == Kind::ProcMacro) && matches!(d.kind, Kind::Lib | Kind::ProcMacro) {
                    externs.push(index);
                }
            }
        }
        externs.sort_unstable();
        externs.dedup();
        all.push(externs);
    }
    all
}

/// Returns the specification of a builtin target, `base`, for a custom target to inherit. Printing it is unstable, so
/// this needs a nightly toolchain.
///
/// It fails, with what rustc reported, if rustc fails or prints no specification. Both are checked, as rustc does
/// not always exit unsuccessfully.
fn target_spec_json(target: &str, base: &str, toolchain: Option<&str>) -> Result<String, Error> {
    let target_flag = format!("--target={base}");
    let mut cmd = crate::rustc::command(&["-Zunstable-options", "--print=target-spec-json", &target_flag], toolchain);
    let output = match cmd.output() {
        Ok(output) => output,
        Err(error) => return Err(Error::Exec(Box::new(cmd), error)),
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim_end().to_string();
    match String::from_utf8(output.stdout) {
        Ok(json) if output.status.success() && !json.trim().is_empty() => Ok(json),
        _ => Err(Error::InheritedSpec {
            target: target.into(),
            base: base.into(),
            nightly: stderr.contains("only accepted on the nightly compiler"),
            stderr,
        }),
    }
}

#[cfg(test)]
mod benches {
    use std::fmt::Write as _;

    use super::*;
    use crate::bench::{assert_linear, configure};

    /// The crates of a project, in the order they are generated.
    fn crates(source: &str) -> Vec<Crate> {
        let mut crates: Vec<_> = configure(source).crates.into_values().collect();
        crates.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
        crates
    }

    #[test]
    fn externs_of_a_chain() {
        // short enough that the crates of the longer chain fit in a core's cache, as the time to look up each crate
        // grows once they do not, which made a longer chain take more than ten times as long for four times its length
        // on some machines, though the work is linear
        let chain = |n: usize| {
            let mut source = String::from("crate c0 'c.rs'\n");
            for i in 1..n {
                let _ = writeln!(source, "crate c{i} 'c.rs'\n    use c{}", i - 1);
            }
            crates(&source)
        };
        assert_linear("externs-chain", 100, chain, |crates| {
            std::hint::black_box(externs(crates));
        });
    }

    #[test]
    fn externs_with_builtins() {
        // like the crates of the standard library, which are passed to every other crate
        let with_builtins = |n: usize| {
            let mut source = String::from("crate core 'c.rs'\n    builtin\ncrate alloc 'c.rs'\n    builtin\n    use core\n");
            for i in 0..n {
                let _ = writeln!(source, "crate c{i} 'c.rs'");
            }
            crates(&source)
        };
        assert_linear("externs-builtins", 2000, with_builtins, |crates| {
            std::hint::black_box(externs(crates));
        });
    }

    #[test]
    #[cfg(unix)]
    fn normalizing_through_links() {
        // a symbolic link before each `..` is resolved, which reads the link
        let dir = std::env::temp_dir().join(format!("jig-bench-normalize-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real/inner")).unwrap();
        let _ = std::os::unix::fs::symlink("real/inner", dir.join("link"));
        let path = |n: usize| dir.join("link/../inner/../".repeat(n)).join("Jig");
        assert_linear("normalize", 50, path, |path| {
            std::hint::black_box(normalize(Path::new(""), path));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{HOST, configure};

    /// Returns the placement of each crate's command which builds it, by its name, with whether it links.
    fn placements(source: &str) -> HashMap<String, rig::Placement> {
        let mut project = configure(source);
        assert!(!project.fatal);
        let mut crates: Vec<_> = std::mem::take(&mut project.crates).into_values().collect();
        crates.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
        let units = units(&project, &crates, "/sysroot", Path::new("build"));
        let placed = |unit: &Unit| (unit.name.clone(), placement(unit, &units, &project, unit.kind != Kind::Lib));
        units.iter().map(placed).collect()
    }

    #[test]
    fn places_what_does_not_depend_on_the_host_anywhere() {
        let placed = placements(
            "crate lib 'lib.rs'\n    worker-tag 'fast'\ncrate derive 'derive.rs'\n    crate-type 'proc-macro'\n\
             crate derived 'derived.rs'\n    use derive\ncrate app 'app.rs'\n    crate-type 'bin'\n    use lib\n\
             set codegen/linker 'rust-lld'\ncrate linked 'linked.rs'\n    crate-type 'bin'\n",
        );
        let any_host = |name: &str| placed[name].any_host;
        // a library may be built on any host, but for one which is or uses a procedural macro, or one which is linked
        // by the host's linker rather than rust-lld
        assert!(any_host("lib") && any_host("linked"));
        assert!(!any_host("derive") && !any_host("derived") && !any_host("app"));
        assert_eq!(placed["lib"].tags, ["fast"]);
        assert_eq!(placed["app"].tags, Vec::<String>::new());
        // each needs the standard library of its target, which is builtin
        assert_eq!(placed["app"].target.as_deref(), Some(HOST));
    }
}
