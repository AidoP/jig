//! Semantic versions, which `semver-compatible` compares as cargo compares a version to a dependency's requirement.

use std::cmp::Ordering;

/// A version, `<major>.<minor>.<patch>`, with an optional pre-release, `-<identifiers>`, where build metadata,
/// `+<identifiers>`, is ignored. A requirement may leave out its minor and patch numbers.
#[derive(Debug, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
    /// The identifiers of the pre-release, separated by `.`, which is empty for a release.
    pre: Vec<String>,
}

impl Version {
    /// Parse a version, which may leave out its minor and patch numbers if `partial`.
    fn parse(version: &str, partial: bool) -> Result<Self, String> {
        let version = version.split_once('+').map_or(version, |(version, _)| version);
        let (numbers, pre) = match version.split_once('-') {
            Some((numbers, pre)) => (numbers, Some(pre)),
            None => (version, None),
        };
        let mut parts = numbers.split('.');
        let mut number = |name: &str| -> Result<Option<u64>, String> {
            let Some(part) = parts.next() else {
                return if partial { Ok(None) } else { Err(format!("its {name} number is missing")) };
            };
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("its {name} number, {part:?}, is not a number"));
            }
            part.parse().map(Some).map_err(|_| format!("its {name} number, {part:?}, is too large"))
        };
        // there is always a first part, which may be empty
        let major = number("major")?.unwrap_or_default();
        let minor = number("minor")?;
        // a patch number needs a minor number
        let patch = if minor.is_some() { number("patch")? } else { None };
        if parts.next().is_some() {
            return Err("it has more than three numbers".into());
        }
        // a pre-release needs all of the numbers, as it is a pre-release of a particular version
        let pre = match pre {
            Some(_) if patch.is_none() => return Err("it has a pre-release without a patch number".into()),
            Some(pre) => {
                let identifiers: Vec<_> = pre.split('.').map(String::from).collect();
                if identifiers.iter().any(|identifier| {
                    identifier.is_empty() || !identifier.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }) {
                    return Err(format!("its pre-release, {pre:?}, is not identifiers separated by `.`"));
                }
                identifiers
            },
            None => Vec::new(),
        };
        Ok(Self { major, minor, patch, pre })
    }
    /// The numbers, where those which are left out are `0`.
    fn numbers(&self) -> (u64, u64, u64) {
        (self.major, self.minor.unwrap_or(0), self.patch.unwrap_or(0))
    }
    /// Compare by precedence, where a pre-release comes before its release.
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers().cmp(&other.numbers()).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_pre(&self.pre, &other.pre),
        })
    }
}

/// Compare the identifiers of pre-releases, where numbers are compared numerically and come before others, which are
/// compared by their bytes, and a pre-release with more identifiers comes after one which they start with.
fn compare_pre(lhs: &[String], rhs: &[String]) -> Ordering {
    for (lhs, rhs) in lhs.iter().zip(rhs) {
        let numeric = |identifier: &str| identifier.bytes().all(|b| b.is_ascii_digit());
        let ordering = match (numeric(lhs), numeric(rhs)) {
            (true, true) => (lhs.len(), lhs).cmp(&(rhs.len(), rhs)),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => lhs.cmp(rhs),
        };
        if ordering.is_ne() {
            return ordering;
        }
    }
    lhs.len().cmp(&rhs.len())
}

/// Whether a version is compatible with a requirement, as cargo's default, caret, requirements are, where the version
/// is at least the requirement, and before the next version which changes its leftmost number which is not `0`, or the
/// last number it gives: `1.2.3` allows up to `2.0.0`, `0.2.3` up to `0.3.0`, `0.0.3` only itself, `1.2` up to `2.0.0`
/// and `0` up to `1.0.0`.
///
/// A pre-release is only compatible with a requirement for a pre-release of the same numbers, as with cargo.
///
/// Returns a description of what is wrong with the version or the requirement if one is invalid.
pub(super) fn compatible(version: &str, requirement: &str) -> Result<bool, (Which, String)> {
    let version = Version::parse(version, false).map_err(|error| (Which::Version, error))?;
    let requirement = Version::parse(requirement, true).map_err(|error| (Which::Requirement, error))?;
    if !version.pre.is_empty() && (requirement.pre.is_empty() || version.numbers() != requirement.numbers()) {
        return Ok(false);
    }
    let (major, minor, patch) = requirement.numbers();
    let upper = match (requirement.minor, requirement.patch) {
        _ if major > 0 => (major + 1, 0, 0),
        (None, _) => (1, 0, 0),
        (Some(_), None) => (0, minor + 1, 0),
        (Some(_), Some(_)) if minor > 0 => (0, minor + 1, 0),
        (Some(_), Some(_)) => (0, 0, patch + 1),
    };
    Ok(version.cmp(&requirement).is_ge() && version.numbers() < upper)
}

/// Which argument of `semver-compatible` is invalid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Which {
    Version,
    Requirement,
}

#[cfg(test)]
mod tests {
    use super::{Which, compatible};

    #[track_caller]
    fn check(version: &str, requirement: &str, expected: bool) {
        assert_eq!(compatible(version, requirement), Ok(expected), "{version} with {requirement}");
    }

    #[test]
    fn caret_requirements() {
        check("1.2.3", "1.2.3", true);
        check("1.9.0", "1.2.3", true);
        check("1.2.2", "1.2.3", false);
        check("2.0.0", "1.2.3", false);
        check("0.2.9", "0.2.3", true);
        check("0.3.0", "0.2.3", false);
        check("0.0.3", "0.0.3", true);
        check("0.0.4", "0.0.3", false);
        check("1.5.0", "1.2", true);
        check("1.1.9", "1.2", false);
        check("0.2.7", "0.2", true);
        check("0.3.0", "0.2", false);
        check("0.0.7", "0.0", true);
        check("0.1.0", "0.0", false);
        check("1.99.0", "1", true);
        check("2.0.0", "1", false);
        check("0.9.9", "0", true);
        check("1.0.0", "0", false);
        // build metadata is ignored
        check("1.2.3+build.5", "1.2.3+other", true);
    }

    #[test]
    fn pre_releases() {
        check("1.2.3-beta.2", "1.2.3-beta.1", true);
        check("1.2.3-beta.1", "1.2.3-beta.2", false);
        check("1.2.3", "1.2.3-beta.1", true);
        // only for a requirement for a pre-release of the same numbers
        check("1.2.4-beta.1", "1.2.3-beta.1", false);
        check("1.3.0-alpha", "1.2.3", false);
        // numbers come before other identifiers, and more identifiers after fewer
        check("1.0.0-alpha.beta", "1.0.0-alpha.1", true);
        check("1.0.0-alpha.1", "1.0.0-alpha", true);
        check("1.0.0-alpha.10", "1.0.0-alpha.9", true);
    }

    #[test]
    fn invalid_versions() {
        let error = |version, requirement| compatible(version, requirement).unwrap_err();
        assert_eq!(error("1.2", "1"), (Which::Version, "its patch number is missing".into()));
        assert_eq!(error("1.2.3", "1.x"), (Which::Requirement, "its minor number, \"x\", is not a number".into()));
        assert_eq!(error("1.2.3", ""), (Which::Requirement, "its major number, \"\", is not a number".into()));
        assert_eq!(error("1.2.3.4", "1"), (Which::Version, "it has more than three numbers".into()));
        let pre = "it has a pre-release without a patch number";
        assert_eq!(error("1.2.3", "1.2-beta"), (Which::Requirement, pre.into()));
        let pre = "its pre-release, \"\", is not identifiers separated by `.`";
        assert_eq!(error("1.2.3-", "1"), (Which::Version, pre.into()));
    }
}
