//! What rig needs of the system which `std` does not provide: interrupts, process groups, the jobserver's FIFO or
//! semaphore, file modes, the size of a terminal and reflinks, each of which has the same interface on each platform.
//!
//! The `unsafe` code of rig is here, behind a safe interface.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Returns the canonical path of a file, as [`std::fs::canonicalize`] does, but on Windows, as such a path is usually
/// written, as `C:\dir` rather than the verbatim `\\?\C:\dir` Windows gives, so that it shares its prefix with the
/// paths it is compared with, unless it is only valid as a verbatim path, such as one longer than Windows otherwise
/// takes.
///
/// # Errors
///
/// As for [`std::fs::canonicalize`].
pub fn canonicalize(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if !cfg!(windows) {
        return Ok(canonical);
    }
    let Some(text) = canonical.to_str() else {
        return Ok(canonical);
    };
    let written = match (text.strip_prefix(r"\\?\UNC\"), text.strip_prefix(r"\\?\")) {
        (Some(unc), _) => format!(r"\\{unc}"),
        (None, Some(disk)) if disk.as_bytes().get(1) == Some(&b':') => disk.to_string(),
        _ => return Ok(canonical),
    };
    // a path Windows takes as it is written: not too long, nor with a component it would read otherwise
    let plain = |component: &str| !component.ends_with(['.', ' ']) && component != "." && component != "..";
    let fits = written.len() < 260 && written.split('\\').skip(1).all(plain);
    Ok(if fits { written.into() } else { canonical })
}

#[cfg(test)]
mod tests {
    #[test]
    fn gives_random_bytes() {
        let (mut a, mut b) = ([0; 32], [0; 32]);
        super::random(&mut a).unwrap();
        super::random(&mut b).unwrap();
        // which are, by chance, neither all zero nor the same twice
        assert!(a != [0; 32] && a != b);
    }

    #[test]
    fn canonicalizes_as_paths_are_written() {
        let dir = std::env::temp_dir();
        let canonical = super::canonicalize(&dir).unwrap();
        assert_eq!(std::fs::canonicalize(&canonical).unwrap(), std::fs::canonicalize(&dir).unwrap());
        // which on Windows is not verbatim
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"), "{canonical:?}");
    }
}
