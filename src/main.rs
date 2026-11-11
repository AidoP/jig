use std::{io::IsTerminal, path::{Path, PathBuf}};

#[cfg(test)]
mod bench;
mod builder;
mod cache;
mod config;
mod check;
mod configured;
mod database;
#[cfg(test)]
mod differential;
mod generate;
mod graph;
mod install;
mod lint;
mod ninja;
mod project;
mod rig_builder;
mod rig_graph;
mod platform;
mod run;
mod rust_analyzer;
mod show_graph;
mod target_spec;
mod test_runner;
mod timings;
mod toolchain;

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

commands:
    generate [<input>]   evaluate the configuration, `Jig` by default, and generate rig's graph in the build
                         directory, or `build.ninja` if `JIG_ENGINE` is `ninja`
    build [<arg> ...]    build the project, or the targets given, with rig, or with ninja if `JIG_ENGINE` is
                         `ninja`, which is given the arguments
    test [-v] [--all] [--tag <tag> ...] [<name> ...]
                         build and run the tests without tags and those with a tag in `test-tags`, or only those named
                         or tagged with `--tag`, or all of them with `--all`, where `-v` or `--verbose` shows the
                         output of each test, such as the tests of a crate's test harness, rather than only those which
                         fail
    run [<name>] [--] [<argument> ...]
                         build a binary crate and run it with arguments, by its target's runner if it has one, where
                         a crate for a target other than the host is `<name>@<target>`, unless it is the only one with
                         the name, and without a name, the only binary crate is run
    lint [-D] [<name> ...]
                         lint the crates which are edited with clippy, or only those named, as the `clippy/<lint>`
                         properties set where they are declared give the levels of the lints, where `-D` or
                         `--deny-warnings` fails if there is a warning
    cache stats|gc [--max-size <size>]|clear
                         show what rig's cache holds, remove what was used longest ago until it is no larger than a
                         size, `JIG_CACHE_SIZE` or 10 GiB by default, as is done after a build once a day, or remove
                         everything in it
    serve --stdio [--push] | serve --listen <address> --token-file <path> [--push]
                         serve rig's cache on stdin and stdout as a remote cache, as `cache/remote` runs it by ssh, or
                         over TCP at an address, such as `0.0.0.0:7070`, to clients with the token in the file,
                         taking pushes with `--push`
    worker (--stdio | --listen <address> --token-file <path>) [--jobs <n>] [--tag <tag> ...] [--install]
                         run the cached commands of builds which send them, as the `workers` property runs it by ssh,
                         on stdin and stdout, or over TCP at an address, for clients with the token in the file, with
                         rig's cache as its store, and the toolchains rustup has, running as many at once as `--jobs`,
                         reporting its tags, and with `--install`, installing a toolchain a command needs with rustup
    workers              show the workers of the `workers` property, with their hosts, tags and toolchains
    graph [--json] [<output> ...]
                         show rig's graph of the project, once it is up to date: each command, with its outputs,
                         inputs and command line, as ninja's `build` statements are written, or as JSON with `--json`,
                         or only those of the outputs given
    install [--proxy] [<prefix>]
                         build and install the files with `install` in a prefix, the `install-prefix` property by
                         default, or with `--proxy`, install wrappers which build and run programs, and links to
                         other files
    help                 show this message";

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
    // the other commands use the toolchain the project is configured with, which is changed first if another is given
    if let Some(toolchain) = &toolchain && !matches!(command.as_ref(), "generate" | "help") {
        configured::ensure(Some(toolchain))?;
    }
    match command.as_ref() {
        "generate" => {
            let input = args.next().map_or_else(|| PathBuf::from("Jig"), PathBuf::from);
            if let Some(extra) = args.next() {
                return Err(Error::ExtraArg(extra.to_string_lossy().into_owned()));
            }
            timings::step("configure", || generate(input, toolchain.as_deref())).map(|()| ExitCode::SUCCESS)
        },
        "build" => {
            let mut args = args.peekable();
            if args.next_if(|arg| arg == "--proxy").is_some() {
                let (Some(prefix), Some(wrapper)) = (args.next(), args.next()) else {
                    return Err(Error::MissingArg("wrapper"));
                };
                if let Some(extra) = args.next() {
                    return Err(Error::ExtraArg(extra.to_string_lossy().into_owned()));
                }
                let reinstalled = install::build_proxy(prefix.as_ref(), wrapper.as_ref())?;
                return Ok(if reinstalled { ExitCode::from(install::REINSTALLED) } else { ExitCode::SUCCESS });
            }
            let project = configured::Configured::find(configured::Output::Stdout)?;
            let args: Vec<_> = args.collect();
            project.build(&builder::Request { args: &args, ..Default::default() }).map(|()| ExitCode::SUCCESS)
        },
        "check" => {
            let Some(label) = args.next() else {
                return Err(Error::MissingArg("label"));
            };
            if let Some(extra) = args.next() {
                return Err(Error::ExtraArg(extra.to_string_lossy().into_owned()));
            }
            check::check(&label.to_string_lossy());
            Ok(ExitCode::SUCCESS)
        },
        "test" => {
            let mut selection = test_runner::Selection::default();
            let mut verbose = false;
            let mut args = args.map(|arg| arg.to_string_lossy().into_owned());
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-v" | "--verbose" => verbose = true,
                    "--all" => selection.all = true,
                    "--tag" => selection.tags.push(args.next().ok_or(Error::MissingArg("tag"))?),
                    _ => selection.names.push(arg),
                }
            }
            test_runner::test(&selection, verbose).map(|()| ExitCode::SUCCESS)
        },
        "run" => {
            let mut args = args.peekable();
            // a name before `--`, which the arguments of the program follow, so that they may start with `-`
            let name = args.next_if(|arg| arg != "--").map(|name| name.to_string_lossy().into_owned());
            args.next_if(|arg| arg == "--");
            let args: Vec<_> = args.collect();
            run::run(name.as_deref(), &args)
        },
        "cache" => cache::command(args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "serve" => cache::serve(args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "worker" => cache::worker(args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "workers" => cache::workers(args, &user_settings(toolchain.as_deref())?).map(|()| ExitCode::SUCCESS),
        "graph" => show_graph::show(args).map(|()| ExitCode::SUCCESS),
        "lint" => {
            let mut args = args.peekable();
            let deny_warnings = args.next_if(|arg| arg == "-D" || arg == "--deny-warnings").is_some();
            let names: Vec<_> = args.map(|name| name.to_string_lossy().into_owned()).collect();
            lint::lint(&names, deny_warnings).map(|()| ExitCode::SUCCESS)
        },
        "install" => {
            let mut proxy = false;
            let mut prefix = None;
            for arg in args {
                if arg == "--proxy" {
                    proxy = true;
                } else if prefix.is_none() {
                    prefix = Some(PathBuf::from(arg));
                } else {
                    return Err(Error::ExtraArg(arg.to_string_lossy().into_owned()));
                }
            }
            install::install(prefix, proxy).map(|()| ExitCode::SUCCESS)
        },
        "help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        },
        command => Err(Error::UnknownCommand(command.into())),
    }
}

/// Evaluate the configuration from its root file, `input`, then generate the graph and the files beside it, with a
/// rustup toolchain if one is given.
fn generate(input: PathBuf, toolchain: Option<&str>) -> Result<(), Error> {
    let sysroot = rustc_print("sysroot", toolchain)?;
    let host = rustc_print("host-tuple", toolchain)?;
    let targets: Vec<String> = rustc_print("target-list", toolchain)?.lines().map(String::from).collect();
    let verbose_version = rustc_output("-vV", toolchain)?;
    let version = rustc_version(&verbose_version)?;

    let mut project = project::Project::new(input.parent().map(Path::to_path_buf).unwrap_or_default(), targets.clone());
    project.toolchain = toolchain.map(String::from);
    project.line_breaks = configured::Engine::from_env()? == configured::Engine::Rig;
    // which toolchain rustup's proxies run, as rustc was asked of above, so that the project is configured again when it
    // changes, and the toolchain is identified afresh
    project.read_env("RUSTUP_TOOLCHAIN");
    let file = match project::File::read(input.clone()) {
        Ok(file) => file,
        Err(error) => return Err(Error::Read(input, error)),
    };
    declare_builtins(&mut project, &sysroot, &host, version, &targets, toolchain);
    // the user's configuration, as if the project began by using it
    config::apply(&mut project)?;
    project.parse(file);
    // a fatal diagnostic stops the configuration, so what is after it, which the check is about, is not known
    if !project.fatal {
        project.check();
    }
    report_diagnostics(&project)?;

    // rig caches when there is a cache, which configures the project again when it is turned on or off, as the variables
    // which override the configuration are read by it
    let mut settings = cache::Settings::resolve(&project.cache_properties(), |name| project.read_env(name))?;
    if settings.dir.is_some() {
        let identity = toolchain_identity(&mut project, &verbose_version, &sysroot);
        // a worker may run what does not depend on the host with the same release on another, and install it by its
        // name, if it does not have it
        let release = rig::worker::Release {
            identity: toolchain::release_identity(&verbose_version, Path::new(&sysroot)),
            install: toolchain::install_name(Path::new(&sysroot)).unwrap_or_default(),
        };
        settings.release = Some((identity.clone(), release));
        project.cache = Some(identity);
        project.identify_cached_customs();
    }
    project.cache_settings = settings;
    generate::write(project, &input, &sysroot)
}

/// Returns how rig caches outside a project, as for `jig cache` and `jig serve`, from the user's configuration alone,
/// and the variables which override it.
///
/// The builtin properties have rustc's values, if it can be run, or are empty, so that a server without Rust can serve
/// a cache.
fn user_settings(toolchain: Option<&str>) -> Result<cache::Settings, Error> {
    let print = |request| rustc_print(request, toolchain).unwrap_or_default();
    // rustc is asked each at once, as a worker, which runs this, answers once it has
    let (sysroot, host, targets, version) = std::thread::scope(|scope| {
        let (sysroot, host) = (scope.spawn(|| print("sysroot")), scope.spawn(|| print("host-tuple")));
        let targets = scope.spawn(|| print("target-list"));
        let version = rustc_output("-vV", toolchain).and_then(|output| rustc_version(&output)).unwrap_or_default();
        let joined = |printed: std::thread::ScopedJoinHandle<String>| printed.join().unwrap_or_default();
        (joined(sysroot), joined(host), joined(targets), version)
    });
    let targets: Vec<String> = targets.lines().map(String::from).collect();
    let mut project = project::Project::new(std::env::current_dir().unwrap_or_default(), targets.clone());
    project.toolchain = toolchain.map(String::from);
    declare_builtins(&mut project, &sysroot, &host, version, &targets, toolchain);
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
                "the directory of rig's cache, `$XDG_CACHE_HOME/jig` or `~/.cache/jig` by default, or `'off'` without \
                 a cache, which `JIG_CACHE` overrides",
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
                "the remote cache which rig's cache is in front of, as `'ssh://[<user>@]<host>[:<port>][/<jig>]'` or \
                 a command which serves it on its stdin and stdout, or empty without one, which `JIG_REMOTE_CACHE` \
                 overrides",
            ),
            String::new(),
        ),
        (
            project::Property::new("cache/token-file".into(), project::Type::String).info(
                "the file of the token which proves rig to a remote cache which needs one, as `jig serve --listen` \
                 does, or empty without one",
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
                 `JIG_REMOTE_CACHE_PUSH=1` overrides",
            ),
            project::Data::Bool(false),
        ),
        (
            project::Property::new("workers".into(), project::Type::List).info(
                "the workers rig sends cached commands to once its own jobs are full, each as \
                 `'ssh://[<user>@]<host>[:<port>][/<jig>]'`, `'tcp://<host>:<port>'`, with the token of \
                 `cache/token-file`, or a command which serves one on its stdin and stdout, which `JIG_WORKERS`, \
                 separated by `;`, overrides",
            ),
            project::Data::List(Vec::new()),
        ),
        (
            project::Property::new("workers-only".into(), project::Type::Bool).info(
                "whether what a worker may build waits for one, rather than being built here, which \
                 `JIG_WORKERS_ONLY=1` overrides, while what no worker builds, and what is not cached, is still built \
                 here",
            ),
            project::Data::Bool(false),
        ),
        (
            project::Property::new("download".into(), project::Type::String).info(
                "what is fetched of what workers build: `'all'`, or `'targets'`, which are what is asked for, as by \
                 default, installed or tested, and what is built here, while the rest is left on the workers, which \
                 `JIG_DOWNLOAD` overrides",
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
    project.declare_builtin_files();
    project.declare_codegen_options();
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
    /// A tag given to `jig test` which no test has.
    UnknownTag(String),
    TestsFailed(usize),
    /// Text which cannot be written to a file of ninja's, as it has a line break.
    LineBreak(std::path::PathBuf, String),
    UnknownLint(String),
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
    /// An argument of `jig build` which rig does not take, or a depfile it could not read.
    RigArgument(String),
    /// A value of `JIG_ENGINE` which is not an engine.
    UnknownEngine(String),
    /// `JIG_ENGINE=ninja` on Windows, where only rig builds.
    NinjaOnWindows,
    /// `jig graph` with `JIG_ENGINE=ninja`, which has no graph of rig's.
    GraphOfNinja,
    /// A value of `change-detection` which is not a way to detect changes.
    ChangeDetection(String),
    /// A size, of the cache, which is not one.
    InvalidSize(String),
    /// `jig cache` without a cache, as `JIG_CACHE` turns it off.
    CacheOff,
    UnknownCacheCommand(String),
    /// A `JIG_REMOTE_CACHE` which is not a remote cache.
    InvalidRemote(String),
    InvalidWorker(String),
    /// A `download` which is not what is fetched of what workers build.
    InvalidDownload(String),
    InvalidJobs(String),
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
            Self::UnknownTag(tag) => write!(f, "no test tagged {tag:?}"),
            Self::TestsFailed(1) => write!(f, "1 test failed"),
            Self::TestsFailed(count) => write!(f, "{count} tests failed"),
            Self::LineBreak(path, text) => write!(f, "cannot write {text:?} to {path:?}, as ninja cannot write a line break"),
            Self::UnknownLint(name) => write!(f, "no crate to lint named {name:?}"),
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
            Self::RigArgument(argument) => write!(f, "rig does not take {argument:?}"),
            Self::ChangeDetection(value) => {
                write!(f, "`change-detection` is {value:?}, rather than `'timestamp'` or `'hash'`")
            },
            Self::UnknownEngine(engine) => write!(f, "unknown engine {engine:?} in `JIG_ENGINE`, rather than `ninja` or `rig`"),
            Self::NinjaOnWindows => write!(f, "`JIG_ENGINE=ninja` is not supported on Windows, where rig builds"),
            Self::GraphOfNinja => {
                write!(f, "`jig graph` shows rig's graph, which is not generated with `JIG_ENGINE=ninja`")
            },
            Self::InvalidSize(size) => write!(f, "{size:?} is not a size, such as `512M` or `10G`"),
            Self::CacheOff => {
                write!(f, "the cache is off, as `JIG_CACHE` is empty or `off`, or neither `XDG_CACHE_HOME` nor `HOME` is set")
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
            Self::InvalidJobs(jobs) => write!(f, "`--jobs` is {jobs:?}, rather than a number of jobs"),
            Self::Serve(error) => write!(f, "failed to serve the cache: {error}"),
            Self::InvalidColour(when) => write!(f, "`--color` is {when:?}, rather than `auto`, `always` or `never`"),
            Self::UnknownCacheCommand(command) => {
                write!(f, "unknown cache command {command:?}, rather than `stats`, `gc` or `clear`")
            },
        }
    }
}

/// Returns the trimmed output of `rustc --print=<request>`, with a toolchain if one is given.
fn rustc_print(request: &str, toolchain: Option<&str>) -> Result<String, Error> {
    rustc_output(&format!("--print={request}"), toolchain)
}

/// Returns what identifies a toolchain, as [`toolchain::identity`] finds it, which the configuration depends on the
/// files of, so that the toolchain is identified again when it is updated in place.
fn toolchain_identity(project: &mut project::Project, verbose_version: &str, sysroot: &str) -> String {
    let (identity, file) = toolchain::identity(verbose_version, Path::new(sysroot));
    // a file which is missing is not depended on, as ninja would configure the project again each time
    if let Some(file) = file {
        project.depend(file);
    }
    identity
}

/// Returns the version of rustc, from the `release` line of the output of `rustc -vV`.
fn rustc_version(output: &str) -> Result<String, Error> {
    let version = output.lines().find_map(|line| line.strip_prefix("release: "));
    version.map(|version| version.trim().into()).ok_or_else(|| {
        let mut cmd = std::process::Command::new("rustc");
        cmd.arg("-vV");
        Error::InvalidOutput(Box::new(cmd))
    })
}

/// Returns the trimmed output of rustc with an argument, with a toolchain if one is given.
fn rustc_output(argument: &str, toolchain: Option<&str>) -> Result<String, Error> {
    use std::process::Command;
    let mut cmd = Command::new("rustc");
    cmd.arg(argument);
    toolchain::apply(&mut cmd, toolchain);
    let output = match cmd.output() {
        Err(error) => {
            return Err(Error::Exec(Box::new(cmd), error));
        },
        Ok(output) => output,
    };
    if !output.status.success() {
        return Err(Error::InvalidOutput(Box::new(cmd)));
    }
    match String::from_utf8(output.stdout) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().into()),
        _ => Err(Error::InvalidOutput(Box::new(cmd))),
    }
}
