//! Generation of the files which build a project: the specifications of its custom targets, `build.ninja` and its
//! depfile in the build directory, and `.rust-project.json` and the database, read by `jig build`, `jig check`,
//! `jig test` and `jig install`, at the project root.
//!
//! Ninja runs in the build directory, so the paths in `build.ninja` are relative to it, while the project's paths are
//! relative to the project root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ninja::{self, NinjaFile};
use crate::project::rust::{Crate, Kind};
use crate::project::{CustomKind, Installed, Project, Severity, Target, TargetKind};
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
    write_depfile(&project, &at, &build_dir)?;

    let installed = project.installed();
    let mut crates: Vec<_> = std::mem::take(&mut project.crates).into_values().collect();
    crates.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
    let units = units(&project, &crates, sysroot, &build_dir);
    write_build_ninja(&project, &crates, &units, &installed, input, &at, &build_dir)?;
    write_rust_project(sysroot, &units)?;
    retire_build_dir(&build_dir)?;
    write_database(&project, &units, &installed, input, &at, &build_dir)
}

/// Rebases the paths of a project, relative to its root, for ninja, which runs in the build directory.
///
/// A path within the build directory is relative to it, while any other is absolute, so that rustc, which also runs in
/// the build directory, writes the paths of sources in its dep-info and diagnostics as absolute paths, which ninja and
/// rust-analyzer can both find.
struct Rebase {
    /// The project root, which is absolute.
    root: PathBuf,
    /// The build directory, which is absolute.
    build_dir: PathBuf,
}
impl Rebase {
    fn new(build_dir: &Path) -> Self {
        let root = normalize(&std::env::current_dir().unwrap_or_default());
        let build_dir = normalize(&root.join(build_dir));
        Self { root, build_dir }
    }
    fn path(&self, path: &Path) -> PathBuf {
        let path = normalize(&self.root.join(path));
        match path.strip_prefix(&self.build_dir) {
            Ok(relative) if relative.as_os_str().is_empty() => PathBuf::from("."),
            Ok(relative) => relative.to_path_buf(),
            Err(_) => path,
        }
    }
    fn paths<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
        paths.into_iter().map(|path| self.path(path)).collect()
    }
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
    /// The directory of the outputs for the target.
    pub dir: PathBuf,
}
impl Unit {
    fn check_dir(&self) -> PathBuf {
        self.dir.join("check")
    }
    fn check_rmeta(&self) -> PathBuf {
        self.check_dir().join(format!("lib{}.rmeta", self.name))
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
    /// The `--target` flag, which names a builtin target, or gives the path of the specification of a custom target.
    fn flag(&self, at: &Rebase) -> String {
        match &self.spec {
            Some(spec) => format!("--target={} -Zunstable-options", at.path(spec).display()),
            None => format!("--target={}", self.target),
        }
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
    let path = build_dir.join("build.ninja.d");
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
    write!(w, "build.ninja:")?;
    let stamp = stamp.to_path_buf();
    let paths = project.files.iter().map(|file| &file.path).chain(&project.dependencies).chain([&stamp]);
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
    let mut units = Vec::new();
    for (c, deps) in crates.iter().zip(externs(crates)) {
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
            // crates from the toolchain, such as `core`, are not edited
            member: !rust_analyzer::absolute(&c.path).starts_with(sysroot),
            build_file: project.files[c.name_span.file].path.clone(),
            codegen: codegen(c, project),
            cfg: c.cfg.clone(),
            env: c.env.clone(),
            dir: c.dir.clone(),
        });
    }
    units
}

fn write_build_ninja(
    project: &Project,
    crates: &[Crate],
    units: &[Unit],
    installed: &[Installed],
    input: &Path,
    at: &Rebase,
    build_dir: &Path,
) -> Result<(), Error> {
    let mut build_ninja = NinjaFile::new(build_dir.join("build.ninja"))?;
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
    build_ninja.rule("configure", &[
        ("command", &command),
        ("description", "CONFIGURE $out"),
        ("pool", "console"),
        ("deps", "gcc"),
        ("depfile", "$out.d"),
        ("generator", "1"),
    ])?;
    let exe = at.paths(&exe);
    build_ninja.build(&ninja::Build {
        rule: "configure",
        outputs: &[Path::new("build.ninja")],
        inputs: &[&at.path(input)],
        implicit: &as_paths(&exe),
        ..Default::default()
    })?;

    build_ninja.variable("edition", "2024")?;
    // the temporary files rustc writes to `--out-dir` are kept in the build directory, in a directory for each edge, as
    // they are named after the crate, which may be compiled for several targets, or as a test harness, at once
    // `crate-env` is the environment variables of a crate, before rustc, as `sh` takes them
    let rustc = &format!("{env}$crate-env rustc --color=always --out-dir=$tmpdir --emit=link=$out --emit=dep-info=$depfile --crate-type=$crate-type --crate-name=$crate-name --edition=$edition $flags $in");
    build_ninja.rule("rustc", &[
        ("command", rustc),
        ("description", "RUSTC $out"),
        ("deps", "gcc"),
        ("depfile", "$out.d"),
    ])?;

    build_ninja.rule("rustc-check", &[
        ("command", &format!("{env}$crate-env rustc --error-format=json --out-dir=$outdir --emit=metadata=$out --emit=dep-info=$out.d --crate-type=$crate-type --crate-name=$crate-name --edition=$edition $flags $in 2> $diagnostics")),
        ("description", "CHECK $out"),
        ("deps", "gcc"),
        ("depfile", "$out.d"),
    ])?;

    // the targets built by `ninja` without any, which are all targets if there are none
    let mut defaults: Vec<PathBuf> = Vec::new();
    // what is built without defaults, which is all but the checks, which are for rust-analyzer
    let mut everything: Vec<PathBuf> = Vec::new();

    // the command of an edge is `command-line`, as its `command` would replace the rule's
    build_ninja.rule("custom", &[("command", &format!("{cd} {env}$command-line")), ("description", "CUSTOM $name")])?;
    let mut tests = Vec::new();
    for custom in &project.customs {
        // a test is run by `jig test`, which builds its inputs and program with this target first
        let CustomKind::Command { outputs, default, .. } = &custom.kind else {
            let phony = Path::new("test").join(&custom.name);
            let inputs = custom.inputs(&project.runners);
            let inputs = at.paths(inputs.iter().chain(&custom.program).chain(&custom.depends));
            build_ninja.build(&ninja::Build { rule: "phony", outputs: &[&phony], inputs: &as_paths(&inputs), ..Default::default() })?;
            tests.push(phony);
            continue;
        };
        // ninja runs commands with `sh`, which must not split or expand the arguments
        let command: Vec<_> = custom.command.iter().map(|argument| shell_quote(argument)).collect();
        let command = command.join(" ").replace('$', "$$");
        let outputs = at.paths(outputs);
        build_ninja.build(&ninja::Build {
            rule: "custom",
            outputs: &as_paths(&outputs),
            inputs: &as_paths(&at.paths(&custom.inputs)),
            implicit: &as_paths(&at.paths(&custom.program)),
            order_only: &as_paths(&at.paths(&custom.depends)),
            variables: &[("command-line", &command), ("name", &custom.name)],
            ..Default::default()
        })?;
        // so that the outputs can be built by name
        let phony = Path::new("custom").join(&custom.name);
        build_ninja.build(&ninja::Build {
            rule: "phony",
            outputs: &[&phony],
            inputs: &as_paths(&outputs),
            ..Default::default()
        })?;
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
        let mut flags = rustc_flags(&target.flag(at), &at.path(&c.dir), c.kind, &deps, &codegen(c, project), at);
        flags.push_str(&cfg_flags(&c.cfg));
        let crate_env = crate_env(&c.env);
        let deps: Vec<_> = deps.iter().map(|d| d.path.as_path()).chain(as_paths(&spec)).collect();
        // the linker script of the crate, or else of its target, which is passed to rustc where it links, for the
        // test harness and any crate but a library, which then depends on it
        let link_script = c.link_script.as_ref().or_else(|| project.target(&c.target)?.link_script.as_ref());
        let link_script = link_script.map(|path| at.path(path));
        let link_flags = match &link_script {
            Some(path) => codegen_flag(&format!("link-arg=-T{}", path.display())),
            None => String::new(),
        };
        let link_deps: Vec<_> = deps.iter().copied().chain(link_script.as_deref()).collect();
        let depends = at.paths(&c.depends);
        let crate_type = c.kind.crate_type();
        let output = at.path(&c.output());
        let input = at.path(&c.path);
        let tmpdir = Path::new("tmp").join(&c.target).join(&c.name);
        let links = c.kind != Kind::Lib;
        build_ninja.build(&ninja::Build {
            rule: "rustc",
            outputs: &[&output],
            inputs: &[&input],
            implicit: if links { &link_deps } else { &deps },
            order_only: &as_paths(&depends),
            variables: &[
                ("crate-name", &c.name),
                ("crate-type", crate_type),
                ("flags", &if links { format!("{flags}{link_flags}") } else { flags.clone() }),
                ("tmpdir", &tmpdir.display().to_string()),
                ("crate-env", &crate_env),
            ],
            ..Default::default()
        })?;
        everything.push(output.clone());
        if c.test {
            everything.push(at.path(&c.test_harness()));
            let tmpdir = Path::new("tmp").join(&c.target).join("test").join(&c.name);
            build_ninja.build(&ninja::Build {
                rule: "rustc",
                outputs: &[&at.path(&c.test_harness())],
                inputs: &[&input],
                implicit: &link_deps,
                order_only: &as_paths(&depends),
                variables: &[
                    ("crate-name", &c.name),
                    ("crate-type", crate_type),
                    ("flags", &format!("{flags}{link_flags} --test")),
                    ("tmpdir", &tmpdir.display().to_string()),
                    ("crate-env", &crate_env),
                ],
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
        let mut flags = rustc_flags(&target.flag(at), &outdir, unit.kind, &deps, &unit.codegen, at);
        flags.push_str(&cfg_flags(&unit.cfg));
        if unit.test {
            flags.push_str(" --test");
        }
        let crate_env = crate_env(&unit.env);
        let deps: Vec<_> = deps.iter().map(|d| d.path.as_path()).chain(as_paths(&spec)).collect();
        let output = at.path(&unit.check_rmeta());
        let diagnostics = at.path(&unit.diagnostics());
        build_ninja.build(&ninja::Build {
            rule: "rustc-check",
            outputs: &[&output],
            implicit_outputs: &[&diagnostics],
            inputs: &[&at.path(&unit.root)],
            implicit: &deps,
            variables: &[
                ("crate-name", &unit.name),
                ("crate-type", unit.kind.crate_type()),
                ("flags", &flags),
                ("outdir", &outdir.display().to_string()),
                ("diagnostics", &diagnostics.display().to_string()),
                ("crate-env", &crate_env),
            ],
            ..Default::default()
        })?;
        build_ninja.build(&ninja::Build {
            rule: "phony",
            outputs: &[&Path::new("check").join(&unit.label)],
            inputs: &[&output],
            ..Default::default()
        })?;
    }

    // builds what all of the tests need
    build_ninja.build(&ninja::Build { rule: "phony", outputs: &[Path::new("test")], inputs: &as_paths(&tests), ..Default::default() })?;
    // builds what `jig install` installs
    let installed = at.paths(installed.iter().map(|file| &file.path));
    build_ninja.build(&ninja::Build { rule: "phony", outputs: &[Path::new("install")], inputs: &as_paths(&installed), ..Default::default() })?;

    // without a `default` statement, ninja would build every target, including the checks
    let defaults = if defaults.is_empty() { everything } else { defaults };
    if !defaults.is_empty() {
        build_ninja.default(&as_paths(&defaults))?;
    }

    build_ninja.sync_all()
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
    let path = previous.join("build.ninja");
    if !path.is_file() {
        return Ok(());
    }
    let stub = format!(
        "# `build-dir` is now {:?}, where `jig generate` writes `build.ninja`\nbuild build.ninja: phony\n",
        build_dir.display().to_string(),
    );
    if let Err(error) = std::fs::write(&path, stub) {
        return Err(Error::Write(path, error));
    }
    Ok(())
}

/// The command of a test, which the database borrows.
struct TestCommand<'a> {
    name: &'a str,
    command: Vec<String>,
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
    let proxy_build = project.deferred_severity("proxy-build", Severity::Warning).to_string();
    let other_jig = project.deferred_severity("other-jig", Severity::Warning).to_string();
    // the `jig` which regenerates `build.ninja`, as its configure rule runs it
    let jig = std::env::current_exe().map(|jig| jig.display().to_string()).unwrap_or_default();
    let missing: Vec<_> = project.missing.iter().map(|path| path.display().to_string()).collect();
    // the database borrows the paths and commands
    let diagnostics: Vec<_> = units.iter().map(|unit| unit.diagnostics().display().to_string()).collect();
    let mut commands = Vec::new();
    for custom in &project.customs {
        if custom.is_test() {
            commands.push(TestCommand { name: &custom.name, command: custom.command(&project.runners) });
        }
    }

    let missing = missing.iter().map(String::as_str).collect();
    let install_paths: Vec<_> = installed.iter().map(|file| file.path.display().to_string()).collect();
    let install_targets: Vec<_> = installed.iter().map(|file| at.path(&file.path).display().to_string()).collect();
    let mut database = database::Database {
        input: &input,
        toolchain: project.toolchain.as_deref().unwrap_or_default(),
        build_dir: &build_dir,
        missing,
        install_prefix: &install_prefix,
        proxy_build: &proxy_build,
        other_jig: &other_jig,
        jig: &jig,
        ..Default::default()
    };
    for (unit, diagnostics) in units.iter().zip(&diagnostics) {
        let deps = unit.deps.iter().map(|&index| index as u32).collect();
        let (label, member) = (&unit.label, unit.member);
        database.flychecks.push(database::Flycheck { label, diagnostics, member, deps });
    }
    for test in &commands {
        let command = test.command.iter().map(String::as_str).collect();
        database.tests.push(database::Test { name: test.name, command });
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
fn rustc_flags(target: &str, search: &Path, kind: Kind, deps: &[Extern], codegen: &[String], at: &Rebase) -> String {
    let mut flags = format!("{target} -L dependency={}", search.display());
    for option in codegen {
        let option = match option.strip_prefix("linker=") {
            Some(linker) if linker.contains('/') => format!("linker={}", at.path(Path::new(linker)).display()),
            _ => option.clone(),
        };
        flags.push_str(&codegen_flag(&option));
    }
    // the toolchain's `proc_macro` crate is not in the extern prelude otherwise
    if kind == Kind::ProcMacro {
        flags.push_str(" --extern proc_macro");
    }
    for dep in deps {
        flags.push_str(&format!(" --extern {}={}", dep.name, dep.path.display()));
    }
    flags
}

/// Returns the flag of a codegen option, `<option>=<value>`, after a space.
fn codegen_flag(option: &str) -> String {
    // a value, such as of `llvm-args`, may have spaces, and ninja would take a `$` as a variable
    let option = option.replace('$', "$$");
    if option.contains(|c: char| !c.is_ascii_alphanumeric() && !"-_=.,/+$".contains(c)) {
        format!(" -C {}", shell_quote(&option))
    } else {
        format!(" -C {option}")
    }
}

/// Returns the `--cfg` flags of configuration options, each quoted for `sh`, with `$` escaped for ninja.
fn cfg_flags(cfg: &[String]) -> String {
    cfg.iter().map(|option| format!(" --cfg {}", ninja_quote(option))).collect()
}

/// Returns environment variables as the assignments which `sh` takes before a command, quoted for it, with `$` escaped
/// for ninja.
fn crate_env(env: &[(String, String)]) -> String {
    env.iter().map(|(name, value)| format!("{name}={} ", ninja_quote(value))).collect()
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
        Err(error) => return Err(Error::Exec(cmd, error)),
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
        let chain = |n: usize| {
            let mut source = String::from("crate c0 'c.rs'\n");
            for i in 1..n {
                source += &format!("crate c{i} 'c.rs'\n    use c{}\n", i - 1);
            }
            crates(&source)
        };
        assert_linear("externs-chain", 2000, chain, |crates| {
            std::hint::black_box(externs(crates));
        });
    }

    #[test]
    fn externs_with_builtins() {
        // like the crates of the standard library, which are passed to every other crate
        let with_builtins = |n: usize| {
            let mut source = String::from("crate core 'c.rs'\n    builtin\ncrate alloc 'c.rs'\n    builtin\n    use core\n");
            for i in 0..n {
                source += &format!("crate c{i} 'c.rs'\n");
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
