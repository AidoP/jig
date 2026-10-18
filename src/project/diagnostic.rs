//! Diagnostics, and how they are written for a person to read.

use std::path::PathBuf;

use super::functions::Arguments;
use super::syntax::Span;
use super::value::{Data, Property, Type};
use super::{File, Project};

#[derive(Clone, Copy, PartialEq)]
pub enum Severity {
    /// An error which stops the configuration where it is reported, so that nothing after it is processed.
    Fatal,
    Error,
    Warning,
    Note,
    /// A change which fixes a diagnostic before it.
    Suggestion,
    /// A diagnostic which is not reported, as its `diagnostic/<id>` property allows it.
    Allow,
}
impl Severity {
    pub const fn accent(&self) -> &'static str {
        match self {
            Self::Fatal | Self::Error => "\x1b[91m\x1b[1m",
            Self::Warning => "\x1b[93m\x1b[1m",
            Self::Note => "\x1b[96m\x1b[1m",
            Self::Suggestion => "\x1b[92m\x1b[1m",
            Self::Allow => "",
        }
    }
}
impl core::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Fatal => write!(f, "fatal"),
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Note => write!(f, "note"),
            Severity::Suggestion => write!(f, "suggestion"),
            Severity::Allow => write!(f, "allow"),
        }
    }
}

/// Something declared by a statement, as it is named in messages.
#[derive(Clone, Copy)]
pub enum Item {
    Crate,
    CustomCommand,
    Test,
    Target,
    Property,
}
impl Item {
    /// The keyword of the statement which declares the item.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Crate => "crate",
            Self::CustomCommand => "custom",
            Self::Test => "test",
            Self::Target => "target",
            Self::Property => "declare",
        }
    }
}
impl core::fmt::Display for Item {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Crate => "crate",
            Self::CustomCommand => "custom command",
            Self::Test => "test",
            Self::Target => "target",
            Self::Property => "property",
        })
    }
}

pub struct Diagnostic {
    pub span: Option<Span>,
    pub message: Message,
    pub severity: Severity,
}

/// The identifiers of the diagnostics, as given by [`Message::id`], which each have a `diagnostic/<id>` property to
/// override their severity. Those attached to the diagnostic before them follow its severity instead.
const DIAGNOSTIC_IDS: &[&str] = &[
    "user",
    "unknown-severity",
    "unknown-statement",
    "unknown-meta-statement",
    "expected-statement-after-meta",
    "unknown-function",
    "expected-separator",
    "argument-count",
    "none-selected",
    "positional-after-env",
    "invalid-program-name",
    "program-not-found",
    "run-spawn",
    "run-failed",
    "run-output",
    "environment-variable",
    "impure",
    "else-without-if",
    "then-without-statement",
    "else-not-first",
    "unset-property",
    "expected-identifier",
    "invalid-identifier",
    "expected-string",
    "unterminated-string",
    "string-indentation",
    "invalid-string-template",
    "invalid-integer",
    "invalid-operand",
    "mismatched-operands",
    "integer-overflow",
    "shift-amount",
    "division-by-zero",
    "invalid-type",
    "expected-path",
    "expected-expression",
    "read-failure",
    "repeated-use",
    "expected-exact",
    "undeclared-property",
    "redeclare-property",
    "read-only-property",
    "unknown-declaration-attribute",
    "unknown-crate-attribute",
    "unknown-attribute",
    "repeated-attribute",
    "missing-attribute",
    "redeclare-custom",
    "newline-in-argument",
    "test-target",
    "builtin-target-name",
    "invalid-crate-type",
    "empty-separator",
    "no-values",
    "repeated-key",
    "expected-dict-entry",
    "missing-key",
    "index-out-of-range",
    "proc-macro-target",
    "unknown-reference",
    "unknown-namespace",
    "unknown-attribute-of",
    "attribute-not-set",
    "use-binary",
    "use-with-target",
    "override-attribute",
    "repeated-target",
    "redeclare-crate",
    "unknown-crate",
    "invalid-target",
    "overlapping-output",
    "invalid-crate-name",
    "invalid-install-mode",
    "invalid-version",
    "unknown-keyword-argument",
    "repeated-keyword-argument",
    "install-conflict",
    "proxy-build",
    "unknown-target",
    "mismatched-type",
];

/// The message of a diagnostic, each kind of which is identified by [`Message::id`].
pub enum Message {
    User(String),
    /// The message of a `log` statement with an `id` attribute, which is identified by it.
    Logged {
        id: String,
        message: String,
    },
    UnknownSeverity(String),
    UnknownStatement(String),
    UnknownMetaStatement(String),
    ExpectedStatementAfterMeta,
    UnknownFunction(String),
    /// A separator or the closing character of a list.
    ExpectedSeparator(char),
    ArgumentCount {
        function: String,
        expected: Arguments,
        found: usize,
    },
    NoneSelected(Vec<String>),
    PositionalAfterEnv,
    InvalidProgramName(String),
    ProgramNotFound(String),
    RunSpawn {
        program: String,
        error: String,
    },
    RunFailed {
        program: String,
        status: String,
        stderr: String,
    },
    RunOutput {
        program: String,
    },
    EnvironmentVariable {
        name: String,
        /// Whether the variable is set, but is not valid unicode.
        present: bool,
    },
    /// A function whose value can change without the build being reconfigured, such as `env` or `run`.
    Impure(&'static str),
    ElseWithoutIf(String),
    ThenWithoutStatement,
    ElseNotFirst(String),
    UnsetProperty(String),
    ExpectedIdentifier,
    InvalidIdentifier {
        name: String,
        character: char,
    },
    ExpectedString,
    UnterminatedString,
    StringIndentation {
        expected: usize,
    },
    InvalidStringTemplate,
    InvalidInteger,
    InvalidOperand {
        ty: &'static str,
        expected: &'static str,
        coerce: bool,
        /// Where the value was set, if it is the value of a property.
        origin: Option<Span>,
    },
    MismatchedOperands {
        lhs: &'static str,
        rhs: &'static str,
        lhs_origin: Option<Span>,
        rhs_origin: Option<Span>,
    },
    IntegerOverflow,
    ShiftAmount,
    DivisionByZero,
    InvalidType(String),
    ExpectedPath,
    ExpectedExpression,
    ReadFailure(PathBuf, std::io::Error),
    RepeatedUse {
        path: PathBuf,
        file: usize,
    },
    ExpectedExact(&'static str),
    UndeclaredProperty(String),
    RedeclareProperty {
        id: usize,
        name: String,
        builtin: bool,
    },
    ReadOnlyProperty {
        id: usize,
        name: String,
        /// The value already assigned, if it is not builtin.
        assigned: Option<Span>,
        builtin: bool,
    },
    UnknownDeclarationAttribute {
        property: usize,
        name: String,
    },
    UnknownCrateAttribute {
        name: String,
    },
    UnknownAttribute {
        /// The keyword of the statement.
        keyword: &'static str,
        name: String,
    },
    RepeatedAttribute(String),
    MissingAttribute(&'static str),
    RedeclareCustom {
        item: Item,
        name: String,
        previous: Span,
    },
    NewlineInArgument,
    TestTarget {
        target: String,
        host: String,
    },
    BuiltinTargetName(String),
    InvalidCrateType(String),
    EmptySeparator,
    NoValues,
    RepeatedKey(String),
    ExpectedDictEntry,
    MissingKey(String),
    IndexOutOfRange {
        len: usize,
    },
    ProcMacroTarget {
        target: String,
        host: String,
    },
    UnknownReference(String),
    /// A namespace, `<prefix>/`, which is written with its `/`.
    UnknownNamespace(String),
    UnknownAttributeOf {
        item: Item,
        name: String,
        attribute: String,
    },
    AttributeNotSet {
        item: Item,
        name: String,
        attribute: String,
    },
    UseBinary {
        name: String,
        /// The declaration of the binary.
        declared: Span,
    },
    UseWithTarget {
        /// The declaration of the crate for the target, if it has been declared.
        declared: Option<Span>,
    },
    /// A suggestion to replace the span of the diagnostic.
    Replacement {
        description: &'static str,
        replacement: String,
    },
    /// A suggestion to insert a line before the line of the diagnostic.
    Insertion {
        description: &'static str,
        line: String,
    },
    OverrideAttribute(String),
    RepeatedTarget(String),
    RedeclareCrate {
        name: String,
        target: String,
    },
    UnknownCrate {
        name: String,
        target: String,
    },
    /// The target of a reference to a crate, `<target>/<name>`, which is neither builtin nor declared.
    InvalidTarget(String),
    /// An output of a custom command which is, or is within or contains, the output of something else.
    OverlappingOutput {
        path: String,
        other: String,
        item: Item,
        name: String,
        declared: Span,
    },
    InvalidCrateName {
        name: String,
        character: char,
    },
    /// The value of an `install-mode` attribute, which is not a permission.
    InvalidInstallMode(String),
    /// A keyword argument, `<name>: <value>`, which a function does not take.
    UnknownKeywordArgument {
        function: &'static str,
        name: String,
    },
    RepeatedKeywordArgument(String),
    /// A version, or a requirement for one, given to `semver-compatible`, which is not a semantic version.
    InvalidVersion {
        value: String,
        requirement: bool,
        reason: String,
    },
    /// A program which a wrapper installed by `jig install --proxy` builds, as it or the configuration is out of date,
    /// before running it. It is reported by `jig build --proxy`, with the severity recorded when configuring.
    ProxyBuild {
        program: String,
        /// Whether the configuration is out of date, which is regenerated first.
        configuration: bool,
    },
    /// A file which is installed at the same destination, relative to the prefix, as another.
    InstallConflict {
        path: String,
        item: Item,
        name: String,
        other_item: Item,
        other_name: String,
        declared: Span,
    },
    UnknownTarget {
        target: String,
        suggestion: Option<String>,
    },
    AvailableTargets(Vec<String>),
    MismatchedType {
        value: usize,
        name: String,
    },
}
impl core::fmt::Display for Message {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User(string) | Self::Logged { message: string, .. } => f.write_str(string),
            Self::UnknownSeverity(name) => write!(f, "unknown severity {name:?}"),
            Self::UnknownStatement(name) => write!(f, "unknown statement {name:?}"),
            Self::UnknownMetaStatement(name) => write!(f, "unknown meta-statement \"@{name}\""),
            Self::UnknownFunction(name) => write!(f, "unknown function {name:?}"),
            Self::ExpectedSeparator(close) => write!(f, "expected \", \" or \"{close}\""),
            Self::ArgumentCount { function, expected, found } => {
                let suffix = if expected.singular() { "" } else { "s" };
                let verb = if *found == 1 { "was" } else { "were" };
                write!(f, "function {function:?} takes {expected} argument{suffix}, but {found} {verb} given")
            },
            Self::InvalidProgramName(name) => write!(f, "invalid program name {name:?}, which cannot contain \"/\""),
            Self::ProgramNotFound(name) => write!(f, "program {name:?} not found in PATH"),
            Self::PositionalAfterEnv => write!(f, "program arguments must come before environment variables"),
            Self::RunSpawn { program, error } => write!(f, "failed to run {program:?}: {error}"),
            Self::RunFailed { program, status, .. } => write!(f, "{program:?} failed with {status}"),
            Self::RunOutput { program } => write!(f, "output of {program:?} is not valid unicode"),
            Self::NoneSelected(names) => {
                let names: Vec<_> = names.iter().map(|name| format!("{name:?}")).collect();
                write!(f, "no value for any of the properties {}", names.join(", "))
            },
            Self::EnvironmentVariable { name, present: false } => write!(f, "environment variable {name:?} is not set"),
            Self::EnvironmentVariable { name, present: true } => {
                write!(f, "environment variable {name:?} is not valid unicode")
            },
            Self::Impure(function) => write!(
                f,
                "function {function:?} is impure, as the build is not reconfigured when its value changes",
            ),
            Self::ExpectedStatementAfterMeta => write!(f, "expected statement on the line after a meta-statement"),
            Self::ThenWithoutStatement => write!(f, "\"@then\" must be directly after a statement"),
            Self::ElseWithoutIf(name) => write!(f, "\"@{name}\" must follow a conditional statement"),
            Self::ElseNotFirst(name) => write!(f, "\"@{name}\" must be the first meta-statement of a statement"),
            Self::UnsetProperty(name) => write!(f, "no value for property {name:?}"),
            Self::ExpectedIdentifier => write!(f, "expected identifier"),
            Self::InvalidIdentifier { name, character } => write!(f, "invalid character {character:?} in identifier {name:?}"),
            Self::ExpectedString => write!(f, "expected string"),
            Self::UnterminatedString => write!(f, "multi-line string is not terminated"),
            Self::StringIndentation { expected } => {
                write!(f, "lines of a multi-line string must be indented by {expected} spaces, or end it")
            },
            Self::InvalidStringTemplate { .. } => write!(f, "invalid string template"),
            Self::InvalidInteger => write!(f, "invalid integer"),
            Self::InvalidOperand { ty, expected, coerce: false, .. } => write!(f, "{ty} value used where {expected} is required"),
            Self::InvalidOperand { ty, expected, coerce: true, .. } => write!(f, "{ty} value cannot be coerced to {expected}"),
            Self::MismatchedOperands { lhs, rhs, .. } => write!(f, "mismatched operand types {lhs} and {rhs}"),
            Self::IntegerOverflow => write!(f, "integer overflow"),
            Self::ShiftAmount => write!(f, "shift amount must be from 0 to 63"),
            Self::DivisionByZero => write!(f, "division by zero"),
            Self::InvalidType(ty) => write!(f, "invalid data type {ty:?}"),
            Self::ExpectedPath => write!(f, "expected path"),
            Self::ExpectedExpression => write!(f, "expected expression"),
            Self::ExpectedExact(s) => write!(f, "expected {s:?}"),
            Self::RepeatedUse { path, .. } => write!(f, "file {path:?} is already used"),
            Self::ReadFailure(path, error) => write!(f, "failed to read {path:?}: {error}"),
            Self::UndeclaredProperty(name) => write!(f, "property {name:?} is undeclared"),
            Self::RedeclareProperty { name, builtin: false, .. } => write!(f, "redeclaration of property {name:?}"),
            Self::RedeclareProperty { name, builtin: true, .. } => write!(f, "redeclaration of builtin property {name:?}"),
            Self::ReadOnlyProperty { name, builtin: false, .. } => write!(f, "cannot set read-only property {name:?}"),
            Self::ReadOnlyProperty { name, builtin: true, .. } => write!(f, "cannot set read-only builtin property {name:?}"),
            Self::UnknownDeclarationAttribute { name, .. } => write!(f, "unknown declare attribute {name:?}"),
            Self::UnknownCrateAttribute { name, .. } => write!(f, "unknown crate attribute {name:?}"),
            Self::UnknownAttribute { keyword, name } => write!(f, "unknown {keyword} attribute {name:?}"),
            Self::RepeatedAttribute(name) => write!(f, "attribute {name:?} cannot be repeated"),
            Self::MissingAttribute(name) => write!(f, "missing required attribute {name:?}"),
            Self::RedeclareCustom { item, name, .. } => write!(f, "redeclaration of {item} {name:?}"),
            Self::NewlineInArgument => write!(f, "command arguments cannot contain a newline"),
            Self::EmptySeparator => write!(f, "the separator cannot be empty"),
            Self::NoValues => write!(f, "there are no values to compare"),
            Self::RepeatedKey(key) => write!(f, "key {key:?} is repeated in the dict"),
            Self::ExpectedDictEntry => write!(f, "expected an entry of a dict, as `name: value`"),
            Self::MissingKey(key) => write!(f, "no entry for key {key:?}"),
            Self::IndexOutOfRange { len } => write!(f, "index out of range for a list of length {len}"),
            Self::InvalidCrateType(name) => {
                write!(f, "invalid crate type {name:?}, which must be \"lib\", \"bin\" or \"proc-macro\"")
            },
            Self::ProcMacroTarget { target, host } => write!(
                f,
                "procedural macros cannot be compiled for target {target:?}, only the host target {host:?}",
            ),
            Self::UseWithTarget { .. } => write!(f, "cannot use a crate of a different target"),
            Self::Replacement { description, .. } | Self::Insertion { description, .. } => f.write_str(description),
            Self::UnknownReference(name) => write!(f, "no property, custom command, target or test named {name:?}"),
            Self::UnknownNamespace(prefix) => write!(f, "no property or target has the namespace `{prefix}`"),
            Self::UnknownAttributeOf { item, name, attribute } => {
                write!(f, "unknown attribute {attribute:?} of {item} {name:?}")
            },
            Self::AttributeNotSet { item, name, attribute } => {
                write!(f, "attribute {attribute:?} of {item} {name:?} is not set")
            },
            Self::UseBinary { name, .. } => write!(f, "crate {name:?} is a binary, which cannot be used by another crate"),
            Self::BuiltinTargetName(name) => {
                write!(f, "{name:?} is the name of a builtin target, which can be changed with `override`")
            },
            Self::OverrideAttribute(name) => write!(f, "attribute {name:?} cannot be used with `override`"),
            Self::RepeatedTarget(target) => write!(f, "target {target:?} is listed more than once"),
            Self::TestTarget { target, host } => write!(
                f,
                "tests for target {target:?} cannot be run, only those for targets which the host {host:?} can run, or \
                 a target with `test`",
            ),
            Self::UnknownCrate { name, target } => write!(f, "no crate {name:?} for target {target:?}"),
            Self::InvalidTarget(target) => write!(f, "`{target}` is not a valid target"),
            Self::OverlappingOutput { path, other, item, name, .. } if path == other => {
                write!(f, "output {path:?} of the custom command is also the output of the {item} {name:?}")
            },
            Self::OverlappingOutput { path, other, item, name, .. } => {
                write!(f, "output {path:?} of the custom command overlaps {other:?}, the output of the {item} {name:?}")
            },
            Self::InvalidCrateName { name, character } => write!(f, "invalid character {character:?} in crate name {name:?}"),
            Self::UnknownKeywordArgument { function, name } => {
                write!(f, "function {function:?} has no keyword argument {name:?}")
            },
            Self::RepeatedKeywordArgument(name) => write!(f, "keyword argument {name:?} cannot be repeated"),
            Self::InvalidVersion { value, requirement, reason } => {
                let kind = if *requirement { "version requirement" } else { "version" };
                write!(f, "invalid {kind} {value:?}, as {reason}")
            },
            Self::InvalidInstallMode(mode) => {
                write!(f, "invalid install mode {mode}, which must be permissions from 0 to 0o7777")
            },
            Self::ProxyBuild { program, configuration: false } => write!(f, "building {program}"),
            Self::ProxyBuild { program, configuration: true } => {
                write!(f, "regenerating the configuration and building {program}")
            },
            Self::InstallConflict { path, item, name, other_item, other_name, .. } => write!(
                f,
                "the {item} {name:?} installs {path:?}, which the {other_item} {other_name:?} also installs",
            ),
            Self::RedeclareCrate { name, target } => write!(f, "redeclaration of crate {name:?} for target {target:?}"),
            Self::UnknownTarget { target, suggestion: None } => write!(f, "unknown target {target:?}"),
            Self::UnknownTarget { target, suggestion: Some(suggestion) } => {
                write!(f, "unknown target {target:?}, did you mean {suggestion:?}?")
            },
            Self::AvailableTargets(targets) => {
                write!(f, "available targets are:")?;
                for target in targets {
                    write!(f, "\n{target}")?;
                }
                Ok(())
            },
            Self::MismatchedType { name, .. } => write!(f, "invalid data type for property {name:?}"),
        }
    }
}

/// Returns the candidate closest to `s`, if it is close enough to be a likely misspelling.
pub(super) fn suggest(s: &str, candidates: &[String]) -> Option<String> {
    let (distance, candidate) = candidates.iter()
        .map(|candidate| (edit_distance(s, candidate), candidate))
        .min_by_key(|(distance, _)| *distance)?;
    (distance <= s.chars().count() / 3).then(|| candidate.clone())
}

/// Returns the Levenshtein distance between two strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    // the distances from the prefix of `a` processed so far to each prefix of `b`
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitute = diagonal + (ca != cb) as usize;
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

impl Message {
    /// The identifier of the kind of message, by which its severity is overridden.
    pub fn id(&self) -> &str {
        match self {
            Self::User { .. } => "user",
            Self::Logged { id, .. } => id,
            Self::UnknownSeverity { .. } => "unknown-severity",
            Self::UnknownStatement { .. } => "unknown-statement",
            Self::UnknownMetaStatement { .. } => "unknown-meta-statement",
            Self::ExpectedStatementAfterMeta { .. } => "expected-statement-after-meta",
            Self::UnknownFunction { .. } => "unknown-function",
            Self::ExpectedSeparator { .. } => "expected-separator",
            Self::ArgumentCount { .. } => "argument-count",
            Self::NoneSelected { .. } => "none-selected",
            Self::PositionalAfterEnv { .. } => "positional-after-env",
            Self::InvalidProgramName { .. } => "invalid-program-name",
            Self::ProgramNotFound { .. } => "program-not-found",
            Self::RunSpawn { .. } => "run-spawn",
            Self::RunFailed { .. } => "run-failed",
            Self::RunOutput { .. } => "run-output",
            Self::EnvironmentVariable { .. } => "environment-variable",
            Self::Impure(_) => "impure",
            Self::ElseWithoutIf { .. } => "else-without-if",
            Self::ThenWithoutStatement { .. } => "then-without-statement",
            Self::ElseNotFirst { .. } => "else-not-first",
            Self::UnsetProperty { .. } => "unset-property",
            Self::ExpectedIdentifier { .. } => "expected-identifier",
            Self::InvalidIdentifier { .. } => "invalid-identifier",
            Self::ExpectedString { .. } => "expected-string",
            Self::UnterminatedString { .. } => "unterminated-string",
            Self::StringIndentation { .. } => "string-indentation",
            Self::InvalidStringTemplate { .. } => "invalid-string-template",
            Self::InvalidInteger { .. } => "invalid-integer",
            Self::InvalidOperand { .. } => "invalid-operand",
            Self::MismatchedOperands { .. } => "mismatched-operands",
            Self::IntegerOverflow { .. } => "integer-overflow",
            Self::ShiftAmount { .. } => "shift-amount",
            Self::DivisionByZero { .. } => "division-by-zero",
            Self::InvalidType { .. } => "invalid-type",
            Self::ExpectedPath { .. } => "expected-path",
            Self::ExpectedExpression { .. } => "expected-expression",
            Self::ReadFailure { .. } => "read-failure",
            Self::RepeatedUse { .. } => "repeated-use",
            Self::ExpectedExact { .. } => "expected-exact",
            Self::UndeclaredProperty { .. } => "undeclared-property",
            Self::RedeclareProperty { .. } => "redeclare-property",
            Self::ReadOnlyProperty { .. } => "read-only-property",
            Self::UnknownDeclarationAttribute { .. } => "unknown-declaration-attribute",
            Self::UnknownCrateAttribute { .. } => "unknown-crate-attribute",
            Self::UnknownAttribute { .. } => "unknown-attribute",
            Self::RepeatedAttribute { .. } => "repeated-attribute",
            Self::MissingAttribute { .. } => "missing-attribute",
            Self::RedeclareCustom { .. } => "redeclare-custom",
            Self::NewlineInArgument { .. } => "newline-in-argument",
            Self::TestTarget { .. } => "test-target",
            Self::BuiltinTargetName { .. } => "builtin-target-name",
            Self::InvalidCrateType { .. } => "invalid-crate-type",
            Self::EmptySeparator { .. } => "empty-separator",
            Self::NoValues { .. } => "no-values",
            Self::RepeatedKey { .. } => "repeated-key",
            Self::ExpectedDictEntry { .. } => "expected-dict-entry",
            Self::MissingKey { .. } => "missing-key",
            Self::IndexOutOfRange { .. } => "index-out-of-range",
            Self::ProcMacroTarget { .. } => "proc-macro-target",
            Self::UnknownReference { .. } => "unknown-reference",
            Self::UnknownNamespace(_) => "unknown-namespace",
            Self::UnknownAttributeOf { .. } => "unknown-attribute-of",
            Self::AttributeNotSet { .. } => "attribute-not-set",
            Self::UseBinary { .. } => "use-binary",
            Self::UseWithTarget { .. } => "use-with-target",
            Self::Replacement { .. } => "replacement",
            Self::Insertion { .. } => "insertion",
            Self::OverrideAttribute { .. } => "override-attribute",
            Self::RepeatedTarget { .. } => "repeated-target",
            Self::RedeclareCrate { .. } => "redeclare-crate",
            Self::UnknownCrate { .. } => "unknown-crate",
            Self::InvalidTarget { .. } => "invalid-target",
            Self::OverlappingOutput { .. } => "overlapping-output",
            Self::InvalidCrateName { .. } => "invalid-crate-name",
            Self::InvalidInstallMode(_) => "invalid-install-mode",
            Self::InvalidVersion { .. } => "invalid-version",
            Self::UnknownKeywordArgument { .. } => "unknown-keyword-argument",
            Self::RepeatedKeywordArgument(_) => "repeated-keyword-argument",
            Self::InstallConflict { .. } => "install-conflict",
            Self::ProxyBuild { .. } => "proxy-build",
            Self::UnknownTarget { .. } => "unknown-target",
            Self::AvailableTargets { .. } => "available-targets",
            Self::MismatchedType { .. } => "mismatched-type",
        }
    }
    /// Whether the message is part of the diagnostic before it, as a suggestion or note about it, so that it has the
    /// severity of that diagnostic overridden with it.
    fn is_attachment(&self) -> bool {
        matches!(self, Self::Replacement { .. } | Self::Insertion { .. } | Self::AvailableTargets(_))
    }
}

/// Returns the severity a diagnostic can be overridden to, as the value of its `diagnostic/<id>` property.
pub fn severity_override(name: &str) -> Option<Severity> {
    match name {
        "fatal" => Some(Severity::Fatal),
        "error" => Some(Severity::Error),
        "warning" => Some(Severity::Warning),
        "note" => Some(Severity::Note),
        "allow" => Some(Severity::Allow),
        _ => None,
    }
}

impl Project {
    /// Declare the property of each diagnostic, `diagnostic/<id>`, which has no value until it is set.
    pub fn declare_diagnostic_overrides(&mut self) {
        for id in DIAGNOSTIC_IDS {
            self.declare_diagnostic_override(id);
        }
    }
    /// Declare the property of a diagnostic, `diagnostic/<id>`, unless it has been declared, such as for the ID of a
    /// `log` statement which is used again.
    pub(super) fn declare_diagnostic_override(&mut self, id: &str) {
        let name = format!("diagnostic/{id}");
        if self.properties_idx.get(&name).is_some_and(|&property| self.properties[property].is_some()) {
            return;
        }
        let info = "the severity of the diagnostic where it is reported: 'fatal', 'error', 'warning', 'note' or 'allow'";
        self.declare(name.clone(), Some(Property::new(name, Type::String).info(info)));
    }
    /// Give the diagnostics reported since this was last called the severities of their `diagnostic/<id>` properties.
    pub(super) fn override_severities(&mut self) {
        for index in self.overridden..self.diagnostics.len() {
            let message = &self.diagnostics[index].message;
            let severity = if message.is_attachment() {
                // what is attached to a diagnostic is hidden with it, but otherwise keeps its own severity
                let allowed = index > 0 && self.diagnostics[index - 1].severity == Severity::Allow;
                allowed.then_some(Severity::Allow)
            } else {
                self.overridden_severity(message.id())
            };
            if let Some(severity) = severity {
                self.diagnostics[index].severity = severity;
            }
            if self.diagnostics[index].severity == Severity::Fatal {
                self.fatal = true;
            }
        }
        self.overridden = self.diagnostics.len();
    }
    /// Returns the severity of a diagnostic which is reported after configuring, such as while building, which is
    /// set by its property where the configuration ends, or `default`.
    pub fn deferred_severity(&self, id: &str, default: Severity) -> Severity {
        self.overridden_severity(id).unwrap_or(default)
    }
    /// Returns the severity which the property of a diagnostic sets, if it is set.
    fn overridden_severity(&self, id: &str) -> Option<Severity> {
        let &value = self.values_idx.get(&format!("diagnostic/{id}"))?;
        let Data::Str(name) = &self.values[value].data else {
            return None;
        };
        severity_override(name)
    }
    /// Write a diagnostic in a human readable form.
    pub fn write_diagnostic(&self, w: &mut dyn std::io::Write, diagnostic: &Diagnostic, pretty: bool) -> std::io::Result<()> {
        let severity = diagnostic.severity;
        let mut style_accent = "";
        let mut style_white = "";
        let mut style_note = "";
        let mut style_reset = "";
        if pretty {
            style_accent = severity.accent();
            style_white = "\x1b[97m";
            style_note = Severity::Note.accent();
            style_reset = "\x1b[0m";
        }

        // the lines of a multi-line message, such as the output of a program, are aligned with the first
        let message = diagnostic.message.to_string();
        let mut lines = message.lines();
        // the message of a `log` statement, or what is attached to another diagnostic, is shown without its identifier
        let heading = match &diagnostic.message {
            Message::User(_) => severity.to_string(),
            message if message.is_attachment() => severity.to_string(),
            message => format!("{severity}[{}]", message.id()),
        };
        writeln!(w, "{style_accent}{heading}{style_white}: {}{style_reset}", lines.next().unwrap_or_default())?;
        let indent = " ".repeat(heading.len() + 2);
        for line in lines {
            if line.is_empty() {
                writeln!(w)?;
            } else {
                writeln!(w, "{indent}{style_white}{line}{style_reset}")?;
            }
        }
        if let Some(span) = diagnostic.span.as_ref() {
            match &diagnostic.message {
                // the line is shown with the replacement
                Message::Replacement { replacement, .. } => {
                    write_replacement(w, &self.files[span.file], span, replacement, style_accent, style_reset)?;
                },
                // the line is shown after the inserted one
                Message::Insertion { line, .. } => {
                    write_insertion(w, &self.files[span.file], span, line, style_accent, style_reset)?;
                },
                _ => write_location(w, &self.files[span.file], span, style_accent, style_reset)?,
            }
        }
        match &diagnostic.message {
            Message::RedeclareProperty { id, .. } => {
                if let Some(span) = &self.properties[*id].as_ref().unwrap().span {
                    writeln!(w, "{style_note}note{style_white}: previously declared here{style_reset}")?;
                    write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                }
            },
            Message::ReadOnlyProperty { id, assigned, .. } => {
                if let Some(span) = assigned {
                    writeln!(w, "{style_note}note{style_white}: value assigned here{style_reset}")?;
                    write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                }
                if let Some(span) = &self.properties[*id].as_ref().unwrap().span {
                    writeln!(w, "{style_note}note{style_white}: declared read-only here{style_reset}")?;
                    write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                }
            },
            Message::UseBinary { declared, .. }
            | Message::UseWithTarget { declared: Some(declared) }
            | Message::OverlappingOutput { declared, .. }
            | Message::InstallConflict { declared, .. } => {
                writeln!(w, "{style_note}note{style_white}: declared here{style_reset}")?;
                write_location(w, &self.files[declared.file], declared, style_note, style_reset)?;
            },
            Message::RedeclareCustom { previous, .. } => {
                writeln!(w, "{style_note}note{style_white}: previously declared here{style_reset}")?;
                write_location(w, &self.files[previous.file], previous, style_note, style_reset)?;
            },
            Message::RepeatedUse { file, .. } => {
                if let Some(span) = &self.files[*file].used {
                    writeln!(w, "{style_note}note{style_white}: first used here{style_reset}")?;
                    write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                }
            },
            Message::RedeclareCrate { name, target } => {
                let span = &self.crates[&(name.clone(), target.clone())].name_span;
                writeln!(w, "{style_note}note{style_white}: previously declared here{style_reset}")?;
                write_location(w, &self.files[span.file], span, style_note, style_reset)?;
            },
            Message::UnknownDeclarationAttribute { property, .. } => {
                let property = self.properties[*property].as_ref().unwrap();
                if let Some(span) = &property.span {
                    writeln!(w, "{style_note}note{style_white}: attribute specified for declaration of {:?}{style_reset}", property.name)?;
                    write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                }
            },
            Message::RunFailed { stderr, .. } if !stderr.is_empty() => {
                // shown like a location, with a gutter as wide as the one before it
                let width = diagnostic.span.map_or(1, |span| {
                    self.files[span.file].position(span.offset).0.to_string().len()
                });
                let gutter = " ".repeat(width);
                writeln!(w, "{gutter}--> stderr")?;
                writeln!(w, "{gutter} |")?;
                for line in stderr.lines() {
                    if line.is_empty() {
                        writeln!(w, "{gutter} |")?;
                    } else {
                        writeln!(w, "{gutter} | {line}")?;
                    }
                }
                writeln!(w, "{gutter} |")?;
            },
            Message::InvalidOperand { ty, origin: Some(span), .. } => {
                writeln!(w, "{style_note}note{style_white}: {ty} value set here{style_reset}")?;
                write_location(w, &self.files[span.file], span, style_note, style_reset)?;
            },
            Message::MismatchedOperands { lhs, rhs, lhs_origin, rhs_origin } => {
                for (ty, origin) in [(lhs, lhs_origin), (rhs, rhs_origin)] {
                    if let Some(span) = origin {
                        writeln!(w, "{style_note}note{style_white}: {ty} value set here{style_reset}")?;
                        write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                    }
                }
            },
            Message::MismatchedType { value, .. } => {
                let value = &self.values[*value];
                // the value of a referenced property was set elsewhere
                if let Some(origin) = value.origin && value.data_span.is_some_and(|span| span.offset != origin.offset || span.file != origin.file) {
                    writeln!(w, "{style_note}note{style_white}: {} value set here{style_reset}", value.data.type_name())?;
                    write_location(w, &self.files[origin.file], &origin, style_note, style_reset)?;
                }
                if let Some(span) = &self.properties[value.property].as_ref().unwrap().span {
                    writeln!(w, "{style_note}note{style_white}: declared here{style_reset}")?;
                    write_location(w, &self.files[span.file], span, style_note, style_reset)?;
                }
            },
            _ => (),
        }
        if let Some(property) = self.message_property(&diagnostic.message) && let Some(info) = &property.info {
            writeln!(w, "{style_note}note{style_white}: {}: {info}{style_reset}", property.name)?;
        }
        Ok(())
    }
    /// Returns the property a message is about, if it has been declared.
    fn message_property(&self, message: &Message) -> Option<&Property> {
        let id = match message {
            Message::RedeclareProperty { id, .. } | Message::ReadOnlyProperty { id, .. } => *id,
            Message::UnknownDeclarationAttribute { property, .. } => *property,
            Message::MismatchedType { value, .. } => self.values[*value].property,
            Message::UnsetProperty(name) => *self.properties_idx.get(name)?,
            _ => return None,
        };
        self.properties[id].as_ref()
    }
}

/// Write the location of a span followed by its first line with the span replaced, and the replacement underlined.
fn write_replacement(
    w: &mut dyn std::io::Write,
    file: &File,
    span: &Span,
    replacement: &str,
    style_accent: &str,
    style_reset: &str,
) -> std::io::Result<()> {
    let (line, col) = file.position(span.offset);
    let line = line.to_string();
    let gutter = " ".repeat(line.len());
    writeln!(w, "{gutter}--> {}:{line}:{col}", file.path.display())?;

    let start = span.offset - (col - 1);
    let rest = &file.data[start..];
    let text = rest[..rest.find('\n').unwrap_or(rest.len())].trim_end_matches('\r');
    let (before, after) = text.split_at((col - 1).min(text.len()));
    let after = &after[span.len.min(after.len())..];
    let indent = " ".repeat(before.chars().count());
    let width = replacement.chars().count().max(1);
    writeln!(w, "{gutter} |")?;
    writeln!(w, "{line} | {before}{replacement}{after}")?;
    writeln!(w, "{gutter} | {indent}{style_accent}{}{style_reset}", "~".repeat(width))
}

/// Write the location of a span followed by a line inserted before its line, and its line.
fn write_insertion(
    w: &mut dyn std::io::Write,
    file: &File,
    span: &Span,
    inserted: &str,
    style_accent: &str,
    style_reset: &str,
) -> std::io::Result<()> {
    let (line, col) = file.position(span.offset);
    let line = line.to_string();
    let gutter = " ".repeat(line.len());
    writeln!(w, "{gutter}--> {}:{line}:{col}", file.path.display())?;

    let start = span.offset - (col - 1);
    let rest = &file.data[start..];
    let text = rest[..rest.find('\n').unwrap_or(rest.len())].trim_end_matches('\r');
    writeln!(w, "{gutter} |")?;
    writeln!(w, "{line} {style_accent}+ {inserted}{style_reset}")?;
    writeln!(w, "{line} | {text}")
}

/// Write the location of a span followed by its first line with the span underlined.
fn write_location(
    w: &mut dyn std::io::Write,
    file: &File,
    span: &Span,
    style_accent: &str,
    style_reset: &str,
) -> std::io::Result<()> {
    let (line, col) = file.position(span.offset);
    let line = line.to_string();
    let gutter = " ".repeat(line.len());
    writeln!(w, "{gutter}--> {}:{line}:{col}", file.path.display())?;

    let start = span.offset - (col - 1);
    let rest = &file.data[start..];
    let text = rest[..rest.find('\n').unwrap_or(rest.len())].trim_end_matches('\r');
    let (before, after) = text.split_at((col - 1).min(text.len()));
    let indent = " ".repeat(before.len());
    let end = span.len.min(after.len());
    let width = after[..end].chars().count().max(1);
    writeln!(w, "{gutter} |")?;
    writeln!(w, "{line} | {text}")?;
    writeln!(w, "{gutter} | {indent}{style_accent}{}{style_reset}", "^".repeat(width))
}
