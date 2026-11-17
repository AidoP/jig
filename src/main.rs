use std::{io::IsTerminal, path::{Path, PathBuf}};

#[cfg(test)]
mod bench;
#[cfg(test)]
mod book;
mod cache;
mod config;
mod check;
mod configured;
mod database;
#[cfg(test)]
mod differential;
mod doc;
mod generate;
mod install;
mod lint;
mod project;
mod rig_builder;
mod rig_graph;
mod platform;
mod run;
mod rustc;
mod rust_analyzer;
mod show_graph;
mod target_spec;
mod test_runner;
mod timings;
mod toolchain;

// the program the differential tests' edges run, which is this executable, run again
#[cfg(test)]
rig::register_test_helper!();

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or("jig".into());
    let ran = run(args);
    // what took how long is shown last, whether the command succeeded or not
    if let Some(report) = timings::report() {
        eprint!("{report}");
    }
    match ran {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{}: {error}", program.display());
            std::process::ExitCode::FAILURE
        },
    }
}

/// How jig is used, as shown by `jig help` and with a mistaken command.
///
/// The internal commands are left out: `check <label>`, which rust-analyzer runs as its flycheck, and
/// `build --proxy <prefix> <wrapper>`, which the wrappers installed by `install --proxy` run.
const USAGE: &str = "\
usage: jig [+<toolchain>] [--color <when>] [--timings] <command> [<argument> ...]

A rustup toolchain given with `+` configures the project with it, and is used until it is configured with another.
`--color` is `auto`, by default, to show colour where the output is a terminal, or `always` or `never`. `--timings`
shows on stderr, once the command has finished, how long configuring, building, running and testing took, and the
commands of the build which took longest.

A command takes its options anywhere before `--`, and fails for one it does not take. `-h` or `--help` after any
command shows this message. `test`, `run`, `lint`, `doc` and `install` build first, and take the options of `build` which are
about building: `-j`, `-k` and `-q`.

commands:
    generate [<input>]   evaluate the configuration, `Jig` by default, and generate rig's graph in the build
                         directory
    build [-j <jobs>] [-k <failures>] [-n] [-v] [-q] [<target> ...]
                         build the project, or the targets given, with as many jobs at once, going on until as many
                         commands have failed, `0` for any, or showing what would run, and why, with `-n`, the
                         commands rather than their descriptions with `-v`, or only what they print with `-q` or
                         `--quiet`
    clean [<target> ...] remove what the build wrote, or what builds the targets given and what they are built from,
                         by the outputs of the graph, so that it is built again, leaving the graph and what the build
                         did not write
    test [-v] [-j <jobs>] [--timeout <seconds>] [--all] [--tag <tag> ...] [<name> ...] [-- <argument> ...]
                         build and run the tests without tags and those with a tag in `test-tags`, or only those named
                         or tagged with `--tag`, or all of them with `--all`, where `-v` or `--verbose` shows the
                         output of each test, such as the tests of a crate's test harness, rather than only those which
                         fail, `-j` builds and runs the tests with as many jobs, `--timeout` fails a test which runs
                         for longer in place of its own timeout, and each test is given the arguments after `--`
    run [<name>] [--] [<argument> ...]
                         build a binary crate and run it with arguments, by its target's runner if it has one, where
                         a crate for a target other than the host is `<name>@<target>`, unless it is the only one with
                         the name, and without a name, the only binary crate is run, where what follows the name, or
                         `--`, is the program's
    lint [-D] [<name> ...]
                         lint the crates which are edited with clippy, or only those named, as the `clippy/<lint>`
                         properties set where they are declared give the levels of the lints, where `-D` or
                         `--deny-warnings` fails if there is a warning
    doc [--open] [<name> ...]
                         document the crates with `doc`, or only those named, with rustdoc, and show where the first
                         page of each is, opening the first in the browser with `--open`
    cache stats|gc [--max-size <size>]|clear
                         show what rig's cache holds, remove what was used longest ago until it is no larger than a
                         size, `cache/size` or 10 GiB by default, as is done after a build once a day, or remove
                         everything in it
    serve --stdio [--push] | serve --listen <address> --token-file <path> [--push]
                         serve rig's cache on stdin and stdout as a remote cache, as `cache/remote` runs it by ssh, or
                         over TCP at an address, such as `0.0.0.0:7070`, to clients with the token in the file,
                         taking pushes with `--push`
    worker (--stdio | --listen <address> --token-file <path>) [--jobs <n>] [--tag <tag> ...] [--install]
                         run the cached commands of builds which send them, as `workers/list` runs it by ssh,
                         on stdin and stdout, or over TCP at an address, for clients with the token in the file, with
                         rig's cache as its store, and the toolchains rustup has, running as many at once as `--jobs`,
                         reporting its tags, and with `--install`, installing a toolchain a command needs with rustup
    workers              show the workers of `workers/list`, with their hosts, tags and toolchains
    graph [--json] [<output> ...]
                         show rig's graph of the project, once it is up to date: each command, headed by its outputs,
                         with its inputs, dependencies and command line, or as JSON with `--json`, or only those of the
                         outputs given
    install [--proxy] [<prefix>]
                         build and install the files with `install` in a prefix, the `install-prefix` property by
                         default, or with `--proxy`, install wrappers which build and run programs, and links to
                         other files
    help                 show this message, as `-h` and `--help` do
    version              show the version of jig, as `--version` does";

fn run(args: std::env::ArgsOs) -> Result<std::process::ExitCode, Error> {
    use std::process::ExitCode;
    let mut args = args.peekable();
    let toolchain = toolchain::parse(&mut args)?;
    // the options before the command, in either order
    let mut when = None;
    loop {
        if args.next_if(|arg| arg == "--timings").is_some() {
            timings::enable();
        } else if when.is_none() && args.peek().is_some_and(|arg| arg.to_string_lossy().starts_with("--color")) {
            when = Some(parse_colour(&mut args)?);
        } else {
            break;
        }
    }
    let _ = COLOUR.set(when.unwrap_or(Colour::Auto));
    let Some(command) = args.next() else {
        return Err(Error::MissingArg("command"));
    };
    let command = command.to_string_lossy();
    // `--help` and `--version` are taken in place of a command, as other programs take them
    let command = match command.as_ref() {
        "-h" | "--help" => "help",
        "--version" => "version",
        command => command,
    };
    let args: Vec<_> = args.collect();
    // `jig build --proxy <prefix> <wrapper>`, which the wrappers `jig install --proxy` installs run, takes no options
    let proxy = command == "build" && args.first().is_some_and(|arg| arg == "--proxy");
    let parsed = if proxy { None } else { Some(Args::parse(command, args.iter().cloned())?) };
    if command == "help" || parsed.as_ref().is_some_and(|args| args.help) {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    // the other commands use the toolchain the project is configured with, which is changed first if another is given
    if let Some(toolchain) = &toolchain && !matches!(command, "generate" | "version") {
        configured::ensure(Some(toolchain))?;
    }
    let Some(args) = parsed else {
        let [_, prefix, wrapper] = &args[..] else {
            return Err(args.get(3).map_or(Error::MissingArg("wrapper"), |extra| Error::ExtraArg(lossy(extra))));
        };
        let reinstalled = install::build_proxy(prefix.as_ref(), wrapper.as_ref())?;
        return Ok(if reinstalled { ExitCode::from(install::REINSTALLED) } else { ExitCode::SUCCESS });
    };
    match command {
        "generate" => {
            let input = args.operands.first().map_or_else(|| PathBuf::from("Jig"), PathBuf::from);
            timings::step("configure", || generate(input, toolchain.as_deref())).map(|()| ExitCode::SUCCESS)
        },
        "build" => {
            let project = configured::Configured::find(configured::Output::Stdout)?;
            // in the form the graph has them, with `/`, which a path written on Windows may not use
            let targets: Vec<String> = args.operands.iter().map(|target| rig_graph::path_string(Path::new(target))).collect();
            let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
            let request = rig_builder::Request { targets: &targets, build: args.build()?, ..Default::default() };
            project.build(&request).map(|()| ExitCode::SUCCESS)
        },
        "clean" => {
            // what the graph which was last generated builds, without configuring the project again first
            let project = configured::Configured::locate()?;
            let targets: Vec<String> = args.operands.iter().map(|target| rig_graph::path_string(Path::new(target))).collect();
            let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
            let cleaned = project.builder().clean(&targets)?;
            let plural = if cleaned.files == 1 { "" } else { "s" };
            println!("removed {} file{plural}", cleaned.files);
            Ok(ExitCode::SUCCESS)
        },
        "check" => {
            let Some(label) = args.operands.first() else {
                return Err(Error::MissingArg("label"));
            };
            check::check(&label.to_string_lossy());
            Ok(ExitCode::SUCCESS)
        },
        "test" => {
            let (selection, options) = test_runner::options(&args)?;
            test_runner::test(&selection, &options).map(|()| ExitCode::SUCCESS)
        },
        "run" => run::run(args.operands.first().map(lossy).as_deref(), &args.passed, args.build()?),
        "cache" => cache::command(&args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "serve" => cache::serve(&args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "worker" => cache::worker(&args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "workers" => cache::workers(&user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "graph" => show_graph::show(args.flag("json"), &args.names()).map(|()| ExitCode::SUCCESS),
        "doc" => doc::doc(&args.names(), args.flag("open"), args.build()?).map(|()| ExitCode::SUCCESS),
        "lint" => lint::lint(&args.names(), args.flag("deny-warnings"), args.build()?).map(|()| ExitCode::SUCCESS),
        "install" => {
            let prefix = args.operands.first().map(PathBuf::from);
            install::install(prefix, args.flag("proxy"), args.build()?).map(|()| ExitCode::SUCCESS)
        },
        "version" => {
            println!("jig {}", version());
            Ok(ExitCode::SUCCESS)
        },
        // every command has a spec, so that it is parsed above
        command => Err(Error::UnknownCommand(command.into())),
    }
}

/// The version of jig, which a release gives the build as `JIG_VERSION`, from the `jig-version` property in `Jig`,
/// or `unreleased` for any other build.
fn version() -> &'static str {
    option_env!("JIG_VERSION").filter(|version| !version.is_empty()).unwrap_or("unreleased")
}

fn lossy(arg: &std::ffi::OsString) -> String {
    arg.to_string_lossy().into_owned()
}

/// An option of a command: its long name, its short one, and the name of its value, if it takes one.
#[derive(Clone, Copy)]
struct Opt {
    long: &'static str,
    short: Option<char>,
    value: Option<&'static str>,
}

const fn switch(long: &'static str, short: Option<char>) -> Opt {
    Opt { long, short, value: None }
}

const fn valued(long: &'static str, short: Option<char>, value: &'static str) -> Opt {
    Opt { long, short, value: Some(value) }
}

const JOBS: Opt = valued("jobs", Some('j'), "jobs");
const VERBOSE: Opt = switch("verbose", Some('v'));
const TAG: Opt = valued("tag", None, "tag");
const STDIO: Opt = switch("stdio", None);
const LISTEN: Opt = valued("listen", None, "address");
const TOKEN_FILE: Opt = valued("token-file", None, "path");

/// The options of `jig build` which are about building, which the commands which build first take too.
const BUILDING: [Opt; 3] = [JOBS, valued("keep-going", Some('k'), "failures"), switch("quiet", Some('q'))];

/// What a command takes, beside `--help` and `-h`.
struct Spec {
    name: &'static str,
    /// Its own options.
    options: &'static [Opt],
    /// Whether it builds first, and takes the options of [`BUILDING`].
    builds: bool,
    /// The most operands it takes, if there is a limit.
    operands: Option<usize>,
    /// Whether it passes arguments on to what it runs: what follows `--`, and what follows its operands once it has
    /// as many as it takes, though they look like options.
    passes_on: bool,
}

const fn spec(name: &'static str, options: &'static [Opt], builds: bool, operands: Option<usize>) -> Spec {
    Spec { name, options, builds, operands, passes_on: false }
}

/// Every command's [`Spec`], but for `jig build --proxy`, which the wrappers it installs run.
const SPECS: [Spec; 16] = [
    spec("generate", &[], false, Some(1)),
    spec("build", &[switch("dry-run", Some('n')), VERBOSE], true, None),
    spec("clean", &[], false, None),
    spec("check", &[], false, Some(1)),
    Spec { passes_on: true, ..spec("test", &[VERBOSE, switch("all", None), TAG, valued("timeout", None, "seconds")], true, None) },
    Spec { passes_on: true, ..spec("run", &[], true, Some(1)) },
    spec("lint", &[switch("deny-warnings", Some('D'))], true, None),
    spec("doc", &[switch("open", None)], true, None),
    spec("install", &[switch("proxy", None)], true, Some(1)),
    spec("cache", &[valued("max-size", None, "size")], false, Some(1)),
    spec("serve", &[STDIO, LISTEN, TOKEN_FILE, switch("push", None)], false, Some(0)),
    spec("worker", &[STDIO, LISTEN, TOKEN_FILE, JOBS, TAG, switch("install", None)], false, Some(0)),
    spec("workers", &[], false, Some(0)),
    spec("graph", &[switch("json", None)], false, None),
    spec("help", &[], false, Some(0)),
    spec("version", &[], false, Some(0)),
];

/// The arguments of a command, parsed by its [`Spec`], which every command is.
///
/// Options are taken wherever they are before `--`, as `--<long>`, `-<short>`, and with a value, as
/// `--<long> <value>`, `--<long>=<value>`, `-<short> <value>` or `-<short><value>`, such as `-j4`. An option the
/// command does not take is an error, as is an operand beyond those it takes.
struct Args {
    /// The options given, by their long names, with their values, in the order they were given.
    options: Vec<(&'static str, Option<String>)>,
    /// What is not an option: names, targets, a path.
    operands: Vec<std::ffi::OsString>,
    /// The arguments of what the command runs, for a command which [passes them on](Spec::passes_on).
    passed: Vec<std::ffi::OsString>,
    /// Whether `--help` or `-h` was given.
    help: bool,
}

impl Args {
    /// Parse the arguments of a command, which fails for a command which jig does not have.
    fn parse(command: &str, args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self, Error> {
        let spec = SPECS.iter().find(|spec| spec.name == command).ok_or_else(|| Error::UnknownCommand(command.into()))?;
        let building: &[Opt] = if spec.builds { &BUILDING } else { &[] };
        let takes = |found: &dyn Fn(&Opt) -> bool| spec.options.iter().chain(building).copied().find(|opt| found(opt));
        let mut parsed = Self { options: Vec::new(), operands: Vec::new(), passed: Vec::new(), help: false };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            // once a command which passes arguments on has its operands, the rest are passed on, after a `--`
            if spec.passes_on && spec.operands == Some(parsed.operands.len()) {
                parsed.passed.extend((arg != "--").then_some(arg));
                parsed.passed.extend(args.by_ref());
                break;
            }
            if arg == "--" {
                let rest = if spec.passes_on { &mut parsed.passed } else { &mut parsed.operands };
                rest.extend(args.by_ref());
                break;
            }
            let text = arg.to_string_lossy();
            if text == "-" || !text.starts_with('-') {
                parsed.operands.push(arg);
                continue;
            }
            if text == "--help" || text == "-h" {
                parsed.help = true;
                continue;
            }
            // the option, and its value if it is in the same argument
            let (opt, inline) = if let Some(long) = text.strip_prefix("--") {
                let (name, inline) = long.split_once('=').map_or((long, None), |(name, value)| (name, Some(value)));
                (takes(&|opt| opt.long == name), inline)
            } else {
                let short = text[1..].chars().next();
                let rest = &text[1 + short.map_or(0, char::len_utf8)..];
                (takes(&|opt| opt.short.is_some() && opt.short == short), Some(rest).filter(|rest| !rest.is_empty()))
            };
            let unknown = || Error::UnknownOption(spec.name, text.clone().into_owned());
            let opt = opt.ok_or_else(unknown)?;
            let value = match (opt.value, inline) {
                (None, None) => None,
                // a value given to an option which takes none, as `--quiet=yes`, or several short options at once
                (None, Some(_)) => return Err(unknown()),
                (Some(_), Some(value)) => Some(value.to_string()),
                (Some(name), None) => Some(args.next().ok_or(Error::MissingArg(name))?.to_string_lossy().into_owned()),
            };
            parsed.options.push((opt.long, value));
        }
        if let Some(max) = spec.operands && let Some(extra) = parsed.operands.get(max) {
            return Err(Error::ExtraArg(lossy(extra)));
        }
        Ok(parsed)
    }
    /// Whether an option was given, by its long name.
    fn flag(&self, long: &str) -> bool {
        self.options.iter().any(|(name, _)| *name == long)
    }
    /// The value of an option, by its long name, the last given if it was given more than once.
    fn value(&self, long: &str) -> Option<&str> {
        self.values(long).last()
    }
    /// The values of an option which may be given more than once, in the order they were given.
    fn values(&self, long: &str) -> impl Iterator<Item = &str> {
        self.options.iter().filter(move |(name, _)| *name == long).filter_map(|(_, value)| value.as_deref())
    }
    /// The operands, as strings, such as names.
    fn names(&self) -> Vec<String> {
        self.operands.iter().map(lossy).collect()
    }
    /// The number of jobs given with `-j` or `--jobs`, which is above zero.
    fn jobs(&self) -> Result<Option<usize>, Error> {
        let jobs = |value: &str| value.parse().ok().filter(|&jobs| jobs > 0).ok_or_else(|| Error::InvalidJobs(value.into()));
        self.value("jobs").map(jobs).transpose()
    }
    /// How to build, from the options of [`BUILDING`], and of `jig build`.
    fn build(&self) -> Result<rig_builder::Options, Error> {
        let failures = |value: &str| value.parse().map_err(|_| Error::InvalidFailures(value.into()));
        Ok(rig_builder::Options {
            jobs: self.jobs()?,
            keep_going: self.value("keep-going").map(failures).transpose()?,
            dry_run: self.flag("dry-run"),
            verbose: self.flag("verbose"),
            quiet: self.flag("quiet"),
        })
    }
}

/// Evaluate the configuration from its root file, `input`, then generate the graph and the files beside it, with a
/// rustup toolchain if one is given.
/// The stack the configuration is evaluated on, which `project::MAX_USE_DEPTH` files, each used by the one before, fit
/// in, as an unoptimized `jig` processes them. It is only reserved, until it is used.
const CONFIGURE_STACK: usize = 64 << 20;

fn generate(input: PathBuf, toolchain: Option<&str>) -> Result<(), Error> {
    // rustc's answers, as they were when the project was last configured, if the toolchain has not changed since
    let known = std::fs::read(database::PATH).ok();
    let known = known.as_deref().and_then(|bytes| database::read(bytes).ok()).map(|database| database.rustc());
    let rustc = rustc::ask(toolchain, known)?;
    let (sysroot, host, targets) = (&rustc.sysroot, &rustc.host, &rustc.targets);
    let verbose_version = &rustc.verbose_version;
    let version = rustc.version()?;

    let mut project = project::Project::new(input.parent().map(Path::to_path_buf).unwrap_or_default(), targets.clone());
    project.toolchain = toolchain.map(String::from);
    // which toolchain rustup's proxies run, as rustc was asked of above, so that the project is configured again when it
    // changes, and the toolchain is identified afresh
    project.read_env("RUSTUP_TOOLCHAIN");
    let file = match project::File::read(input.clone()) {
        Ok(file) => file,
        Err(error) => return Err(Error::Read(input, error)),
    };
    declare_builtins(&mut project, sysroot, host, version, targets, toolchain);
    // on a thread with a stack of its own, as deep on every platform, as a file which uses itself goes deep, while the
    // main thread's stack is 8 MiB on Linux, and 1 MiB on Windows
    let evaluate = || -> Result<(), Error> {
        // the user's configuration, as if the project began by using it
        config::apply(&mut project)?;
        project.parse(file);
        // a fatal diagnostic stops the configuration, so what is after it, which the check is about, is not known
        if !project.fatal {
            project.check();
        }
        Ok(())
    };
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new().name("configure".into()).stack_size(CONFIGURE_STACK);
        let evaluating = thread.spawn_scoped(scope, evaluate).expect("failed to start configuring");
        evaluating.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })?;
    report_diagnostics(&project)?;

    // rig caches when there is a cache, which configures the project again when it is turned on or off, as the variables
    // which override the configuration are read by it
    let mut settings = cache::Settings::resolve(&project.cache_properties(), |name| project.read_env(name))?;
    if settings.dir.is_some() {
        let identity = toolchain_identity(&mut project, verbose_version, sysroot);
        // a worker may run what does not depend on the host with the same release on another, and install it by its
        // name, if it does not have it
        let release = rig::worker::Release {
            identity: toolchain::release_identity(verbose_version, Path::new(sysroot)),
            install: toolchain::install_name(Path::new(sysroot)).unwrap_or_default(),
        };
        settings.release = Some((identity.clone(), release));
        project.cache = Some(identity);
        project.identify_cached_customs();
        project.identify_static_libraries();
    }
    project.cache_settings = settings;
    generate::write(project, &input, &rustc)
}

/// Returns how rig caches outside a project, as for `jig cache` and `jig serve`, from the user's configuration alone,
/// and the variables which override it.
///
/// The builtin properties have rustc's values, if it can be run, or are empty, so that a server without Rust can serve
/// a cache.
fn user_settings(toolchain: Option<&str>) -> Result<cache::Settings, Error> {
    let rustc = rustc::ask(toolchain, None).unwrap_or_default();
    let version = rustc.version().unwrap_or_default();
    let mut project = project::Project::new(std::env::current_dir().unwrap_or_default(), rustc.targets.clone());
    project.toolchain = toolchain.map(String::from);
    declare_builtins(&mut project, &rustc.sysroot, &rustc.host, version, &rustc.targets, toolchain);
    config::apply(&mut project)?;
    if !project.fatal {
        project.check();
    }
    report_diagnostics(&project)?;
    cache::Settings::resolve(&project.cache_properties(), |name| std::env::var_os(name))
}

/// Declare the builtin properties, with the sysroot, host target, version and targets of the toolchain as rustc reports
/// them, and the properties of the codegen options and diagnostics.
fn declare_builtins(
    project: &mut project::Project,
    sysroot: &str,
    host: &str,
    version: String,
    targets: &[String],
    toolchain: Option<&str>,
) {
    let target_list = targets.iter().map(|target| project::Data::Str(target.clone())).collect();
    let cache_dir = cache::default_dir(|name| project.read_env(name));
    let install_prefix = platform::install_prefix(|name| project.read_env(name));
    let builtins = [
        (
            project::Property::new("rustc/sysroot".into(), project::Type::String)
                .info("the sysroot of the Rust toolchain")
                .read_only(),
            sysroot.into(),
        ),
        (
            project::Property::new("rustc/version".into(), project::Type::String)
                .info("the version of rustc, such as `1.95.0`, or `1.96.0-nightly` for a nightly toolchain")
                .read_only(),
            version,
        ),
        (
            project::Property::new("target".into(), project::Type::String)
                .info("the target triple for crates without a `target` attribute"),
            host.into(),
        ),
        (
            project::Property::new("build-dir".into(), project::Type::String)
                .info("the directory of what is built, relative to the project root, which is read-only once it is used"),
            "build".into(),
        ),
        (
            project::Property::new("install-prefix".into(), project::Type::String)
                .info(
                    "the directory `jig install` installs in, unless it is given another: `/usr/local` on Unix, and \
                     `%LOCALAPPDATA%\\Programs` on Windows",
                ),
            install_prefix,
        ),
        (
            project::Property::new("change-detection".into(), project::Type::String).info(
                "how rig finds that a file has changed: `'timestamp'`, by its time and size, or `'hash'`, by its \
                 contents too, so that what is written again unchanged builds nothing more",
            ),
            "timestamp".into(),
        ),
        (
            project::Property::new("host-target".into(), project::Type::String)
                .info("the target triple of the machine running the build")
                .read_only(),
            host.into(),
        ),
        (
            project::Property::new("toolchain".into(), project::Type::String)
                .info("the rustup toolchain given with `+<toolchain>`, or empty if none was given")
                .read_only(),
            toolchain.unwrap_or_default().into(),
        ),
        (
            project::Property::new("cache/dir".into(), project::Type::String).info(
                "the directory of rig's cache, or `'off'` without a cache: `$XDG_CACHE_HOME/jig`, or failing that \
                 `~/.cache/jig`, by default, and on Windows `%LOCALAPPDATA%\\jig\\cache`, which `JIG_CACHE_DIR` \
                 overrides",
            ),
            cache_dir,
        ),
        (
            project::Property::new("cache/size".into(), project::Type::String).info(
                "the size rig's cache is collected down to, such as `'512M'` or `'10G'`, which `JIG_CACHE_SIZE` \
                 overrides",
            ),
            cache::DEFAULT_SIZE.into(),
        ),
        (
            project::Property::new("cache/remote".into(), project::Type::String).info(
                "the remote cache which rig's cache is in front of, as `'ssh://[<user>@]<host>[:<port>][/<jig>]'`, \
                 `'tcp://<host>:<port>'`, `'http://[<user>:<password>@]<host>[:<port>][/<path>]'` or a command which \
                 serves it on its stdin and stdout, or empty without one, which `JIG_CACHE_REMOTE` overrides",
            ),
            String::new(),
        ),
        (
            project::Property::new("cache/token-file".into(), project::Type::String).info(
                "the file of the token which proves rig to a remote cache or a worker which needs one, as \
                 `jig serve --listen` does, and which is sent as a bearer token over HTTP, or empty without one, \
                 which `JIG_CACHE_TOKEN_FILE` overrides",
            ),
            String::new(),
        ),
    ];
    let lists = [
        (
            project::Property::new("rustc/targets".into(), project::Type::List)
                .info("the builtin targets of rustc, as printed by `rustc --print=target-list`")
                .read_only(),
            project::Data::List(target_list),
        ),
        (
            project::Property::new("cache/push".into(), project::Type::Bool).info(
                "whether rig pushes what it builds to the remote cache, if it takes pushes, which \
                 `JIG_CACHE_PUSH=1` overrides",
            ),
            project::Data::Bool(false),
        ),
        (
            project::Property::new(cache::WORKERS.property.into(), project::Type::List).info(
                "the workers rig sends cached commands to once its own jobs are full, each as \
                 `'ssh://[<user>@]<host>[:<port>][/<jig>]'`, `'tcp://<host>:<port>'`, with the token of \
                 `cache/token-file`, or a command which serves one on its stdin and stdout, which `JIG_WORKERS_LIST`, \
                 separated by `;`, overrides",
            ),
            project::Data::List(Vec::new()),
        ),
        (
            project::Property::new(cache::WORKERS_ONLY.property.into(), project::Type::Bool).info(
                "whether what a worker may build waits for one, rather than being built here, which \
                 `JIG_WORKERS_ONLY=1` overrides, while what no worker builds, and what is not cached, is still built \
                 here",
            ),
            project::Data::Bool(false),
        ),
        (
            project::Property::new(cache::DOWNLOAD.property.into(), project::Type::String).info(
                "what is fetched of what workers build: `'all'`, or `'targets'`, which are what is asked for, as by \
                 default, installed or tested, and what is built here, while the rest is left on the workers, which \
                 `JIG_WORKERS_DOWNLOAD` overrides",
            ),
            project::Data::Str("all".into()),
        ),
        (
            project::Property::new("test-tags".into(), project::Type::List)
                .info("the tags of the tests `jig test` runs without names or tags, beside the tests without tags"),
            project::Data::List(Vec::new()),
        ),
    ];
    let builtins = builtins.into_iter().map(|(property, value)| (property, project::Data::Str(value))).chain(lists);
    for (property, value) in builtins {
        let name = property.name().to_string();
        let id = project.declare(name.clone(), Some(property));
        project.set(name, project::Value::new(id, value));
    }
    // the minimum supported Rust version has no value unless the project sets it
    let rust_version = project::Property::new("rust-version".into(), project::Type::String)
        .info("the minimum supported Rust version, such as `1.95.0`, which `use msrv` requires of rustc");
    project.declare("rust-version".into(), Some(rust_version));
    // nor do the tests have a timeout unless it sets one
    let test_timeout = project::Property::new("test-timeout".into(), project::Type::Any).info(
        "the timeout of the tests declared while it is set, in seconds, such as `60` or `'0.5'`, which their own \
         `timeout` replaces",
    );
    project.declare("test-timeout".into(), Some(test_timeout));
    project.declare_builtin_files();
    project.declare_codegen_options();
    project.declare_native_properties();
    project.declare_clippy_lints();
    project.declare_diagnostic_overrides();
}

/// When what jig shows is in colour, as `--color` gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Colour {
    /// Where it is shown on a terminal.
    #[default]
    Auto,
    Always,
    Never,
}

/// When what jig shows is in colour, which `--color` sets before the command runs.
static COLOUR: std::sync::OnceLock<Colour> = std::sync::OnceLock::new();

/// Take `--color <when>` or `--color=<when>` from the arguments, if it is next, which is `auto` without it.
fn parse_colour(args: &mut std::iter::Peekable<impl Iterator<Item = std::ffi::OsString>>) -> Result<Colour, Error> {
    let Some(arg) = args.next_if(|arg| arg == "--color" || arg.to_string_lossy().starts_with("--color=")) else {
        return Ok(Colour::Auto);
    };
    let arg = arg.to_string_lossy().into_owned();
    let when = match arg.strip_prefix("--color=") {
        Some(when) => when.to_string(),
        None => args.next().ok_or(Error::MissingArg("--color <when>"))?.to_string_lossy().into_owned(),
    };
    match when.as_str() {
        "auto" => Ok(Colour::Auto),
        "always" => Ok(Colour::Always),
        "never" => Ok(Colour::Never),
        _ => Err(Error::InvalidColour(when)),
    }
}

/// Whether what is written to a stream is in colour, as `--color` gives it: by default, where it is a terminal.
pub fn colour(stream: &impl IsTerminal) -> bool {
    match COLOUR.get().copied().unwrap_or_default() {
        Colour::Auto => stream.is_terminal(),
        Colour::Always => true,
        Colour::Never => false,
    }
}

/// Write the diagnostics of a project to stderr, failing if there are errors.
///
/// Warnings alone do not fail, but are followed by a warning which counts them.
fn report_diagnostics(project: &project::Project) -> Result<(), Error> {
    if project.diagnostics.is_empty() {
        return Ok(());
    }
    let mut errors = 0;
    let mut warnings = 0;
    let mut stderr = std::io::stderr().lock();
    let pretty = colour(&stderr);
    for diagnostic in &project.diagnostics {
        match diagnostic.severity {
            project::Severity::Fatal | project::Severity::Error => errors += 1,
            project::Severity::Warning => warnings += 1,
            project::Severity::Allow => continue,
            _ => (),
        }
        if let Err(error) = project.write_diagnostic(&mut stderr, diagnostic, pretty) {
            return Err(Error::Stderr(error));
        }
    }
    let result = Error::Diagnostics { errors, warnings };
    if errors > 0 {
        return Err(result);
    } else if warnings > 0 {
        let diagnostic = project::Diagnostic {
            span: None,
            message: project::Message::User(result.to_string()),
            severity: project::Severity::Warning,
        };
        if let Err(error) = project.write_diagnostic(&mut stderr, &diagnostic, pretty) {
            return Err(Error::Stderr(error));
        }
    }
    Ok(())
}

enum Error {
    MissingArg(&'static str),
    ExtraArg(String),
    UnknownCommand(String),
    Read(std::path::PathBuf, std::io::Error),
    Create(std::path::PathBuf, std::io::Error),
    Write(std::path::PathBuf, std::io::Error),
    Diagnostics {
        errors: usize,
        warnings: usize,
    },
    Stderr(std::io::Error),
    // the commands are boxed, as they are much larger than the other errors
    InvalidOutput(Box<std::process::Command>),
    TestBuild,
    TargetSpec(String, &'static str),
    /// The specification of a builtin target which a custom target inherits could not be read, as rustc reported.
    InheritedSpec {
        target: String,
        base: String,
        nightly: bool,
        stderr: String,
    },
    UnknownTest(String),
    /// An option which a command does not take, with the command.
    UnknownOption(&'static str, String),
    /// A `--timeout` which is not a number of seconds above zero.
    InvalidTimeout(String),
    /// A tag given to `jig test` which no test has.
    UnknownTag(String),
    TestsFailed(usize),
    /// A path which cannot be written to a depfile, as it has a line break.
    LineBreak(std::path::PathBuf, String),
    UnknownLint(String),
    UnknownDoc(String),
    /// `jig doc` without names, in a project which documents no crate.
    NoDocs,
    UnknownProgram(String, Vec<String>),
    /// A program to run was not named, though there are several, or a name is that of crates for several targets.
    ProgramNotChosen(Vec<String>),
    NoPrograms,
    LintFailed,
    LintWarnings,
    Exec(Box<std::process::Command>, std::io::Error),
    Install(std::path::PathBuf, std::path::PathBuf, std::io::Error),
    NotConfigured,
    Toolchain(String, String),
    BuildFailed,
    ProxyOutOfDate,
    /// A diagnostic reported once the project is configured was an error, as its property set it to be.
    Reported(String),
    ProxyNotInstalled(PathBuf),
    /// rig could not build, save or read a graph.
    Rig(rig::Error),
    /// A depfile rig could not read.
    Depfile(PathBuf, String),
    /// A value of `change-detection` which is not a way to detect changes.
    ChangeDetection(String),
    /// A size, of the cache, which is not one.
    InvalidSize(String),
    /// `jig cache` without a cache, as `JIG_CACHE_DIR` turns it off.
    CacheOff,
    UnknownCacheCommand(String),
    /// A remote cache, from `cache/remote` or its variable, which is not one.
    InvalidRemote(String),
    InvalidWorker(String),
    /// A `workers/download` which is not what is fetched of what workers build.
    InvalidDownload(String),
    InvalidJobs(String),
    /// A `-k` which is not a number of failures.
    InvalidFailures(String),
    /// A `--color` which is not when to show colour.
    InvalidColour(String),
    /// `jig serve` could not serve the cache.
    Serve(std::io::Error),
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::MissingArg(name) => write!(f, "missing required argument `{name}`\n\n{USAGE}"),
            Self::ExtraArg(arg) => write!(f, "unexpected argument {arg:?}\n\n{USAGE}"),
            Self::UnknownCommand(command) => write!(f, "unknown command {command:?}\n\n{USAGE}"),
            Self::Read(path, error) => write!(f, "unable to read {path:?}: {error}"),
            Self::Create(path, error) => write!(f, "failed to create {path:?}: {error}"),
            Self::Write(path, error) => write!(f, "failed to write to {path:?}: {error}"),
            Self::Diagnostics { errors: 0, warnings: 1 } => write!(f, "configuration raised 1 warning"),
            Self::Diagnostics { errors: 1, warnings: 0 } => write!(f, "configuration failed with 1 error"),
            Self::Diagnostics { errors: 0, warnings } => write!(f, "configuration raised {warnings} warnings"),
            Self::Diagnostics { errors, warnings: 0 } => write!(f, "configuration failed with {errors} errors"),
            Self::Diagnostics { errors, warnings } => {
                let error_suffix = if *errors > 1 { "s" } else { "" };
                let warning_suffix = if *warnings > 1 { "s" } else { "" };
                write!(f, "configuration failed with {errors} error{error_suffix}, {warnings} warning{warning_suffix}")
            },
            Self::Stderr(error) => write!(f, "failed to write to stderr: {error}"),
            Self::InvalidOutput(cmd) => write!(f, "invalid output from command {cmd:?}"),
            Self::TestBuild => write!(f, "failed to build the tests"),
            Self::TargetSpec(name, error) => write!(f, "failed to generate the specification of target {name:?}: {error}"),
            Self::InheritedSpec { target, base, nightly: true, stderr } => write!(
                f,
                "custom target {target:?} needs a nightly toolchain to read the specification of {base:?}, which it \
                 inherits, such as with `jig +nightly` or a `rust-toolchain.toml`, as rustc reported:\n{stderr}",
            ),
            Self::InheritedSpec { target, base, nightly: false, stderr } => write!(
                f,
                "failed to read the specification of {base:?}, which custom target {target:?} inherits, as rustc \
                 reported:\n{stderr}",
            ),
            Self::UnknownTest(name) => write!(f, "no test named {name:?}"),
            Self::UnknownOption(command, option) => write!(f, "`jig {command}` does not take {option:?}\n\n{USAGE}"),
            Self::InvalidTimeout(value) => write!(f, "`--timeout` is {value:?}, rather than a number of seconds"),
            Self::UnknownTag(tag) => write!(f, "no test tagged {tag:?}"),
            Self::TestsFailed(1) => write!(f, "1 test failed"),
            Self::TestsFailed(count) => write!(f, "{count} tests failed"),
            Self::LineBreak(path, text) => {
                write!(f, "cannot write {text:?} to {path:?}, as a depfile cannot have a line break")
            },
            Self::UnknownLint(name) => write!(f, "no crate to lint named {name:?}"),
            Self::UnknownDoc(name) => write!(f, "no crate to document named {name:?}, as only a crate with `doc` is"),
            Self::NoDocs => write!(f, "no crate to document, as none has `doc`"),
            Self::UnknownProgram(name, names) => write!(f, "no program named {name:?}, of {}", names.join(", ")),
            Self::ProgramNotChosen(names) => write!(f, "choose the program to run, of {}", names.join(", ")),
            Self::NoPrograms => write!(f, "no binary crate to run"),
            Self::LintFailed => write!(f, "linting failed"),
            Self::LintWarnings => write!(f, "linting raised warnings, which `-D` denies"),
            Self::Exec(cmd, error) => write!(f, "failed to execute {cmd:?}: {error}"),
            Self::Install(source, destination, error) => {
                write!(f, "failed to install {source:?} as {destination:?}: {error}")
            },
            Self::NotConfigured => write!(
                f,
                "no `{}` or `Jig` in the working directory or its parents, which the project root has",
                database::PATH,
            ),
            Self::Toolchain(name, message) => write!(f, "cannot use toolchain {name:?}: {message}"),
            Self::BuildFailed => write!(f, "the build failed"),
            Self::ProxyNotInstalled(wrapper) => write!(
                f,
                "no program is installed at {wrapper:?}",
            ),
            Self::Reported(id) => write!(f, "stopped, as `diagnostic/{id}` makes it an error"),
            Self::ProxyOutOfDate => write!(f, "not building or running the program"),
            Self::Rig(error) => write!(f, "rig: {error}"),
            Self::Depfile(path, error) => write!(f, "failed to read the depfile {path:?}: {error}"),
            Self::ChangeDetection(value) => {
                write!(f, "`change-detection` is {value:?}, rather than `'timestamp'` or `'hash'`")
            },
            Self::InvalidSize(size) => write!(f, "{size:?} is not a size, such as `512M` or `10G`"),
            Self::CacheOff => {
                write!(
                    f,
                    "the cache is off, as `cache/dir` or `JIG_CACHE_DIR` is empty or `off`, or neither \
                     `XDG_CACHE_HOME` nor `HOME` is set",
                )
            },
            Self::InvalidRemote(remote) => {
                write!(
                    f,
                    "the remote cache is {remote:?}, rather than `ssh://[<user>@]<host>[:<port>][/<jig>]`, \
                     `tcp://<host>:<port>`, `http://[<user>:<password>@]<host>[:<port>][/<path>]` or a command, as \
                     `https://` is not spoken, which a proxy which speaks TLS can be put in front of",
                )
            },
            Self::InvalidDownload(download) => {
                write!(f, "{download:?} is not what is fetched of what workers build, `all` or `targets`")
            },
            Self::InvalidWorker(worker) => {
                write!(
                    f,
                    "the worker {worker:?} is not `ssh://[<user>@]<host>[:<port>][/<jig>]`, `tcp://<host>:<port>` or a \
                     command",
                )
            },
            Self::InvalidJobs(jobs) => write!(f, "{jobs:?} is not a number of jobs"),
            Self::InvalidFailures(failures) => write!(f, "{failures:?} is not a number of failures"),
            Self::Serve(error) => write!(f, "failed to serve the cache: {error}"),
            Self::InvalidColour(when) => write!(f, "`--color` is {when:?}, rather than `auto`, `always` or `never`"),
            Self::UnknownCacheCommand(command) => {
                write!(f, "unknown cache command {command:?}, rather than `stats`, `gc` or `clear`")
            },
        }
    }
}

/// Returns what identifies a toolchain, as [`toolchain::identity`] finds it, which the configuration depends on the
/// files of, so that the toolchain is identified again when it is updated in place.
fn toolchain_identity(project: &mut project::Project, verbose_version: &str, sysroot: &str) -> String {
    let (identity, file) = toolchain::identity(verbose_version, Path::new(sysroot));
    // a file which is missing identifies nothing, so it is not depended on
    if let Some(file) = file {
        project.depend(file);
    }
    identity
}



#[cfg(test)]
mod tests {
    use super::*;

    fn parse(command: &str, args: &[&str]) -> Result<Args, Error> {
        Args::parse(command, args.iter().map(std::ffi::OsString::from))
    }

    #[track_caller]
    fn ok(command: &str, args: &[&str]) -> Args {
        parse(command, args).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn parses_options_anywhere() {
        let args = ok("build", &["a", "-j", "4", "-k0", "-v", "--quiet", "b", "-n", "--", "-c"]);
        let build = args.build().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!((build.jobs, build.keep_going), (Some(4), Some(0)));
        assert!(build.verbose && build.quiet && build.dry_run);
        // what follows `--` is an operand, though it looks like an option
        assert_eq!(args.names(), ["a", "b", "-c"]);
        // long forms, with their values in the same argument or the next, and `-q` for `--quiet`
        let build = ok("build", &["--jobs=2", "--keep-going", "3", "-q"]).build().unwrap_or_else(|error| panic!("{error}"));
        assert_eq!((build.jobs, build.keep_going, build.quiet), (Some(2), Some(3), true));
        // after a name, as `jig lint rig -D` is
        let args = ok("lint", &["rig", "-D"]);
        assert!(args.flag("deny-warnings"));
        assert_eq!(args.names(), ["rig"]);
        // `-` alone is an operand
        assert_eq!(ok("graph", &["-"]).names(), ["-"]);
    }

    #[test]
    fn rejects_what_a_command_does_not_take() {
        let unknown = |command, args: &[&str], expected: &str| {
            let error = parse(command, args).err().map(|error| error.to_string()).unwrap_or_default();
            assert!(error.starts_with(&format!("`jig {command}` does not take {expected:?}")), "{error}");
        };
        unknown("install", &["--bogus"], "--bogus");
        unknown("lint", &["-n"], "-n");
        unknown("graph", &["-v"], "-v");
        unknown("generate", &["-j", "2"], "-j");
        // a value given to an option without one, or short options together
        unknown("build", &["--quiet=yes"], "--quiet=yes");
        unknown("build", &["-vq"], "-vq");
        assert!(matches!(parse("install", &["a", "b"]), Err(Error::ExtraArg(extra)) if extra == "b"));
        assert!(matches!(parse("workers", &["a"]), Err(Error::ExtraArg(_))));
        assert!(matches!(parse("build", &["-j"]), Err(Error::MissingArg("jobs"))));
        assert!(matches!(parse("nope", &[]), Err(Error::UnknownCommand(_))));
        assert!(matches!(ok("build", &["-j0"]).build(), Err(Error::InvalidJobs(_))));
        assert!(matches!(ok("build", &["-k", "x"]).build(), Err(Error::InvalidFailures(_))));
    }

    #[test]
    fn passes_arguments_on() {
        // what follows the name of the program is its, as is what follows `--`
        let args = ok("run", &["-q", "app", "-q", "--", "x"]);
        assert_eq!((args.names(), args.flag("quiet")), (vec!["app".into()], true));
        assert_eq!(args.passed, ["-q", "--", "x"]);
        assert_eq!(ok("run", &["app", "--", "-x"]).passed, ["-x"]);
        let args = ok("run", &["--", "--help"]);
        assert!(args.operands.is_empty() && !args.help);
        assert_eq!(args.passed, ["--help"]);
        // while a test's names may follow its options
        let args = ok("test", &["a", "-j", "2", "b", "--", "c"]);
        assert_eq!((args.names(), args.passed), (vec!["a".into(), "b".into()], vec!["c".into()]));
    }

    #[test]
    fn takes_help_everywhere() {
        for command in SPECS.iter().map(|spec| spec.name) {
            assert!(ok(command, &["--help"]).help && ok(command, &["-h"]).help, "{command}");
        }
        assert!(ok("install", &["stage", "-h"]).help);
    }
}
