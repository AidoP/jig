//! Generation of the files which build a project: the specifications of its custom targets, `build.ninja`, its depfile
//! and rig's graph in the build directory, and `.rust-project.json` and the database, read by `jig build`, `jig check`,
//! `jig test` and `jig install`, at the project root.
//!
//! Ninja runs in the build directory, so the paths in `build.ninja` are relative to it, while the project's paths are
//! relative to the project root.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::graph::{Edge, Graph, Rule, Tee};
use crate::ninja::{self, NinjaFile};
use crate::project::rust::{Crate, Kind};
use crate::project::{CustomKind, GeneratedFile, Installed, Project, Severity, Target, TargetKind, has_line_break};
use crate::rig_graph::{self, RigGraph};
use crate::{Error, database, rust_analyzer, target_spec, toolchain};

/// Write the files generated from a project, which has been read and checked.
///
/// `input` is the root build file.
pub fn write(mut project: Project, input: &Path, sysroot: &str) -> Result<(), Error> {
    let build_dir = project.build_dir();
    if let Err(error) = std::fs::create_dir_all(&build_dir) {
        return Err(Error::Create(build_dir, error));
    }
    let at = Rebase::new(&build_dir);
    write_reconfigure_stamp(&build_dir)?;
    write_target_specs(&project.targets, &build_dir, project.toolchain.as_deref())?;
    write_generated_files(&project.generated)?;
    write_depfile(&project, &at, &build_dir)?;

    let installed = project.installed();
    let mut crates: Vec<_> = std::mem::take(&mut project.crates).into_values().collect();
    crates.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
    let units = units(&project, &crates, sysroot, &build_dir);
    // `build.ninja` and rig's graph are written from the same description, `build.ninja` first, as rig records the
    // edge which generates it once the graph is written
    let mut ninja_file = NinjaFile::new(build_dir.join(ninja::FILE))?;
    let mut rig_graph = RigGraph::new(build_dir.join(rig_graph::FILE));
    write_graph(&mut Tee(&mut ninja_file, &mut rig_graph), &project, &crates, &units, &installed, input, &at)?;
    write_rust_project(sysroot, &units)?;
    retire_build_dir(&build_dir)?;
    write_database(&project, &units, &installed, input, &at, &build_dir)
}

/// Rebases the paths of a project, relative to its root, for ninja, which runs in the build directory.
///
/// Every path is relative to the build directory, so that what rustc, which also runs there, is given and writes does
/// not depend on where the project is: a path within the build directory is relative to it, and any other leads out of
/// it with `..`, from where the build directory is once symbolic links are followed, so that it leads to the same file
/// when the build directory is a link. rustc writes the paths of sources in its dep-info as it is given them, which
/// ninja finds, while what it writes elsewhere, such as in diagnostics and debug info, is remapped to be relative to the
/// project root, by [`Rebase::remap`].
struct Rebase {
    /// The project root, which is absolute.
    root: PathBuf,
    /// The build directory, which is absolute.
    build_dir: PathBuf,
    /// The build directory once symbolic links are followed, which `..` leaves.
    physical: PathBuf,
}
impl Rebase {
    fn new(build_dir: &Path) -> Self {
        let root = normalize(&std::env::current_dir().unwrap_or_default());
        let build_dir = normalize(&root.join(build_dir));
        let physical = std::fs::canonicalize(&build_dir).unwrap_or_else(|_| build_dir.clone());
        Self { root, build_dir, physical }
    }
    fn path(&self, path: &Path) -> PathBuf {
        let path = normalize(&self.root.join(path));
        match path.strip_prefix(&self.build_dir) {
            Ok(relative) if relative.as_os_str().is_empty() => PathBuf::from("."),
            Ok(relative) => relative.to_path_buf(),
            Err(_) => relative_to(&self.physical, &path),
        }
    }
    /// Returns the arguments of rustc, rustdoc and clippy which remap the paths they write, other than in dep-info, to be
    /// relative to the project root, rather than the build directory, as its sources are, given the root module of the
    /// crate, rebased.
    ///
    /// - The build directory, which rustc writes as the working directory of debug info, is the project root, `.`.
    /// - The path from the build directory to the root is removed from the paths of sources outside the build directory.
    /// - A root module in the build directory, as one which is generated, has the build directory before it, by the first
    ///   component of its path, which its modules share.
    ///
    /// rustc takes the last of the prefixes which match a path.
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

/// Returns a path relative to a directory, both of which are absolute and normalized, with a `..` for each component of
/// the directory which the path does not share, or `.` if they are the same.
fn relative_to(dir: &Path, path: &Path) -> PathBuf {
    let (dir, path): (Vec<_>, Vec<_>) = (dir.components().collect(), path.components().collect());
    let shared = dir.iter().zip(&path).take_while(|(lhs, rhs)| lhs == rhs).count();
    let mut relative: PathBuf = std::iter::repeat_n(std::path::Component::ParentDir, dir.len() - shared).collect();
    relative.extend(&path[shared..]);
    if relative.as_os_str().is_empty() { PathBuf::from(".") } else { relative }
}

/// Returns a path with its `.` and `..` components resolved, for ninja.
///
/// Ninja removes a `..` with the component before it, without following symbolic links, but a `..` after a symbolic
/// link leaves the directory the link points to, rather than the one containing the link. So a symbolic link followed
/// by `..` is resolved first, and the path is otherwise left as it is, including the paths which do not exist yet.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                if normal.symlink_metadata().is_ok_and(|metadata| metadata.is_symlink())
                    && let Ok(target) = std::fs::canonicalize(&normal)
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
    /// The directory of the outputs for the target.
    pub dir: PathBuf,
    /// The name of the crate's documentation, `doc/<name>`, if it is documented.
    pub doc: Option<String>,
    /// Whether the crate is built by `ninja` without targets, which `jig run` prefers.
    pub default: bool,
    /// The name of the crate's lint, `lint/<name>`, if it is edited, so that it is linted.
    pub lint: Option<String>,
    /// The flags which give clippy's lints their levels.
    pub lints: Vec<String>,
    /// Whether the crate has a test harness, as which it is linted, so that its tests are, while a library is checked
    /// without them, as the crates which use it use its metadata.
    pub harness: bool,
}
impl Unit {
    fn check_dir(&self) -> PathBuf {
        self.dir.join("check")
    }
    fn check_rmeta(&self) -> PathBuf {
        self.check_dir().join(format!("lib{}.rmeta", self.name))
    }
    /// The directory of the documentation of the crates for the target, where each crate's is in a directory named
    /// after it, which rustdoc writes their shared files beside.
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
        // every crate for the target depends on its specification, so it is only written when it changes
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

/// Write the files generated by `file` statements, each only when it changes, so that what uses it is not built again
/// otherwise, and give each its permissions, which are set again if they have been changed.
fn write_generated_files(files: &[GeneratedFile]) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
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
        let current = std::fs::metadata(&file.output).map(|metadata| metadata.permissions().mode() & 0o7777);
        if current.is_ok_and(|mode| mode == file.mode) {
            continue;
        }
        if let Err(error) = std::fs::set_permissions(&file.output, std::fs::Permissions::from_mode(file.mode)) {
            return Err(Error::Write(file.output.clone(), error));
        }
    }
    Ok(())
}

/// Returns the hash of the value of an environment variable as the database records it, in hexadecimal.
pub fn env_hash(hash: u64) -> String {
    format!("{hash:016x}")
}

/// The file in the build directory which `build.ninja` depends on, which is touched to regenerate it, such as when a path
/// the configuration found was missing is created.
pub const RECONFIGURE_STAMP: &str = "reconfigure.stamp";

/// Create the stamp which regenerates `build.ninja` when it is touched, unless it exists, as it must not be newer than
/// `build.ninja`.
fn write_reconfigure_stamp(build_dir: &Path) -> Result<(), Error> {
    let path = build_dir.join(RECONFIGURE_STAMP);
    match std::fs::File::create_new(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => Err(Error::Create(path, error)),
        _ => Ok(()),
    }
}

/// Write the depfile of `build.ninja`, which is regenerated when a file the configuration read changes, or the
/// reconfigure stamp is touched.
fn write_depfile(project: &Project, at: &Rebase, build_dir: &Path) -> Result<(), Error> {
    let path = build_dir.join(format!("{}.d", ninja::FILE));
    // a line break cannot be escaped in a depfile, as in `build.ninja`
    let files = project.files.iter().map(|file| &file.path);
    if let Some(broken) = files.chain(&project.dependencies).find(|path| has_line_break(&path.to_string_lossy())) {
        return Err(Error::LineBreak(path, broken.display().to_string()));
    }
    let mut file = match std::fs::File::create(&path) {
        Ok(file) => file,
        Err(error) => return Err(Error::Create(path, error)),
    };
    if let Err(error) = write_dependencies(&mut file, project, at, &build_dir.join(RECONFIGURE_STAMP)) {
        return Err(Error::Write(path, error));
    }
    Ok(())
}

fn write_dependencies(w: &mut dyn std::io::Write, project: &Project, at: &Rebase, stamp: &Path) -> Result<(), std::io::Error> {
    write!(w, "{}:", ninja::FILE)?;
    let stamp = stamp.to_path_buf();
    // builtin files change with jig, which the configuration already depends on, and a file processed again is listed
    // once
    let mut seen = std::collections::HashSet::new();
    let files = project.files.iter().filter(|file| !file.builtin && seen.insert(&file.path)).map(|file| &file.path);
    // as are the files generated by `file` statements, so that one which is removed is written again
    let generated = project.generated.iter().map(|file| &file.output);
    let paths = files.chain(&project.dependencies).chain(generated).chain([&stamp]);
    for path in at.paths(paths) {
        // escape the characters which are special in a depfile
        let path = path.display().to_string().replace(' ', "\\ ").replace('#', "\\#").replace('$', "$$");
        write!(w, " {path}")?;
    }
    writeln!(w)?;
    Ok(())
}

/// Returns the crates as units for rust-analyzer, in the same order.
fn units(project: &Project, crates: &[Crate], sysroot: &str, build_dir: &Path) -> Vec<Unit> {
    let host = project.host();
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
            test: c.test && !matches!(c.kind, Kind::Lib | Kind::ProcMacro),
            kind: c.kind,
            output: c.output(),
            deps,
            member,
            build_file: project.files[c.name_span.file].path.clone(),
            codegen: codegen(c, project),
            cfg: c.cfg.clone(),
            env: c.env.clone(),
            pass_env: c.pass_env.clone(),
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
    graph: &mut dyn Graph,
    project: &Project,
    crates: &[Crate],
    units: &[Unit],
    installed: &[Installed],
    input: &Path,
    at: &Rebase,
) -> Result<(), Error> {
    // the specifications of custom targets are found in the build directory, and rebased
    let build_dir = &at.build_dir;
    // whether rig stamps the files of an edge by their contents
    let hashed = match project.change_detection().as_str() {
        "timestamp" => false,
        "hash" => true,
        other => return Err(Error::ChangeDetection(other.into())),
    };
    // rig caches the results of rustc and clippy, by the identity of the toolchain, when it caches, but not of rustdoc,
    // which writes a directory
    let cache = project.cache.as_deref();
    // the configure tool and custom commands run from the project root, as their arguments are relative to it
    let cd = format!("cd {} &&", ninja_quote(&at.root.display().to_string()));

    // `build.ninja` regenerates itself with the configure tool which generated it, and again when the tool changes, so
    // a project which builds the tool, such as its own, has ninja build it first
    let exe = std::env::current_exe().ok();
    let configure = exe.clone().unwrap_or_else(|| "jig".into());
    // with the toolchain it was configured with
    let toolchain = project.toolchain.as_deref();
    let plus = toolchain.map(|toolchain| format!(" {}", ninja_quote(&format!("+{toolchain}")))).unwrap_or_default();
    let command = format!(
        "{cd} {}{plus} generate {}",
        ninja_quote(&configure.display().to_string()),
        ninja_quote(&input.display().to_string()),
    );
    // rustc and custom commands run with the toolchain, even when ninja is run directly
    let env = toolchain::shell_prefix(toolchain).replace('$', "$$");
    graph.rule(&Rule {
        name: "configure",
        command: &command,
        description: "CONFIGURE $out",
        console: true,
        depfile: Some("$out.d"),
        generator: true,
    })?;
    let exe = at.paths(&exe);
    let mut args: Vec<String> = toolchain.map(|toolchain| format!("+{toolchain}")).into_iter().collect();
    args.extend(["generate".to_string(), input.display().to_string()]);
    let exec = program(&configure.display().to_string(), args, Some(&at.root), Vec::new());
    graph.edge(&Edge {
        rule: "configure",
        outputs: &[Path::new(ninja::FILE)],
        inputs: &[&at.path(input)],
        implicit: &as_paths(&exe),
        exec: Some(&exec),
        hashed,
        ..Default::default()
    })?;

    graph.variable("edition", "2024")?;
    // the temporary files rustc writes to `--out-dir` are kept in the build directory, in a directory for each edge, as
    // they are named after the crate, which may be compiled for several targets, or as a test harness, at once
    // `crate-env` is the environment variables of a crate, before rustc, as `sh` takes them
    let rustc = &format!("{env}$crate-env rustc --color=always --out-dir=$tmpdir --emit=link=$out --emit=dep-info=$depfile --crate-type=$crate-type --crate-name=$crate-name --edition=$edition $flags $in");
    graph.rule(&Rule {
        name: "rustc",
        command: rustc,
        description: "RUSTC $out",
        depfile: Some("$out.d"),
        ..Default::default()
    })?;

    // rustdoc only writes dep-info with unstable options, so the documentation depends on the crate's check instead,
    // which is built again when its sources change, and the crates for a target are documented together, as rustdoc
    // locks the files they share
    graph.rule(&Rule {
        name: "rustdoc",
        command: &format!("{env}$crate-env rustdoc --color=always --out-dir=$outdir --crate-type=$crate-type --crate-name=$crate-name --edition=$edition $flags $in"),
        description: "RUSTDOC $out",
        ..Default::default()
    })?;

    // a lint is a check by clippy, whose diagnostics are saved, so that `jig lint` shows them when it is up to date
    graph.rule(&Rule {
        name: "clippy",
        command: &format!("{env}$crate-env clippy-driver --color=always --out-dir=$outdir --emit=metadata=$out --emit=dep-info=$out.d --crate-type=$crate-type --crate-name=$crate-name --edition=$edition $flags $in 2> $diagnostics"),
        description: "CLIPPY $out",
        depfile: Some("$out.d"),
        ..Default::default()
    })?;

    graph.rule(&Rule {
        name: "rustc-check",
        command: &format!("{env}$crate-env rustc --error-format=json --out-dir=$outdir --emit=metadata=$out --emit=dep-info=$out.d --crate-type=$crate-type --crate-name=$crate-name --edition=$edition $flags $in 2> $diagnostics"),
        description: "CHECK $out",
        depfile: Some("$out.d"),
        ..Default::default()
    })?;

    // the targets built by `ninja` without any, which are all targets if there are none
    let mut defaults: Vec<PathBuf> = Vec::new();
    // what is built without defaults, which is all but the checks, which are for rust-analyzer
    let mut everything: Vec<PathBuf> = Vec::new();
    // the documentation of the crates
    let mut docs: Vec<PathBuf> = Vec::new();
    // the lints of the crates, which are not built without targets, like the checks
    let mut lints: Vec<PathBuf> = Vec::new();

    // the command of an edge is `command-line`, as its `command` would replace the rule's
    graph.rule(&Rule {
        name: "custom",
        command: &format!("{cd} {env}$command-line"),
        description: "CUSTOM $name",
        ..Default::default()
    })?;
    let mut tests = Vec::new();
    for custom in &project.customs {
        // a test is run by `jig test`, which builds its inputs and program with this target first
        let CustomKind::Command { outputs, default, .. } = &custom.kind else {
            let phony = Path::new("test").join(&custom.name);
            let inputs = custom.inputs(&project.runners);
            let inputs = at.paths(inputs.iter().chain(&custom.program).chain(&custom.depends));
            graph.alias(&phony, &as_paths(&inputs))?;
            tests.push(phony);
            continue;
        };
        // ninja runs commands with `sh`, which must not split or expand the arguments
        let command: Vec<_> = custom.command.iter().map(|argument| shell_quote(argument)).collect();
        let command = command.join(" ").replace('$', "$$");
        let outputs = at.paths(outputs);
        // from the project root, as a path relative to the build directory, so that the action does not depend on where
        // the project is, as for the key of the cache
        let root = at.path(&at.root);
        let exec = program(&custom.command[0], custom.command[1..].to_vec(), Some(&root), toolchain_env(toolchain));
        graph.edge(&Edge {
            rule: "custom",
            outputs: &as_paths(&outputs),
            inputs: &as_paths(&at.paths(&custom.inputs)),
            implicit: &as_paths(&at.paths(&custom.program)),
            order_only: &as_paths(&at.paths(&custom.depends)),
            variables: &[("command-line", &command), ("name", &custom.name)],
            exec: Some(&exec),
            hashed: custom.change_detection.as_deref().map_or(hashed, |detection| detection == "hash"),
            cache: custom.cache.as_deref(),
            pass_env: &custom.pass_env,
            ..Default::default()
        })?;
        // so that the outputs can be built by name
        let phony = Path::new("custom").join(&custom.name);
        graph.alias(&phony, &as_paths(&outputs))?;
        everything.push(phony.clone());
        if *default {
            defaults.push(phony);
        }
    }

    for (c, unit) in crates.iter().zip(units) {
        // a dependency is a library, so it has an output
        let mut deps = Vec::new();
        for &index in &unit.deps {
            let d = &crates[index];
            deps.push(Extern { name: &d.name, path: at.path(&d.output()) });
        }
        let target = TargetFlag::new(&c.target, project, build_dir);
        let spec = at.paths(&target.spec);
        let input = at.path(&c.path);
        let mut flags = rustc_flags(&target, &at.path(&c.dir), c.kind, &deps, &codegen(c, project), &input, at);
        flags.cfg(&c.cfg);
        let crate_env = crate_env(&c.env);
        let deps: Vec<_> = deps.iter().map(|d| d.path.as_path()).chain(as_paths(&spec)).collect();
        // the linker script of the crate, or else of its target, which is passed to rustc where it links, for the
        // test harness and any crate but a library, which then depends on it
        let link_script = c.link_script.as_ref().or_else(|| project.target(&c.target)?.link_script.as_ref());
        let link_script = link_script.map(|path| at.path(path));
        let mut linked_flags = flags.clone();
        if let Some(path) = &link_script {
            linked_flags.codegen(&format!("link-arg=-T{}", path.display()));
        }
        // Apple's linker writes the absolute paths of the object files into what it links, as the map by which debug
        // info is found in them, which `--remap-path-prefix` does not reach, so they are made relative to its working
        // directory, the build directory, by `-oso_prefix .`, as `cc`, the linker of the host, takes it
        let host = project.host();
        let default_linker = !codegen(c, project).iter().any(|option| option.starts_with("linker="));
        if c.target == host && host.contains("-apple-") && default_linker {
            linked_flags.codegen("link-arg=-Wl,-oso_prefix,.");
        }
        let link_deps: Vec<_> = deps.iter().copied().chain(link_script.as_deref()).collect();
        let depends = at.paths(&c.depends);
        let crate_type = c.kind.crate_type();
        let output = at.path(&c.output());
        let tmpdir = Path::new("tmp").join(&c.target).join(&c.name);
        let linked = c.kind != Kind::Lib;
        let crate_flags = if linked { &linked_flags } else { &flags };
        let rust = Rust { crate_type, name: &c.name, input: &input, env: &c.env, pass_env: &c.pass_env, toolchain };
        let exec = rust.exec("rustc", &rustc_emit(&tmpdir, &output), crate_flags, None);
        graph.edge(&Edge {
            rule: "rustc",
            outputs: &[&output],
            inputs: &[&input],
            implicit: if linked { &link_deps } else { &deps },
            order_only: &as_paths(&depends),
            variables: &[
                ("crate-name", &c.name),
                ("crate-type", crate_type),
                ("flags", crate_flags.ninja()),
                ("tmpdir", &tmpdir.display().to_string()),
                ("crate-env", &crate_env),
            ],
            exec: Some(&exec),
            hashed,
            pass_env: rust.pass_env,
            cache,
            ..Default::default()
        })?;
        everything.push(output.clone());
        if c.test {
            everything.push(at.path(&c.test_harness()));
            let tmpdir = Path::new("tmp").join(&c.target).join("test").join(&c.name);
            let mut test_flags = linked_flags.clone();
            test_flags.plain(&["--test"]);
            let harness = at.path(&c.test_harness());
            let exec = rust.exec("rustc", &rustc_emit(&tmpdir, &harness), &test_flags, None);
            graph.edge(&Edge {
                rule: "rustc",
                outputs: &[&harness],
                inputs: &[&input],
                implicit: &link_deps,
                order_only: &as_paths(&depends),
                variables: &[
                    ("crate-name", &c.name),
                    ("crate-type", crate_type),
                    ("flags", test_flags.ninja()),
                    ("tmpdir", &tmpdir.display().to_string()),
                    ("crate-env", &crate_env),
                ],
                exec: Some(&exec),
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

    // each check is an ordinary edge which saves its diagnostics, so that a flycheck can replay them when it is up to
    // date, as rust-analyzer only shows the diagnostics from the latest flycheck
    for unit in units {
        // a procedural macro is run by rustc, even when checking, so its own output is used rather than its metadata
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
        let mut flags = rustc_flags(&target, &outdir, unit.kind, &deps, &unit.codegen, &root, at);
        flags.cfg(&unit.cfg);
        let crate_env = crate_env(&unit.env);
        let (crate_type, name, env) = (unit.kind.crate_type(), &unit.name, &unit.env);
        let rust = Rust { crate_type, name, input: &root, env, pass_env: &unit.pass_env, toolchain };
        let deps: Vec<_> = deps.iter().map(|d| d.path.as_path()).chain(as_paths(&spec)).collect();
        let output = at.path(&unit.check_rmeta());
        // the documentation is of the crate as it is built, rather than its tests, with the metadata of the crates it
        // uses
        if let Some(name) = &unit.doc {
            let index = at.path(&unit.doc_index());
            let implicit: Vec<_> = deps.iter().copied().chain([output.as_path()]).collect();
            let doc_dir = at.path(&unit.doc_dir()).display().to_string();
            let leading = ["--color=always".to_string(), format!("--out-dir={doc_dir}")];
            let exec = rust.exec("rustdoc", &leading, &flags, None);
            graph.edge(&Edge {
                rule: "rustdoc",
                outputs: &[&index],
                inputs: &[&root],
                implicit: &implicit,
                variables: &[
                    ("crate-name", &unit.name),
                    ("crate-type", unit.kind.crate_type()),
                    ("flags", flags.ninja()),
                    ("outdir", &doc_dir),
                    ("crate-env", &crate_env),
                ],
                exec: Some(&exec),
                hashed,
                pass_env: rust.pass_env,
                ..Default::default()
            })?;
            let phony = Path::new("doc").join(name);
            graph.alias(&phony, &[&index])?;
            everything.push(phony.clone());
            docs.push(phony);
        }
        if unit.test {
            flags.plain(&["--test"]);
        }
        // linted as it is checked, with the metadata of the crates it uses from their checks
        if let Some(name) = &unit.lint {
            let output = at.path(&unit.lint_rmeta());
            let diagnostics = at.path(&unit.lint_diagnostics());
            let outdir = at.path(&unit.lint_dir());
            // each flag is a level and a lint, such as `-W clippy::pedantic`, which are separate arguments
            let mut lint_flags = flags.clone();
            for flag in &unit.lints {
                lint_flags.plain(&flag.split(' ').collect::<Vec<_>>());
            }
            if unit.harness && !unit.test {
                lint_flags.plain(&["--test"]);
            }
            let emit = metadata_emit(&outdir, &output, true);
            let exec = rust.exec("clippy-driver", &emit, &lint_flags, Some(&diagnostics));
            graph.edge(&Edge {
                rule: "clippy",
                outputs: &[&output],
                implicit_outputs: &[&diagnostics],
                inputs: &[&root],
                implicit: &deps,
                variables: &[
                    ("crate-name", &unit.name),
                    ("crate-type", unit.kind.crate_type()),
                    ("flags", lint_flags.ninja()),
                    ("outdir", &outdir.display().to_string()),
                    ("diagnostics", &diagnostics.display().to_string()),
                    ("crate-env", &crate_env),
                ],
                exec: Some(&exec),
                hashed,
                pass_env: rust.pass_env,
                cache,
                ..Default::default()
            })?;
            let phony = Path::new("lint").join(name);
            graph.alias(&phony, &[&output])?;
            lints.push(phony);
        }
        let diagnostics = at.path(&unit.diagnostics());
        let exec = rust.exec("rustc", &metadata_emit(&outdir, &output, false), &flags, Some(&diagnostics));
        graph.edge(&Edge {
            rule: "rustc-check",
            outputs: &[&output],
            implicit_outputs: &[&diagnostics],
            inputs: &[&root],
            implicit: &deps,
            variables: &[
                ("crate-name", &unit.name),
                ("crate-type", unit.kind.crate_type()),
                ("flags", flags.ninja()),
                ("outdir", &outdir.display().to_string()),
                ("diagnostics", &diagnostics.display().to_string()),
                ("crate-env", &crate_env),
            ],
            exec: Some(&exec),
            hashed,
            pass_env: rust.pass_env,
            cache,
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

    // without a `default` statement, ninja would build every target, including the checks
    let defaults = if defaults.is_empty() { everything } else { defaults };
    if !defaults.is_empty() {
        graph.default(&as_paths(&defaults))?;
    }

    graph.finish()
}

/// Returns paths borrowed as the `Path`s of a ninja statement.
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

/// Replace `build.ninja` in the previous build directory, from the database, if `build-dir` has changed, with a stub
/// which ninja finds is up to date, so that ninja running there, which is regenerating it, stops.
///
/// `jig` then finds the new build directory from the database. The rest of the previous build directory is kept.
fn retire_build_dir(build_dir: &Path) -> Result<(), Error> {
    let Ok(bytes) = std::fs::read(database::PATH) else {
        return Ok(());
    };
    let Ok(previous) = database::read(&bytes) else {
        return Ok(());
    };
    let previous = Path::new(previous.build_dir);
    // the paths are relative to the project root, which is the working directory, unless they are absolute
    let absolute = |path: &Path| normalize(&std::path::absolute(path).unwrap_or_else(|_| path.into()));
    if absolute(previous) == absolute(build_dir) {
        return Ok(());
    }
    ninja::retire(previous, build_dir)
}

/// The command of a test, which the database borrows.
struct TestCommand<'a> {
    name: &'a str,
    command: Vec<String>,
    libtest: bool,
    tags: &'a [String],
}

fn write_database(
    project: &Project,
    units: &[Unit],
    installed: &[Installed],
    input: &Path,
    at: &Rebase,
    build_dir: &Path,
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
    // the `jig` which regenerates `build.ninja`, as its configure rule runs it
    let jig = std::env::current_exe().map(|jig| jig.display().to_string()).unwrap_or_default();
    let missing: Vec<_> = project.missing.iter().map(|path| path.display().to_string()).collect();
    // the database borrows the paths and commands
    let diagnostics: Vec<_> = units.iter().map(|unit| unit.diagnostics().display().to_string()).collect();
    let lint_diagnostics: Vec<_> = units.iter().map(|unit| unit.lint_diagnostics().display().to_string()).collect();
    let mut commands = Vec::new();
    for custom in &project.customs {
        if custom.is_test() {
            let command = custom.command(&project.runners);
            let (name, libtest, tags) = (&custom.name, custom.is_libtest(), &custom.tags);
            commands.push(TestCommand { name, command, libtest, tags });
        }
    }

    // the binary crates, by the name of the crate with its target, as a test is named
    let host = project.host();
    let mut programs = Vec::new();
    for unit in units.iter().filter(|unit| unit.kind == Kind::Bin) {
        let name = if unit.target == host { unit.name.clone() } else { format!("{}@{}", unit.name, unit.target) };
        let runner = project.target(&unit.target).and_then(|target| target.runner);
        let runner = runner.map(|runner| project.runners[runner].command.clone()).unwrap_or_default();
        let (path, target) = (unit.output.display().to_string(), at.path(&unit.output).display().to_string());
        programs.push((name, path, target, runner, unit.default));
    }

    let missing = missing.iter().map(String::as_str).collect();
    let test_tags = project.test_tags();
    let env_hashes: Vec<String> = project.env.iter().map(|(_, hash)| hash.map(env_hash).unwrap_or_default()).collect();
    let install_paths: Vec<_> = installed.iter().map(|file| file.path.display().to_string()).collect();
    let install_targets: Vec<_> = installed.iter().map(|file| at.path(&file.path).display().to_string()).collect();
    let mut database = database::Database {
        input: &input,
        toolchain: project.toolchain.as_deref().unwrap_or_default(),
        build_dir: &build_dir,
        missing,
        env: project.env.iter().map(|(name, _)| name.as_str()).collect(),
        env_hashes: env_hashes.iter().map(String::as_str).collect(),
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
    for test in &commands {
        let command = test.command.iter().map(String::as_str).collect();
        let tags = test.tags.iter().map(String::as_str).collect();
        database.tests.push(database::Test { name: test.name, command, libtest: test.libtest, tags });
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
/// The outputs for a target share a directory, `search`, for the dependencies of `--extern` crates. A linker given as a
/// path, which is relative to the project root, is rebased like the other paths.
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
    flags.quoted(&at.remap(root_module));
    match &target.spec {
        Some(spec) => flags.plain(&[&format!("--target={}", at.path(spec).display()), "-Zunstable-options"]),
        None => flags.plain(&[&format!("--target={}", target.target)]),
    }
    flags.plain(&["-L", &format!("dependency={}", search.display())]);
    for option in codegen {
        let option = match option.strip_prefix("linker=") {
            Some(linker) if linker.contains('/') => format!("linker={}", at.path(Path::new(linker)).display()),
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

/// The flags of rustc, rustdoc or clippy, both as the arguments of the program, which rig runs directly, and as they are
/// written to `build.ninja`, whose commands `sh` runs, with `$` escaped for ninja.
#[derive(Clone, Default)]
struct Flags {
    args: Vec<String>,
    /// The flags for `build.ninja`, each after a space.
    ninja: String,
}
impl Flags {
    /// Add arguments, which are written to `build.ninja` as they are, as `sh` takes them as they are.
    fn plain(&mut self, args: &[&str]) {
        for arg in args {
            self.ninja.push(' ');
            self.ninja.push_str(arg);
            self.args.push((*arg).to_string());
        }
    }
    /// Add arguments, each quoted for `sh` in `build.ninja`, as a path may have spaces, and ninja would take a `$` as a
    /// variable.
    fn quoted(&mut self, args: &[String]) {
        for arg in args {
            let _ = write!(self.ninja, " {}", ninja_quote(arg));
            self.args.push(arg.clone());
        }
    }
    /// Add a codegen option, `-C <option>=<value>`, quoted for `sh` in `build.ninja` if it has to be, as a value, such
    /// as of `llvm-args`, may have spaces, and ninja would take a `$` as a variable.
    fn codegen(&mut self, option: &str) {
        let escaped = option.replace('$', "$$");
        if escaped.contains(|c: char| !c.is_ascii_alphanumeric() && !"-_=.,/+$".contains(c)) {
            let _ = write!(self.ninja, " -C {}", shell_quote(&escaped));
        } else {
            let _ = write!(self.ninja, " -C {escaped}");
        }
        self.args.extend(["-C".to_string(), option.to_string()]);
    }
    /// Add the `--cfg` flags of configuration options, each quoted for `sh` in `build.ninja`.
    fn cfg(&mut self, cfg: &[String]) {
        for option in cfg {
            let _ = write!(self.ninja, " --cfg {}", ninja_quote(option));
            self.args.extend(["--cfg".to_string(), option.clone()]);
        }
    }
    /// Returns the flags as `build.ninja` has them.
    fn ninja(&self) -> &str {
        self.ninja.strip_prefix(' ').unwrap_or(&self.ninja)
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
    /// Returns the program which runs rustc, rustdoc or clippy for the crate, with arguments before those of the crate,
    /// then its flags, then its root, writing stderr to a file if one is given.
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

/// Returns the arguments of rustc which compile a crate to `output`, with its dep-info beside it.
fn rustc_emit(tmpdir: &Path, output: &Path) -> [String; 4] {
    [
        "--color=always".into(),
        format!("--out-dir={}", tmpdir.display()),
        format!("--emit=link={}", output.display()),
        format!("--emit=dep-info={}.d", output.display()),
    ]
}

/// Returns the arguments of rustc or clippy which check a crate, to its metadata, `output`, with JSON diagnostics,
/// unless it is linted, when they are coloured as clippy shows them.
fn metadata_emit(outdir: &Path, output: &Path, lint: bool) -> [String; 4] {
    [
        if lint { "--color=always".into() } else { "--error-format=json".into() },
        format!("--out-dir={}", outdir.display()),
        format!("--emit=metadata={}", output.display()),
        format!("--emit=dep-info={}.d", output.display()),
    ]
}

/// Returns the environment which selects the toolchain, if there is one.
fn toolchain_env(toolchain: Option<&str>) -> Vec<(String, String)> {
    toolchain.map(|toolchain| ("RUSTUP_TOOLCHAIN".to_string(), toolchain.to_string())).into_iter().collect()
}

/// Returns a program, run from a directory, which is the build directory without one.
fn program(program: &str, args: Vec<String>, dir: Option<&Path>, env: Vec<(String, String)>) -> rig::Exec {
    let dir = dir.map(|dir| dir.display().to_string());
    rig::Exec { program: program.into(), args, env, dir, stderr: None }
}

/// Returns a program run from the build directory, as rustc is, whose paths are relative to it.
fn program_in_build_dir(name: &str, args: Vec<String>, env: Vec<(String, String)>) -> rig::Exec {
    program(name, args, None, env)
}

/// Returns environment variables as the assignments which `sh` takes before a command, quoted for it, with `$` escaped
/// for ninja.
fn crate_env(env: &[(String, String)]) -> String {
    env.iter().fold(String::new(), |mut assignments, (name, value)| {
        let _ = write!(assignments, "{name}={} ", ninja_quote(value));
        assignments
    })
}

/// Quote a string as a single argument for `sh`, in a ninja command.
fn ninja_quote(s: &str) -> String {
    shell_quote(s).replace('$', "$$")
}

/// Quote a string as a single argument for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Returns the codegen options of a crate, followed by the linker of its target, unless the crate sets its own linker or
/// linker flavor, as the two go together.
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
/// The crates are looked up by name, and the builtin crates by target, rather than each crate being compared with every
/// other, which would grow with the square of their number.
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
                if (d.target == c.target || d.kind == Kind::ProcMacro) && d.kind != Kind::Bin {
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

/// Returns the specification of a builtin target, `base`, which a custom target inherits, which is unstable, so it
/// needs a nightly toolchain.
///
/// It fails if rustc does, or prints no specification, as it may not exit unsuccessfully, with what rustc reported.
fn target_spec_json(target: &str, base: &str, toolchain: Option<&str>) -> Result<String, Error> {
    let mut cmd = std::process::Command::new("rustc");
    cmd.args(["-Zunstable-options", "--print=target-spec-json", &format!("--target={base}")]);
    toolchain::apply(&mut cmd, toolchain);
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
    fn normalizing_through_links() {
        // a symbolic link before each `..` is resolved, which reads the link
        let dir = std::env::temp_dir().join(format!("jig-bench-normalize-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real/inner")).unwrap();
        let _ = std::os::unix::fs::symlink("real/inner", dir.join("link"));
        let path = |n: usize| dir.join("link/../inner/../".repeat(n)).join("Jig");
        assert_linear("normalize", 50, path, |path| {
            std::hint::black_box(normalize(path));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
