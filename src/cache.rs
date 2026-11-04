//! rig's cache, where the results of rustc, clippy and the custom commands with `cache` are kept, and `jig cache`,
//! which shows, collects and clears it.
//!
//! The cache is `$JIG_CACHE`, or `$XDG_CACHE_HOME/jig`, or `$HOME/.cache/jig`, unless `JIG_CACHE` is empty or `off`. It
//! is collected after a build at most once a day, down to `$JIG_CACHE_SIZE`, which is 10 GiB by default.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::Error;

/// The environment variable which gives the directory of the cache, or turns it off.
pub const DIR: &str = "JIG_CACHE";
/// The environment variable which gives the size the cache is collected down to.
pub const SIZE: &str = "JIG_CACHE_SIZE";
/// The size the cache is collected down to by default, 10 GiB.
const DEFAULT_SIZE: u64 = 10 << 30;
/// How often the cache is collected after a build.
const EVERY: Duration = Duration::from_hours(24);

/// Returns the directory of the cache, absolute, from environment variables as `env` gives them, or `None` if it is
/// off, as `JIG_CACHE` is empty or `off`, or there is nowhere for it, without `XDG_CACHE_HOME` and `HOME`.
///
/// A variable is only read if the one before it is not set, so that the configuration, which reads them to find
/// whether rig caches, depends only on those which decide it.
pub fn dir(mut env: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
    let dir = match env(DIR) {
        Some(dir) if dir.is_empty() || dir == "off" => return None,
        Some(dir) => PathBuf::from(dir),
        None => match set(env("XDG_CACHE_HOME")) {
            Some(cache) => PathBuf::from(cache).join("jig"),
            None => PathBuf::from(set(env("HOME"))?).join(".cache/jig"),
        },
    };
    std::path::absolute(&dir).ok()
}

/// Opens the cache, if it is on.
pub fn open() -> Result<Option<rig::cache::Cache>, Error> {
    let Some(dir) = dir(|name| std::env::var_os(name)) else {
        return Ok(None);
    };
    rig::cache::Cache::open(&dir).map(Some).map_err(Error::Rig)
}

/// Returns the size the cache is collected down to, from `JIG_CACHE_SIZE`, or 10 GiB without it.
fn max_size() -> Result<u64, Error> {
    match std::env::var(SIZE) {
        Ok(size) => parse_size(&size).ok_or(Error::InvalidSize(size)),
        Err(_) => Ok(DEFAULT_SIZE),
    }
}

/// Collect the cache after a build, if it was last collected a day ago or longer.
pub fn collect_if_due(cache: &rig::cache::Cache) -> Result<(), Error> {
    let _ = cache.gc_if_due(max_size()?, EVERY);
    Ok(())
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
pub fn command(mut args: impl Iterator<Item = OsString>) -> Result<(), Error> {
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
    let cache = open()?.ok_or(Error::CacheOff)?;
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
            let collected = cache.gc(max.map_or_else(max_size, Ok)?);
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

    #[test]
    fn finds_the_directory() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(var, _)| *var == name).map(|(_, value)| OsString::from(value))
        };
        assert_eq!(dir(env(&[("JIG_CACHE", "/c"), ("HOME", "/h")])), Some(PathBuf::from("/c")));
        assert_eq!(dir(env(&[("JIG_CACHE", "off"), ("HOME", "/h")])), None);
        assert_eq!(dir(env(&[("JIG_CACHE", ""), ("HOME", "/h")])), None);
        assert_eq!(dir(env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])), Some(PathBuf::from("/x/jig")));
        assert_eq!(dir(env(&[("XDG_CACHE_HOME", ""), ("HOME", "/h")])), Some(PathBuf::from("/h/.cache/jig")));
        assert_eq!(dir(env(&[])), None);
        // a variable after one which is set is not read
        let mut read = Vec::new();
        dir(|name| {
            read.push(name.to_string());
            (name == "JIG_CACHE").then(|| "/c".into())
        });
        assert_eq!(read, ["JIG_CACHE"]);
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
}
