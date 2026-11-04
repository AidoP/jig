//! rig's cache, where the results of rustc, clippy and the custom commands with `cache` are kept, and `jig cache`,
//! which shows, collects and clears it.
//!
//! The cache is set by the builtin properties `cache/dir`, `cache/size`, `cache/remote` and `cache/push`, which the
//! user's configuration, as [`crate::config`] finds it, usually sets, each of which an environment variable overrides:
//!
//! - `cache/dir`, or `JIG_CACHE`: the directory of the cache, `$XDG_CACHE_HOME/jig` or `$HOME/.cache/jig` by default,
//!   or `off`, or empty, without a cache.
//! - `cache/size`, or `JIG_CACHE_SIZE`: the size it is collected down to, after a build at most once a day, `10G` by
//!   default.
//! - `cache/remote`, or `JIG_REMOTE_CACHE`: the remote cache it is in front of, if any, as
//!   `ssh://[<user>@]<host>[:<port>][/<jig>]`, which runs `jig serve --stdio` there with ssh, or as a command, split at
//!   spaces, which serves it on its stdin and stdout.
//! - `cache/push`, or `JIG_REMOTE_CACHE_PUSH=1`: whether results are pushed to the remote cache, if it takes them.
//!
//! The settings are resolved when the project is configured, as the variables are read by the configuration, and
//! recorded in its database, which a build reads them from.

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
/// The size the cache is collected down to by default.
pub const DEFAULT_SIZE: &str = "10G";
/// How often the cache is collected after a build.
const EVERY: Duration = Duration::from_hours(24);

/// Returns the directory of the cache by default, `$XDG_CACHE_HOME/jig` or `$HOME/.cache/jig`, or empty, which is
/// off, without either, from environment variables as `env` gives them.
pub fn default_dir(mut env: impl FnMut(&str) -> Option<OsString>) -> String {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    let dir = match set(env("XDG_CACHE_HOME")) {
        Some(cache) => PathBuf::from(cache).join("jig"),
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
}

impl Settings {
    /// Resolve the settings from the cache's properties, each of which its variable, as `env` gives it, overrides.
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
            && remote_command(remote, push).is_none()
        {
            return Err(Error::InvalidRemote(remote.clone()));
        }
        Ok(Self { dir, size, remote, push })
    }
    /// Opens the cache, if it is on, in front of the remote cache, if there is one and `remote`.
    pub fn open(&self, remote: bool) -> Result<Option<rig::cache::Cache>, Error> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        let cache = rig::cache::Cache::open(dir).map_err(Error::Rig)?;
        let command = self.remote.as_deref().filter(|_| remote).and_then(|remote| remote_command(remote, self.push));
        Ok(Some(match command {
            Some(command) => cache.with_remote(std::sync::Arc::new(rig::remote::Remote::command(command, self.push))),
            None => cache,
        }))
    }
    /// Collect the cache after a build, if it was last collected a day ago or longer.
    pub fn collect_if_due(&self, cache: &rig::cache::Cache) {
        let _ = cache.gc_if_due(self.size, EVERY);
    }
}

/// Returns the command which serves a remote cache, from `JIG_REMOTE_CACHE`, which pushes if `push`, or `None` if it is
/// empty, or an `ssh://` address without a host.
///
/// An `ssh://` address runs `jig`, or the path after its host, with `serve --stdio`, and `--push` to push, by ssh in
/// batch mode, so that it fails rather than asking for a password. A command is split at spaces, and is run as it is.
pub fn remote_command(remote: &str, push: bool) -> Option<Vec<String>> {
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
    command.extend([host.to_string(), "--".into(), jig, "serve".into(), "--stdio".into()]);
    if push {
        command.push("--push".into());
    }
    Some(command)
}

/// `jig serve --stdio [--push]`, which serves the cache on stdin and stdout, as a remote cache, taking pushes with
/// `--push`, then collects it if it is due.
pub fn serve(args: impl Iterator<Item = OsString>, settings: &Settings) -> Result<(), Error> {
    let (mut stdio, mut push) = (false, false);
    for arg in args {
        match arg.to_str() {
            Some("--stdio") => stdio = true,
            Some("--push") => push = true,
            _ => return Err(Error::ExtraArg(arg.to_string_lossy().into_owned())),
        }
    }
    if !stdio {
        return Err(Error::MissingArg("--stdio"));
    }
    let cache = settings.open(false)?.ok_or(Error::CacheOff)?;
    let served = rig::remote::serve(&cache, std::io::stdin().lock(), std::io::stdout().lock(), push);
    settings.collect_if_due(&cache);
    served.map_err(Error::Serve)
}

/// Warn of a remote cache which failed during a build, which went on without it.
pub fn report_remote(cache: &rig::cache::Cache) {
    if let Some(error) = cache.remote().and_then(rig::remote::Remote::error) {
        eprintln!("warning: the remote cache failed, so the build went on without it: {error}");
    }
}

/// Returns a size in bytes, such as `512`, `100K`, `1.5G` or `10GiB`, with a binary multiple, or `None` if it is not
/// one.
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
    // a size beyond a `u64` is not one, and a multiple of a power of two up to 2^40 is exact as an `f64`
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
pub fn command(mut args: impl Iterator<Item = OsString>, settings: &Settings) -> Result<(), Error> {
    let command = args.next().ok_or(Error::MissingArg("cache command"))?;
    let command = command.to_string_lossy().into_owned();
    let mut max = None;
    if command == "gc" {
        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy().into_owned();
            let size = match arg.strip_prefix("--max-size=") {
                Some(size) => size.to_string(),
                None if arg == "--max-size" => args.next().ok_or(Error::MissingArg("size"))?.to_string_lossy().into_owned(),
                None => return Err(Error::ExtraArg(arg)),
            };
            max = Some(parse_size(&size).ok_or(Error::InvalidSize(size))?);
        }
    } else if let Some(extra) = args.next() {
        return Err(Error::ExtraArg(extra.to_string_lossy().into_owned()));
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

    fn resolved(settings: Result<Settings, Error>) -> Settings {
        settings.unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn finds_the_settings() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(var, _)| *var == name).map(|(_, value)| OsString::from(value))
        };
        assert_eq!(default_dir(env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])), "/x/jig");
        assert_eq!(default_dir(env(&[("XDG_CACHE_HOME", ""), ("HOME", "/h")])), "/h/.cache/jig");
        assert_eq!(default_dir(env(&[])), "");
        // each variable overrides its property
        let properties = Properties { dir: "/p".into(), size: "1G".into(), remote: "ssh://cache".into(), push: true };
        let settings = resolved(Settings::resolve(&properties, env(&[])));
        let expected = Settings { dir: Some("/p".into()), size: 1 << 30, remote: Some("ssh://cache".into()), push: true };
        assert_eq!(settings, expected);
        let overridden = env(&[
            ("JIG_CACHE", "/c"),
            ("JIG_CACHE_SIZE", "2M"),
            ("JIG_REMOTE_CACHE", "ssh://other"),
            ("JIG_REMOTE_CACHE_PUSH", "0"),
        ]);
        let settings = resolved(Settings::resolve(&properties, overridden));
        let expected = Settings { dir: Some("/c".into()), size: 2 << 20, remote: Some("ssh://other".into()), push: false };
        assert_eq!(settings, expected);
        // and turns it off, or has no remote cache
        let off = resolved(Settings::resolve(&properties, env(&[("JIG_CACHE", "off"), ("JIG_REMOTE_CACHE", " ")])));
        assert_eq!((off.dir, off.remote), (None, None));
        let empty = Properties { dir: String::new(), size: "10G".into(), remote: String::new(), push: false };
        assert_eq!(resolved(Settings::resolve(&empty, env(&[]))).dir, None);
        // what is not a size or a remote cache is an error
        assert!(matches!(Settings::resolve(&properties, env(&[("JIG_CACHE_SIZE", "lots")])), Err(Error::InvalidSize(_))));
        assert!(matches!(Settings::resolve(&properties, env(&[("JIG_REMOTE_CACHE", "ssh://")])), Err(Error::InvalidRemote(_))));
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
    }
}
