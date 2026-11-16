//! rig's cache, which holds the results of rustc, clippy and the custom commands with `cache`, and `jig cache`,
//! which shows, collects and clears it.
//!
//! The cache is configured by builtin properties, which the user's configuration ([`crate::config`]) usually sets.
//! An environment variable overrides each.
//!
//! - `cache/dir`, or `JIG_CACHE`: the directory of the cache. `off`, or an empty value, turns the cache off. The
//!   default is `$XDG_CACHE_HOME/jig`, or failing that `$HOME/.cache/jig`, and on Windows `%LOCALAPPDATA%\jig\cache`.
//! - `cache/size`, or `JIG_CACHE_SIZE`: the size the cache is collected down to, which happens after a build, at most
//!   once a day. `10G` by default.
//! - `cache/remote`, or `JIG_REMOTE_CACHE`: the remote cache behind this one, if any. It is one of:
//!   - `ssh://[<user>@]<host>[:<port>][/<jig>]`, which runs `jig serve --stdio` on the host with ssh;
//!   - `tcp://<host>:<port>`, which `jig serve --listen` serves;
//!   - `http://[<user>:<password>@]<host>[:<port>][/<path>]`, a server of files over HTTP, laid out as a cache's
//!     directory is;
//!   - a command, split at spaces, which serves the cache on its stdin and stdout.
//! - `cache/push`, or `JIG_REMOTE_CACHE_PUSH=1`: whether results are pushed to the remote cache, if it takes them.
//! - `cache/token-file`: the file holding the token for a remote cache which needs one. The token proves the client
//!   to a server reached over TCP, and is sent as a bearer token over HTTP. The file is read when connecting, so the
//!   token itself is never recorded with the project.
//! - `workers`, or `JIG_WORKERS`, in which workers are separated by `;`: the workers rig sends cached edges to once
//!   its own jobs are full. Each is one of:
//!   - `ssh://[<user>@]<host>[:<port>][/<jig>]`, which runs `jig worker --stdio` on the host with ssh;
//!   - `tcp://<host>:<port>`, which `jig worker --listen` serves, using the token of `cache/token-file`;
//!   - a command, split at spaces, which serves a worker on its stdin and stdout.
//! - `workers-only`, or `JIG_WORKERS_ONLY=1`: whether what a worker may build waits for a worker, rather than being
//!   built here.
//! - `download`, or `JIG_DOWNLOAD`: how much of what workers build is fetched. `'all'` fetches everything.
//!   `'targets'` fetches only what was asked for, which includes the defaults and what is installed or tested, and
//!   what edges built here read. The rest stays on the workers.
//!
//! The settings are resolved when the project is configured, as the configuration reads the variables, and are
//! recorded in the project's database, where a build reads them.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::Error;

/// The environment variable which gives the directory of the cache, or turns it off.
pub const DIR: &str = "JIG_CACHE";
/// The environment variable which gives the size the cache is collected down to.
pub const SIZE: &str = "JIG_CACHE_SIZE";
/// The environment variable which gives the remote cache.
pub const REMOTE: &str = "JIG_REMOTE_CACHE";
/// The environment variable which pushes results to the remote cache, if it is `1`.
pub const PUSH: &str = "JIG_REMOTE_CACHE_PUSH";
/// The environment variable which gives the workers, separated by `;`.
pub const WORKERS: &str = "JIG_WORKERS";
/// The environment variable which, if it is `1`, makes what a worker may build wait for a worker.
pub const WORKERS_ONLY: &str = "JIG_WORKERS_ONLY";
/// The environment variable which gives how much of what workers build is fetched, `all` or `targets`.
pub const DOWNLOAD: &str = "JIG_DOWNLOAD";
/// The size the cache is collected down to by default.
pub const DEFAULT_SIZE: &str = "10G";
/// How often the cache is collected after a build.
const EVERY: Duration = Duration::from_hours(24);

/// Returns the default directory of the cache, from environment variables as `env` gives them:
/// `$XDG_CACHE_HOME/jig`, or failing that `$HOME/.cache/jig`. On Windows, where a user's local data goes in
/// `%LOCALAPPDATA%`, it is `%LOCALAPPDATA%\jig\cache`. It is empty, which turns the cache off, if none is set.
pub fn default_dir(mut env: impl FnMut(&str) -> Option<OsString>) -> String {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    let local = |env: &mut dyn FnMut(&str) -> Option<OsString>| set(env("LOCALAPPDATA")).filter(|_| cfg!(windows));
    let dir = match set(env("XDG_CACHE_HOME")) {
        Some(cache) => PathBuf::from(cache).join("jig"),
        None if let Some(local) = local(&mut env) => PathBuf::from(local).join("jig").join("cache"),
        None => match set(env("HOME")) {
            Some(home) => PathBuf::from(home).join(".cache/jig"),
            None => return String::new(),
        },
    };
    dir.display().to_string()
}

/// The values of the cache's properties, as the configuration left them.
pub struct Properties {
    pub dir: String,
    pub size: String,
    pub remote: String,
    pub push: bool,
    pub token_file: String,
    pub workers: Vec<String>,
    pub workers_only: bool,
    pub download: String,
}

/// How rig caches, from the cache's properties and the variables which override them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The directory of the cache, absolute, or `None` if it is off.
    pub dir: Option<PathBuf>,
    /// The size it is collected down to, in bytes.
    pub size: u64,
    /// The remote cache, as `cache/remote` gives it, if there is one.
    pub remote: Option<String>,
    pub push: bool,
    /// The absolute path of the file of the token for the remote cache, if there is one. The same token proves the
    /// client to its workers.
    pub token_file: Option<PathBuf>,
    /// The workers, as `workers` gives them.
    pub workers: Vec<String>,
    /// Whether what a worker may build waits for one, rather than being built here.
    pub workers_only: bool,
    /// Whether, of what workers build, only the targets and what edges built here read are fetched, rather than all.
    pub download_targets: bool,
    /// The identity of the project's toolchain, and its release: the release's identity on any host, and the name
    /// rustup installs it by, if known. Configuring finds them.
    pub release: Option<(String, rig::worker::Release)>,
}

impl Settings {
    /// Resolve the settings from the cache's properties, each overridden by its variable, as `env` gives it.
    pub fn resolve(properties: &Properties, mut env: impl FnMut(&str) -> Option<OsString>) -> Result<Self, Error> {
        let mut string = |name: &str, property: &str| match env(name) {
            Some(value) => value.to_string_lossy().into_owned(),
            None => property.to_string(),
        };
        let dir = string(DIR, &properties.dir);
        let size = string(SIZE, &properties.size);
        let remote = string(REMOTE, &properties.remote);
        let push = match env(PUSH) {
            Some(push) => push == "1",
            None => properties.push,
        };
        let dir = (!dir.is_empty() && dir != "off").then(|| std::path::absolute(&dir).unwrap_or_else(|_| dir.into()));
        let size = parse_size(&size).ok_or(Error::InvalidSize(size))?;
        let remote = (!remote.trim().is_empty()).then_some(remote);
        if let Some(remote) = &remote
            && remote_transport(remote, push).is_none()
        {
            return Err(Error::InvalidRemote(remote.clone()));
        }
        let token_file = &properties.token_file;
        let token_file = (!token_file.is_empty()).then(|| std::path::absolute(token_file).unwrap_or_else(|_| token_file.into()));
        let workers = match env(WORKERS) {
            Some(workers) => workers.to_string_lossy().split(';').map(String::from).collect(),
            None => properties.workers.clone(),
        };
        let workers = workers.iter().map(|worker| worker.trim()).filter(|worker| !worker.is_empty());
        let workers: Vec<String> = workers.map(String::from).collect();
        if let Some(worker) = workers.iter().find(|worker| worker_transport(worker).is_none()) {
            return Err(Error::InvalidWorker(worker.clone()));
        }
        let workers_only = match env(WORKERS_ONLY) {
            Some(only) => only == "1",
            None => properties.workers_only,
        };
        let download = match env(DOWNLOAD) {
            Some(download) => download.to_string_lossy().into_owned(),
            None => properties.download.clone(),
        };
        let download_targets = match download.as_str() {
            "" | "all" => false,
            "targets" => true,
            _ => return Err(Error::InvalidDownload(download)),
        };
        Ok(Self { dir, size, remote, push, token_file, workers, workers_only, download_targets, release: None })
    }
    /// Opens the cache, if it is on. With `remote`, the remote cache is put behind it, if there is one.
    pub fn open(&self, remote: bool) -> Result<Option<rig::cache::Cache>, Error> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        let cache = rig::cache::Cache::open(dir).map_err(Error::Rig)?;
        let transport = self.remote.as_deref().filter(|_| remote).and_then(|remote| remote_transport(remote, self.push));
        let mut remote = match transport {
            Some(Transport::Command(command)) => rig::remote::Remote::command(command, self.push),
            Some(Transport::Tcp(address)) => rig::remote::Remote::tcp(address, self.push),
            Some(Transport::Http(url)) => {
                let token = self.token_file.as_deref().map(read_token).transpose()?;
                rig::remote::Remote::http(url, token.as_deref(), self.push)
            },
            None => return Ok(Some(cache)),
        };
        if let Some(path) = &self.token_file {
            remote = remote.with_token(read_token(path)?);
        }
        Ok(Some(cache.with_remote(std::sync::Arc::new(remote))))
    }
    /// Returns the workers. Each is connected to when a build first sends it an edge. Over TCP, the client proves
    /// itself with the token of `cache/token-file`, if there is one.
    pub fn workers(&self) -> Result<Vec<std::sync::Arc<rig::remote::Remote>>, Error> {
        let token = match &self.token_file {
            Some(path) if self.workers.iter().any(|worker| worker.starts_with("tcp://")) => Some(read_token(path)?),
            _ => None,
        };
        let workers = self.workers.iter().filter_map(|worker| match worker_transport(worker)? {
            Transport::Command(command) => Some(rig::remote::Remote::command(command, false)),
            Transport::Tcp(address) => {
                let remote = rig::remote::Remote::tcp(address, false);
                Some(match &token {
                    Some(token) => remote.with_token(token.clone()),
                    None => remote,
                })
            },
            Transport::Http(_) => None,
        });
        Ok(workers.map(std::sync::Arc::new).collect())
    }
    /// Collect the cache after a build, if it was last collected a day ago or longer.
    pub fn collect_if_due(&self, cache: &rig::cache::Cache) {
        let _ = cache.gc_if_due(self.size, EVERY);
    }
}

/// How a remote cache is reached.
#[derive(Debug, PartialEq, Eq)]
pub enum Transport {
    /// A command which serves it on its stdin and stdout.
    Command(Vec<String>),
    /// A server which listens at `<host>:<port>` over TCP.
    Tcp(String),
    /// A server of files over HTTP, under a URL.
    Http(rig::http::Url),
}

/// Returns how a remote cache is reached, from the value of `cache/remote`: over TCP for `tcp://<host>:<port>`,
/// over HTTP for `http://...`, and otherwise by the command [`remote_command`] gives. Returns `None` if the value
/// is none of those. `push` is whether results are pushed to it.
pub fn remote_transport(remote: &str, push: bool) -> Option<Transport> {
    if remote.starts_with("http://") {
        return rig::http::Url::parse(remote).map(Transport::Http);
    }
    // rig does not speak TLS: a proxy must do that for it
    if remote.starts_with("https://") {
        return None;
    }
    let Some(address) = remote.strip_prefix("tcp://") else {
        return remote_command(remote, push).map(Transport::Command);
    };
    let (host, port) = address.rsplit_once(':')?;
    (!host.is_empty() && port.parse::<u16>().is_ok()).then(|| Transport::Tcp(address.to_string()))
}

/// Returns how a worker is reached, from an item of `workers`: over TCP for `tcp://<host>:<port>`, and otherwise by
/// the command [`worker_command`] gives. Returns `None` if it is neither. A server of files over HTTP is not a
/// worker, as it runs nothing.
pub fn worker_transport(worker: &str) -> Option<Transport> {
    match remote_transport(worker, false)? {
        Transport::Command(_) => worker_command(worker).map(Transport::Command),
        Transport::Http(_) => None,
        tcp @ Transport::Tcp(_) => Some(tcp),
    }
}

/// Returns the token in a file, without the whitespace around it, such as the line break at its end.
fn read_token(path: &std::path::Path) -> Result<Vec<u8>, Error> {
    let token = std::fs::read(path).map_err(|error| Error::Read(path.into(), error))?;
    Ok(token.trim_ascii().to_vec())
}

/// Returns the command which serves a remote cache, from the value of `JIG_REMOTE_CACHE`. Returns `None` if the
/// value is empty, or is an `ssh://` address without a host. `push` is whether results are pushed to it.
///
/// An `ssh://` address runs `jig`, or the path given after the host, with `serve --stdio`, and with `--push` to
/// push. ssh runs in batch mode, so that it fails rather than asking for a password. Any other value is a command,
/// which is split at spaces and run as it is.
pub fn remote_command(remote: &str, push: bool) -> Option<Vec<String>> {
    let args: &[&str] = if push { &["serve", "--stdio", "--push"] } else { &["serve", "--stdio"] };
    command_of(remote, args)
}

/// Returns the command which serves a worker, as [`remote_command`] does for a cache. An `ssh://` address runs
/// `jig worker --stdio`.
pub fn worker_command(worker: &str) -> Option<Vec<String>> {
    command_of(worker, &["worker", "--stdio"])
}

/// Returns the command for an address or command line: for an `ssh://` address, ssh running `jig` on the host
/// with `args`, and otherwise the value split at spaces.
fn command_of(remote: &str, args: &[&str]) -> Option<Vec<String>> {
    let Some(address) = remote.strip_prefix("ssh://") else {
        let command: Vec<String> = remote.split_whitespace().map(String::from).collect();
        return (!command.is_empty()).then_some(command);
    };
    let (host, jig) = match address.split_once('/') {
        Some((host, jig)) => (host, format!("/{jig}")),
        None => (address, "jig".to_string()),
    };
    let (host, port) = match host.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (host, Some(port)),
        _ => (host, None),
    };
    if host.is_empty() || host.ends_with('@') {
        return None;
    }
    let mut command: Vec<String> = ["ssh", "-o", "BatchMode=yes"].map(String::from).to_vec();
    if let Some(port) = port {
        command.extend(["-p".into(), port.into()]);
    }
    command.extend([host.to_string(), "--".into(), jig]);
    command.extend(args.iter().map(|&arg| String::from(arg)));
    Some(command)
}

/// The `jig serve` command.
///
/// - `jig serve --stdio [--push]` serves the cache on stdin and stdout, as a remote cache, then collects it if that
///   is due.
/// - `jig serve --listen <address> --token-file <path> [--push]` serves it to every client which connects over TCP
///   and has the token, collecting it after each client.
///
/// Either takes pushes with `--push`.
pub fn serve(args: &crate::Args, settings: &Settings) -> Result<(), Error> {
    let (stdio, push, listen) = (args.flag("stdio"), args.flag("push"), args.value("listen"));
    let token_file = args.value("token-file").map(PathBuf::from);
    let cache = settings.open(false)?.ok_or(Error::CacheOff)?;
    match (stdio, listen) {
        (true, None) => {
            let served = rig::remote::serve(&cache, std::io::stdin().lock(), std::io::stdout(), push);
            settings.collect_if_due(&cache);
            served.map_err(Error::Serve)
        },
        (false, Some(address)) => {
            // TCP authenticates no one, so a client must have the token
            let token = read_token(&token_file.ok_or(Error::MissingArg("--token-file <path>"))?)?;
            let listener = std::net::TcpListener::bind(address).map_err(Error::Serve)?;
            let bound = listener.local_addr().map_err(Error::Serve)?;
            eprintln!("serving {} on {bound}", cache.dir().display());
            let after = || settings.collect_if_due(&cache);
            rig::remote::listen(&cache, &listener, push, &token, &after).map_err(Error::Serve)
        },
        (true, Some(_)) => Err(Error::ExtraArg("--listen".into())),
        (false, None) => Err(Error::MissingArg("--stdio or --listen <address>")),
    }
}

/// The `jig worker` command.
///
/// - `jig worker --stdio` is a worker on stdin and stdout: it runs the actions of the cached edges a build sends
///   it, with the toolchains rustup has, using the cache as its store. It collects the cache afterwards if that is
///   due.
/// - `jig worker --listen <address> --token-file <path>` does the same for every client which connects over TCP
///   and has the token.
///
/// `--jobs <n>` sets how many actions run at once, which defaults to the number of threads the machine has.
/// `--tag <tag>` adds a tag for the worker to report. With `--install`, the worker uses rustup to install a
/// toolchain which an action needs and it does not have.
pub fn worker(args: &crate::Args, settings: &Settings) -> Result<(), Error> {
    let (stdio, listen, install, jobs) = (args.flag("stdio"), args.value("listen"), args.flag("install"), args.jobs()?);
    let token_file = args.value("token-file").map(PathBuf::from);
    let tags: Vec<String> = args.values("tag").map(String::from).collect();
    let cache = settings.open(false)?.ok_or(Error::CacheOff)?;
    let jobs = jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    let known = cache.dir().join("toolchains");
    let mut executor = rig::worker::Executor::new(cache.dir().join("work"), jobs, move || {
        crate::toolchain::capabilities(tags.clone(), &known)
    });
    if install {
        executor = executor.installing(crate::toolchain::install);
    }
    let executor = &executor;
    // find the worker's capabilities while the client is connecting, as they are the first thing it asks for
    std::thread::scope(|scope| {
        scope.spawn(|| executor.capabilities());
        serve_worker(&cache, executor, stdio, listen, token_file, settings)
    })
}

/// Serve a worker on stdin and stdout, or over TCP at an address, to clients with the token in a file.
fn serve_worker(
    cache: &rig::cache::Cache,
    executor: &rig::worker::Executor,
    stdio: bool,
    listen: Option<&str>,
    token_file: Option<PathBuf>,
    settings: &Settings,
) -> Result<(), Error> {
    match (stdio, listen) {
        (true, None) => {
            let server = rig::remote::Server { cache, push: false, token: None, executor: Some(executor) };
            let worked = server.serve(std::io::stdin().lock(), std::io::stdout());
            settings.collect_if_due(cache);
            worked.map_err(Error::Serve)
        },
        (false, Some(address)) => {
            // TCP authenticates no one, and a worker runs what it is sent, so a client must have the token
            let token = read_token(&token_file.ok_or(Error::MissingArg("--token-file <path>"))?)?;
            let listener = std::net::TcpListener::bind(address).map_err(Error::Serve)?;
            let bound = listener.local_addr().map_err(Error::Serve)?;
            eprintln!("running actions with {} toolchains on {bound}", executor.capabilities().toolchains.len());
            let token = Some(token.as_slice());
            let server = rig::remote::Server { cache, push: false, token, executor: Some(executor) };
            let after = || settings.collect_if_due(cache);
            server.listen(&listener, &after).map_err(Error::Serve)
        },
        (true, Some(_)) => Err(Error::ExtraArg("--listen".into())),
        (false, None) => Err(Error::MissingArg("--stdio or --listen <address>")),
    }
}

/// The `jig workers` command, which shows each worker of the `workers` property with its host, jobs, tags and
/// toolchains, or why it cannot be reached.
pub fn workers(settings: &Settings) -> Result<(), Error> {
    let workers = settings.workers()?;
    if workers.is_empty() {
        println!("no workers, as the `workers` property and `JIG_WORKERS` give none");
    }
    // ask all the workers at once, as connecting can take a while
    let reports: Vec<_> = std::thread::scope(|scope| {
        let mut asked = Vec::new();
        for worker in &workers {
            asked.push(scope.spawn(|| (worker.jobs(), worker.capabilities())));
        }
        let failed = || (0, Err("the worker could not be asked".into()));
        asked.into_iter().map(|asked| asked.join().unwrap_or_else(|_| failed())).collect()
    });
    for ((name, worker), (jobs, report)) in settings.workers.iter().zip(&workers).zip(reports) {
        match report {
            Ok(capabilities) => {
                let tags: String = capabilities.tags.iter().flat_map(|tag| [", tagged ", tag.as_str()]).collect();
                // the round trip of the first request is mostly the time to start the worker over ssh
                let away = worker.link().latency.as_millis();
                println!("{name}: {}, {jobs} jobs{tags}, {away} ms away", capabilities.host);
                for toolchain in &capabilities.toolchains {
                    let name = if toolchain.name.is_empty() { "rustc" } else { &toolchain.name };
                    println!("    {name}: {} for {}", toolchain.release, toolchain.targets.join(", "));
                }
            },
            Err(error) => println!("{name}: {error}"),
        }
    }
    Ok(())
}

/// Warn that a remote cache failed during a build. The build went on without it.
pub fn report_remote(cache: &rig::cache::Cache) {
    if let Some(error) = cache.remote().and_then(rig::remote::Remote::error) {
        eprintln!("warning: the remote cache failed, so the build went on without it: {error}");
    }
}

/// Parses a size in bytes, such as `512`, `100K`, `1.5G` or `10GiB`. Units are binary multiples. Returns `None` if
/// the text is not a size.
pub fn parse_size(size: &str) -> Option<u64> {
    let size = size.trim();
    let size = size.strip_suffix("iB").or_else(|| size.strip_suffix('B')).unwrap_or(size);
    let (number, shift) = match size.chars().last()?.to_ascii_uppercase() {
        'K' => (&size[..size.len() - 1], 10),
        'M' => (&size[..size.len() - 1], 20),
        'G' => (&size[..size.len() - 1], 30),
        'T' => (&size[..size.len() - 1], 40),
        _ => (size, 0),
    };
    if let Ok(number) = number.parse::<u64>() {
        return number.checked_mul(1 << shift);
    }
    let number: f64 = number.parse().ok().filter(|number: &f64| number.is_finite() && *number >= 0.0)?;
    // A size too large for a `u64` is refused. A multiple of a power of two up to 2^40 is exact as an `f64`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let (bytes, max) = (number * (1u64 << shift) as f64, u64::MAX as f64);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (bytes < max).then_some(bytes as u64)
}

/// Returns a size in bytes as it is shown, with a binary multiple, such as `1.5 MiB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    while unit + 1 < UNITS.len() && bytes >> (10 * (unit + 1)) > 0 {
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / (1u64 << (10 * unit)) as f64;
    format!("{value:.1} {}", UNITS[unit])
}

/// `jig cache stats`, `jig cache gc [--max-size <size>]` or `jig cache clear`.
pub fn command(args: &crate::Args, settings: &Settings) -> Result<(), Error> {
    let command = args.names().pop().ok_or(Error::MissingArg("cache command"))?;
    let max = args.value("max-size").map(|size| parse_size(size).ok_or_else(|| Error::InvalidSize(size.into())));
    let max = max.transpose()?;
    // only `gc` takes a size
    if max.is_some() && command != "gc" {
        return Err(Error::UnknownOption("cache", "--max-size".into()));
    }
    let cache = settings.open(false)?.ok_or(Error::CacheOff)?;
    let stats = |stats: rig::cache::Stats| {
        format!(
            "{} results, {} manifests and {} blobs, {}",
            stats.results,
            stats.manifests,
            stats.blobs,
            format_size(stats.bytes),
        )
    };
    match command.as_str() {
        "stats" => {
            println!("{}", cache.dir().display());
            println!("{}", stats(cache.stats()));
        },
        "gc" => {
            let collected = cache.gc(max.unwrap_or(settings.size));
            println!("removed {}", stats(collected.removed));
            println!("kept {}", stats(collected.kept));
        },
        "clear" => cache.clear().map_err(Error::Rig)?,
        command => return Err(Error::UnknownCacheCommand(command.into())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn resolved(settings: Result<Settings, Error>) -> Settings {
        settings.unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn finds_the_settings() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(var, _)| *var == name).map(|(_, value)| OsString::from(value))
        };
        // by their components, as a path joined on Windows has `\\` between them
        let dir = |vars| PathBuf::from(default_dir(env(vars)));
        assert_eq!(dir(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")]), Path::new("/x/jig"));
        // and on Windows, where a user's local data goes
        let local = &[("XDG_CACHE_HOME", ""), ("LOCALAPPDATA", "/l"), ("HOME", "/h")];
        let expected = if cfg!(windows) { "/l/jig/cache" } else { "/h/.cache/jig" };
        assert_eq!(dir(local), Path::new(expected));
        assert_eq!(dir(&[("XDG_CACHE_HOME", ""), ("HOME", "/h")]), Path::new("/h/.cache/jig"));
        assert_eq!(default_dir(env(&[])), "");
        // each absolute, which on Windows has a drive
        let absolute = |path: &str| std::path::absolute(path).unwrap();
        // each variable overrides its property
        let properties = Properties {
            dir: "/p".into(),
            size: "1G".into(),
            remote: "ssh://cache".into(),
            push: true,
            token_file: "/t".into(),
            workers: vec!["ssh://worker".into(), " ".into()],
            workers_only: true,
            download: "targets".into(),
        };
        let settings = resolved(Settings::resolve(&properties, env(&[])));
        let expected = Settings {
            dir: Some(absolute("/p")),
            size: 1 << 30,
            remote: Some("ssh://cache".into()),
            push: true,
            token_file: Some(absolute("/t")),
            workers: vec!["ssh://worker".into()],
            workers_only: true,
            download_targets: true,
            release: None,
        };
        assert_eq!(settings, expected);
        let overridden = env(&[
            ("JIG_CACHE", "/c"),
            ("JIG_CACHE_SIZE", "2M"),
            ("JIG_REMOTE_CACHE", "ssh://other"),
            ("JIG_REMOTE_CACHE_PUSH", "0"),
            ("JIG_WORKERS", "tcp://a:1; env jig worker --stdio ;"),
            ("JIG_WORKERS_ONLY", "0"),
            ("JIG_DOWNLOAD", "all"),
        ]);
        let settings = resolved(Settings::resolve(&properties, overridden));
        let expected = Settings {
            dir: Some(absolute("/c")),
            size: 2 << 20,
            remote: Some("ssh://other".into()),
            push: false,
            token_file: Some(absolute("/t")),
            workers: vec!["tcp://a:1".into(), "env jig worker --stdio".into()],
            workers_only: false,
            download_targets: false,
            release: None,
        };
        assert_eq!(settings, expected);
        // and turns it off, or has no remote cache
        let off = resolved(Settings::resolve(&properties, env(&[("JIG_CACHE", "off"), ("JIG_REMOTE_CACHE", " ")])));
        assert_eq!((off.dir, off.remote), (None, None));
        let empty = Properties {
            dir: String::new(),
            size: "10G".into(),
            remote: String::new(),
            push: false,
            token_file: String::new(),
            workers: Vec::new(),
            workers_only: false,
            download: String::new(),
        };
        assert_eq!(resolved(Settings::resolve(&empty, env(&[]))).dir, None);
        // what is not a size or a remote cache is an error
        assert!(matches!(Settings::resolve(&properties, env(&[("JIG_CACHE_SIZE", "lots")])), Err(Error::InvalidSize(_))));
        let download = Settings::resolve(&properties, env(&[("JIG_DOWNLOAD", "some")]));
        assert!(matches!(download, Err(Error::InvalidDownload(_))));
        assert!(matches!(Settings::resolve(&properties, env(&[("JIG_REMOTE_CACHE", "ssh://")])), Err(Error::InvalidRemote(_))));
        let workers: [&'static [(&str, &str)]; 3] =
            [&[("JIG_WORKERS", "http://cache")], &[("JIG_WORKERS", "ssh://")], &[("JIG_WORKERS", "tcp://worker")]];
        for workers in workers {
            let resolved = Settings::resolve(&properties, env(workers));
            assert!(matches!(resolved, Err(Error::InvalidWorker(_))), "{workers:?}");
        }
    }

    #[test]
    fn parses_and_formats_sizes() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("100K"), Some(100 << 10));
        assert_eq!(parse_size("2m"), Some(2 << 20));
        assert_eq!(parse_size("10GiB"), Some(10 << 30));
        assert_eq!(parse_size("1.5G"), Some(3 << 29));
        assert_eq!(parse_size("1T"), Some(1 << 40));
        for invalid in ["", "G", "-1", "ten", "1X", "nan"] {
            assert_eq!(parse_size(invalid), None, "{invalid}");
        }
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(10 << 30), "10.0 GiB");
    }

    #[test]
    fn finds_the_remote_command() {
        let command = |remote, push| remote_command(remote, push).map(|command| command.join(" "));
        assert_eq!(command("ssh://cache", false).as_deref(), Some("ssh -o BatchMode=yes cache -- jig serve --stdio"));
        assert_eq!(
            command("ssh://ci@cache:2222/opt/bin/jig", true).as_deref(),
            Some("ssh -o BatchMode=yes -p 2222 ci@cache -- /opt/bin/jig serve --stdio --push"),
        );
        assert_eq!(command("ssh://", false), None);
        assert_eq!(command("ssh://ci@", false), None);
        assert_eq!(command("env JIG_CACHE=/srv jig serve --stdio", false).as_deref(), Some("env JIG_CACHE=/srv jig serve --stdio"));
        assert_eq!(command("  ", false), None);
        assert_eq!(remote_transport("tcp://cache:7000", true), Some(Transport::Tcp("cache:7000".into())));
        assert_eq!(remote_transport("tcp://[::1]:7000", false), Some(Transport::Tcp("[::1]:7000".into())));
        assert_eq!(remote_transport("tcp://cache", false), None);
        assert_eq!(remote_transport("tcp://:7000", false), None);
        assert_eq!(remote_transport("tcp://cache:port", false), None);
        let url = rig::http::Url::parse("http://cache:8080/rig").unwrap();
        assert_eq!(remote_transport("http://cache:8080/rig", false), Some(Transport::Http(url)));
        assert_eq!(remote_transport("https://cache/rig", false), None);
        assert_eq!(remote_transport("http://", false), None);
        // a worker over ssh runs `jig worker`
        let worker = |worker| worker_transport(worker);
        let ssh = "ssh -o BatchMode=yes -p 2222 build -- jig worker --stdio".split(' ').map(String::from).collect();
        assert_eq!(worker("ssh://build:2222"), Some(Transport::Command(ssh)));
        assert_eq!(worker("tcp://build:7000"), Some(Transport::Tcp("build:7000".into())));
        assert_eq!(worker("http://build"), None);
    }
}
