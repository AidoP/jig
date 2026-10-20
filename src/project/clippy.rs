//! The levels of clippy's lints for `jig lint`, which are properties named `clippy/<lint>`, such as
//! `clippy/needless_return` or the group `clippy/pedantic`.

use std::collections::BTreeSet;

use super::Project;
use super::diagnostic::{Diagnostic, Message, Severity};
use super::syntax::Span;
use super::value::{Data, Property, Type};

/// The groups of clippy's lints, whose properties are declared from the start, so that `clippy/` is a namespace. Their
/// levels are given before those of single lints, so that a lint in a group can be given another level.
const LINT_GROUPS: &[&str] = &[
    "all",
    "cargo",
    "complexity",
    "correctness",
    "nursery",
    "pedantic",
    "perf",
    "restriction",
    "style",
    "suspicious",
];

/// The levels a lint can have, with the flag rustc gives each.
const LEVELS: &[(&str, &str)] = &[
    ("allow", "-A"),
    ("warn", "-W"),
    ("force-warn", "--force-warn"),
    ("deny", "-D"),
    ("forbid", "-F"),
];

/// Returns the flag of a level of a lint, if it is one.
pub(super) fn level_flag(level: &str) -> Option<&'static str> {
    LEVELS.iter().find(|(name, _)| *name == level).map(|(_, flag)| *flag)
}

/// Returns the name of a lint from the name of its property, `clippy/<lint>`, if it can be the name of one, of
/// lowercase letters, digits, `_` and `-`.
fn lint_name(name: &str) -> Option<&str> {
    let lint = name.strip_prefix("clippy/")?;
    let valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
    (!lint.is_empty() && lint.chars().all(valid)).then_some(lint)
}

/// The info of the property of a lint.
const INFO: &str = "the level of the clippy lint for the crates declared while it is set, which `jig lint` checks \
                    them with: 'allow', 'warn', 'force-warn', 'deny' or 'forbid'";

impl Project {
    /// Declare the property of each group of clippy's lints, which has no value until it is set.
    ///
    /// Those of single lints are declared when they are first used, by [`Self::declare_clippy_lint`], as clippy has
    /// many, which only it knows.
    pub fn declare_clippy_lints(&mut self) {
        for group in LINT_GROUPS {
            self.declare_clippy_lint(&format!("clippy/{group}"));
        }
    }
    /// Declare the property of a clippy lint, `clippy/<lint>`, if it can be the name of one and it has not been
    /// declared. Returns whether it is the name of one.
    pub(super) fn declare_clippy_lint(&mut self, name: &str) -> bool {
        if lint_name(name).is_none() {
            return false;
        }
        if self.clippy_lints.contains(name) {
            return true;
        }
        self.clippy_lints.insert(name.into());
        let property = Property::new(name.into(), Type::String).info(INFO);
        self.declare(name.into(), Some(property));
        true
    }
    /// Returns the flags which give the clippy lints the levels which are set where `span` is, as `-W clippy::<lint>`,
    /// where the groups are given first, so that the single lints replace them.
    pub(super) fn clippy_flags(&mut self, span: Span) -> Vec<String> {
        let (groups, lints): (BTreeSet<_>, BTreeSet<_>) = self.clippy_lints.iter().cloned().partition(|name| {
            lint_name(name).is_some_and(|lint| LINT_GROUPS.contains(&lint))
        });
        let mut flags = Vec::new();
        for name in groups.into_iter().chain(lints) {
            let Some(value) = self.value(&name, span.file) else {
                continue;
            };
            let Data::Str(level) = &value.data else {
                continue;
            };
            // the level was checked when it was set
            let Some(flag) = level_flag(level) else {
                continue;
            };
            // rustc takes `_` in the names of lints, which `-` is written for elsewhere
            let lint = lint_name(&name).unwrap().replace('-', "_");
            flags.push(format!("{flag} clippy::{lint}"));
        }
        flags
    }
    /// Report the level of a clippy lint which is not one, returning whether it is one.
    pub(super) fn check_clippy_level(&mut self, name: &str, data: &Data, span: Span) -> bool {
        if lint_name(name).is_none() {
            return true;
        }
        if let Data::Str(level) = data && level_flag(level).is_some() {
            return true;
        }
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::UnknownLintLevel(data.coerce_string()),
            severity: Severity::Error,
        });
        false
    }
}
