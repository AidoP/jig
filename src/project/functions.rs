//! The functions which can be called in an expression.

use std::path::{Path, PathBuf};

use super::customs::leading_reference;
use super::diagnostic::{Diagnostic, Message, Severity};
use super::statements::Name;
use super::syntax::{Parser, Span, argument_end, is_named_entry};
use super::value::{Coercion, Data, Operand};
use super::{Project, normalize, semver};

/// A function which can be called in an expression.
struct Function {
    name: &'static str,
    /// The number of arguments the function takes.
    arguments: Arguments,
}

/// The functions which can be called in an expression.
const FUNCTIONS: &[Function] = &[
    Function { name: "upper", arguments: Arguments::Exactly(1) },
    Function { name: "lower", arguments: Arguments::Exactly(1) },
    Function { name: "env", arguments: Arguments::Exactly(1) },
    Function { name: "path", arguments: Arguments::Exactly(1) },
    Function { name: "read", arguments: Arguments::Exactly(1) },
    Function { name: "exists", arguments: Arguments::Exactly(1) },
    Function { name: "which", arguments: Arguments::Exactly(1) },
    Function { name: "len", arguments: Arguments::Exactly(1) },
    Function { name: "keys", arguments: Arguments::Exactly(1) },
    Function { name: "values", arguments: Arguments::Exactly(1) },
    Function { name: "unique", arguments: Arguments::AtLeast(1) },
    Function { name: "sort", arguments: Arguments::AtLeast(1) },
    Function { name: "starts-with", arguments: Arguments::Exactly(2) },
    Function { name: "ends-with", arguments: Arguments::Exactly(2) },
    Function { name: "contains", arguments: Arguments::Exactly(2) },
    Function { name: "join", arguments: Arguments::Exactly(2) },
    Function { name: "split", arguments: Arguments::Exactly(2) },
    Function { name: "semver-compatible", arguments: Arguments::Exactly(2) },
    Function { name: "get", arguments: Arguments::Either(2, 3) },
    Function { name: "select", arguments: Arguments::AtLeast(1) },
    Function { name: "run", arguments: Arguments::AtLeast(1) },
    Function { name: "min", arguments: Arguments::AtLeast(1) },
    Function { name: "max", arguments: Arguments::AtLeast(1) },
    // the first argument is evaluated with its errors as warnings
    Function { name: "try", arguments: Arguments::Exactly(2) },
];

/// The number of arguments a function takes.
#[derive(Clone, Copy)]
pub enum Arguments {
    Exactly(usize),
    /// Either of two numbers of arguments.
    Either(usize, usize),
    AtLeast(usize),
}
impl Arguments {
    /// Returns whether a function can be called with `count` arguments.
    fn accepts(self, count: usize) -> bool {
        match self {
            Self::Exactly(expected) => count == expected,
            Self::Either(first, second) => count == first || count == second,
            Self::AtLeast(min) => count >= min,
        }
    }
    /// Returns whether the number ends with 1, so that it is followed by "argument" rather than "arguments".
    pub fn singular(self) -> bool {
        matches!(self, Self::Exactly(1) | Self::Either(_, 1) | Self::AtLeast(1))
    }
}
impl core::fmt::Display for Arguments {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exactly(expected) => write!(f, "{expected}"),
            Self::Either(first, second) => write!(f, "{first} or {second}"),
            Self::AtLeast(min) => write!(f, "at least {min}"),
        }
    }
}

/// An argument of a function which takes keyword arguments, `<name>: <value>`, such as the environment variables of
/// `run`.
enum Argument {
    Positional(Operand),
    Named { name: String, span: Span, value: Operand },
}

impl Project {
    /// Take the arguments of a function call, and apply the function.
    ///
    /// Arguments are separated by `, ` and are expressions, except for `select`, which takes property names.
    pub(super) fn parse_call(&mut self, parser: &mut Parser, name: &str, name_span: Span) -> Option<Operand> {
        parser.expect('(');
        let Some(function) = FUNCTIONS.iter().find(|function| function.name == name) else {
            self.diagnostics.push(Diagnostic {
                span: Some(name_span),
                message: Message::UnknownFunction(name.into()),
                severity: Severity::Error,
            });
            return None;
        };
        let expected = function.arguments;
        if name == "try" {
            return self.parse_try(parser, name_span);
        }
        if name == "run" {
            let arguments = self.parse_arguments(parser, ')', Self::parse_argument)?;
            let span = parser.span_from(name_span.offset);
            self.impure("run", span);
            return self.run(arguments, span);
        }
        if name == "semver-compatible" {
            let arguments = self.parse_arguments(parser, ')', Self::parse_argument)?;
            let span = parser.span_from(name_span.offset);
            let data = self.semver_compatible(arguments, expected, span)?;
            return Some(Operand { data, span, coerce: Coercion::Strict, origin: None });
        }
        if name == "select" {
            let names = self.parse_arguments(parser, ')', |this, parser| {
                this.parse_ident(parser).map(|(name, span)| (name.to_string(), span))
            })?;
            let span = parser.span_from(name_span.offset);
            if names.is_empty() {
                self.argument_count(name, expected, 0, span);
                return None;
            }
            return self.select(&names, span);
        }
        let mut arguments = self.parse_arguments(parser, ')', |this, parser| this.parse_binary(parser, 0))?.into_iter();
        let span = parser.span_from(name_span.offset);
        if !expected.accepts(arguments.len()) {
            self.argument_count(name, expected, arguments.len(), span);
            return None;
        }
        let data = match name {
            "upper" => Data::Str(self.str_operand(arguments.next()?)?.to_uppercase()),
            // the number of items in a list, or bytes in a string
            // the item at an index, or the default if there is one when the index is out of range
            "get" => {
                let collection = arguments.next()?;
                let key = arguments.next()?;
                let default = arguments.next().map(|default| default.data);
                self.get(collection, key, default)?
            },
            "min" | "max" => {
                let mut values = values(arguments);
                let Some(first) = values.next() else {
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::NoValues,
                        severity: Severity::Error,
                    });
                    return None;
                };
                let mut result = self.integer_operand(first)?;
                for value in values {
                    let value = self.integer_operand(value)?;
                    let (value_span, value_coerce) = (value.span, value.coerce);
                    let (result_span, result_coerce) = (result.span, result.coerce);
                    let (current, other) = self.unify_sign(result, value, span)?;
                    let ordering = match (&current, &other) {
                        (Data::Unsigned(lhs), Data::Unsigned(rhs)) => lhs.cmp(rhs),
                        (Data::Signed(lhs), Data::Signed(rhs)) => lhs.cmp(rhs),
                        _ => unreachable!(),
                    };
                    let replace = if name == "min" { ordering.is_gt() } else { ordering.is_lt() };
                    // the result keeps the type both are unified to
                    result = if replace {
                        Operand { data: other, span: value_span, coerce: value_coerce, origin: None }
                    } else {
                        Operand { data: current, span: result_span, coerce: result_coerce, origin: None }
                    };
                }
                result.data
            },
            "join" => {
                let items = self.list_operand(arguments.next()?)?;
                let separator = self.str_operand(arguments.next()?)?;
                let items: Vec<_> = items.iter().map(Data::coerce_string).collect();
                Data::Str(items.join(&separator))
            },
            "split" => {
                let string = self.str_operand(arguments.next()?)?;
                let separator = arguments.next()?;
                let separator_span = separator.span;
                let separator = self.str_operand(separator)?;
                if separator.is_empty() {
                    self.diagnostics.push(Diagnostic {
                        span: Some(separator_span),
                        message: Message::EmptySeparator,
                        severity: Severity::Error,
                    });
                    return None;
                }
                Data::List(string.split(&separator).map(|item| Data::Str(item.into())).collect())
            },
            "len" => {
                let operand = arguments.next()?;
                match &operand.data {
                    Data::Str(string) => Data::Unsigned(string.len() as u64),
                    Data::List(items) => Data::Unsigned(items.len() as u64),
                    Data::Dict(entries) => Data::Unsigned(entries.len() as u64),
                    _ => {
                        self.invalid_operand(&operand, "list, dict or str", false);
                        return None;
                    },
                }
            },
            "contains" => {
                let collection = arguments.next()?;
                let value = arguments.next()?;
                Data::Bool(self.contains(collection, value)?)
            },
            // the first of each value which is equal to others, in order
            "unique" => {
                let mut unique = Vec::new();
                for value in values(arguments) {
                    if !unique.contains(&value.data) {
                        unique.push(value.data);
                    }
                }
                Data::List(unique)
            },
            "sort" => Data::List(self.sort(values(arguments).collect(), span)?),
            "keys" | "values" => {
                let operand = arguments.next()?;
                let Data::Dict(entries) = operand.data else {
                    self.invalid_operand(&operand, "dict", false);
                    return None;
                };
                let mut items = Vec::new();
                for (key, value) in entries {
                    items.push(if name == "keys" { Data::Str(key) } else { value });
                }
                Data::List(items)
            },
            // relative to the directory of the file, where an absolute path stays absolute
            "path" => {
                let path = self.str_operand(arguments.next()?)?;
                Data::Str(normalize(&self.file(name_span.file).relative_path(path)).display().to_string())
            },
            "which" => {
                let argument = arguments.next()?;
                let argument_span = argument.span;
                let name = self.str_operand(argument)?;
                match self.which(&name) {
                    Ok(path) => Data::Str(path.display().to_string()),
                    Err(message) => {
                        self.diagnostics.push(Diagnostic {
                            span: Some(argument_span),
                            message,
                            severity: Severity::Error,
                        });
                        return None;
                    },
                }
            },
            "exists" => {
                let path = self.str_operand(arguments.next()?)?;
                let path = normalize(&self.file(name_span.file).relative_path(path));
                self.depend_on_path(&path);
                Data::Bool(path.exists())
            },
            "read" => {
                let argument = arguments.next()?;
                let argument_span = argument.span;
                let path = self.str_operand(argument)?;
                let path = normalize(&self.file(name_span.file).relative_path(path));
                // depend on the file even if it cannot be read, in case the error is suppressed by `try`
                self.depend_on_path(&path);
                match std::fs::read_to_string(&path) {
                    Ok(contents) => Data::Str(contents.trim_end_matches(['\n', '\r']).into()),
                    Err(error) => {
                        self.diagnostics.push(Diagnostic {
                            span: Some(argument_span),
                            message: Message::ReadFailure(path, error),
                            severity: Severity::Error,
                        });
                        return None;
                    },
                }
            },
            "lower" => Data::Str(self.str_operand(arguments.next()?)?.to_lowercase()),
            "starts-with" | "ends-with" => {
                let string = self.str_operand(arguments.next()?)?;
                let affix = self.str_operand(arguments.next()?)?;
                Data::Bool(if name == "starts-with" { string.starts_with(&affix) } else { string.ends_with(&affix) })
            },
            _ => {
                self.impure("env", span);
                let variable = arguments.next()?;
                let variable_span = variable.span;
                let variable = self.str_operand(variable)?;
                match std::env::var(&variable) {
                    Ok(value) => Data::Str(value),
                    Err(error) => {
                        self.diagnostics.push(Diagnostic {
                            span: Some(variable_span),
                            message: Message::EnvironmentVariable {
                                name: variable,
                                present: matches!(error, std::env::VarError::NotUnicode(_)),
                            },
                            severity: Severity::Error,
                        });
                        return None;
                    },
                }
            },
        };
        Some(Operand { data, span, coerce: Coercion::Strict, origin: None })
    }
    /// Take the arguments of `try`, returning the value of the first, or the default, the second, if evaluating the
    /// first failed.
    ///
    /// Errors from the first argument are reported as warnings instead. As evaluation stops at the first error, the rest
    /// of the argument is skipped to the separator.
    fn parse_try(&mut self, parser: &mut Parser, name_span: Span) -> Option<Operand> {
        let start = parser.offset;
        let first = self.diagnostics.len();
        let operand = if parser.peek() == Some(')') {
            None
        } else {
            self.parse_binary(parser, 0)
        };
        // without the end of the expression, what follows it cannot be found, so the errors stand
        let end = start + argument_end(&parser.data[start..])?;
        if operand.is_some() && parser.offset != end {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_char()),
                message: Message::ExpectedSeparator(')'),
                severity: Severity::Error,
            });
        }
        parser.offset = end;
        let mut operand = operand;
        // a diagnostic overridden to be an error is lowered as well, except for an impure call, which did not fail
        self.override_severities();
        for diagnostic in &mut self.diagnostics[first..] {
            if matches!(diagnostic.severity, Severity::Error) && !matches!(diagnostic.message, Message::Impure(_)) {
                diagnostic.severity = Severity::Warning;
                operand = None;
            }
        }
        // the default is required, and its errors are not suppressed
        if !parser.as_str().starts_with(", ") {
            parser.expect(')');
            self.argument_count("try", Arguments::Exactly(2), 1, parser.span_from(name_span.offset));
            return None;
        }
        parser.offset += 2;
        let default = self.parse_binary(parser, 0)?;
        if !parser.expect(')') {
            self.diagnostics.push(Diagnostic {
                span: Some(parser.span_char()),
                message: Message::ExpectedExact(")"),
                severity: Severity::Error,
            });
            return None;
        }
        let span = parser.span_from(name_span.offset);
        let data = operand.unwrap_or(default).data;
        Some(Operand { data, span, coerce: Coercion::Strict, origin: None })
    }
    /// Returns the item of a list at an index, or the value of a dict for a key, or the default if there is none.
    fn get(&mut self, collection: Operand, key: Operand, default: Option<Data>) -> Option<Data> {
        let key_span = key.span;
        let missing = match &collection.data {
            Data::Dict(entries) => {
                let key = self.str_operand(key)?;
                if let Some(value) = entries.get(&key) {
                    return Some(value.clone());
                }
                Message::MissingKey(key)
            },
            Data::List(items) => {
                let index = match self.integer_operand(key)?.data {
                    Data::Unsigned(i) => usize::try_from(i).ok(),
                    Data::Signed(i) => usize::try_from(i).ok(),
                    _ => unreachable!(),
                };
                // a negative index is out of range
                if let Some(item) = index.and_then(|index| items.get(index)) {
                    return Some(item.clone());
                }
                Message::IndexOutOfRange { len: items.len() }
            },
            _ => {
                self.invalid_operand(&collection, "list or dict", false);
                return None;
            },
        };
        if let Some(default) = default {
            return Some(default);
        }
        self.diagnostics.push(Diagnostic { span: Some(key_span), message: missing, severity: Severity::Error });
        None
    }
    /// Returns values in order, which are all `str`, `bool` or integers, where integers of either sign are compared as
    /// `signed` if any is, as by `min` and `max`.
    fn sort(&mut self, values: Vec<Operand>, span: Span) -> Option<Vec<Data>> {
        let signed = values.iter().all(|value| value.data.is_integer())
            && values.iter().any(|value| matches!(value.data, Data::Signed(_)));
        let mut items = Vec::new();
        for value in values {
            let data = match value.data {
                Data::Unsigned(_) if signed => match value.data.convert_sign() {
                    Some(data) => data,
                    None => {
                        self.diagnostics.push(Diagnostic {
                            span: Some(value.span),
                            message: Message::IntegerOverflow,
                            severity: Severity::Error,
                        });
                        return None;
                    },
                },
                Data::Str(_) | Data::Bool(_) | Data::Unsigned(_) | Data::Signed(_) => value.data,
                _ => {
                    self.invalid_operand(&value, "str, bool or an integer", false);
                    return None;
                },
            };
            items.push((data, value.origin));
        }
        let Some((first, first_origin)) = items.first() else {
            return Some(Vec::new());
        };
        if let Some((other, other_origin)) = items.iter().find(|(item, _)| item.type_name() != first.type_name()) {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::MismatchedOperands {
                    lhs: first.type_name(),
                    rhs: other.type_name(),
                    lhs_origin: *first_origin,
                    rhs_origin: *other_origin,
                },
                severity: Severity::Error,
            });
            return None;
        }
        let mut items: Vec<_> = items.into_iter().map(|(item, _)| item).collect();
        items.sort_by(|lhs, rhs| match (lhs, rhs) {
            (Data::Str(lhs), Data::Str(rhs)) => lhs.cmp(rhs),
            (Data::Bool(lhs), Data::Bool(rhs)) => lhs.cmp(rhs),
            (Data::Unsigned(lhs), Data::Unsigned(rhs)) => lhs.cmp(rhs),
            (Data::Signed(lhs), Data::Signed(rhs)) => lhs.cmp(rhs),
            _ => unreachable!("the items have the same type"),
        });
        Some(items)
    }
    /// Returns whether a list has an item equal to a value, or a dict has an entry for a key.
    fn contains(&mut self, collection: Operand, value: Operand) -> Option<bool> {
        match &collection.data {
            Data::Dict(entries) => {
                let key = self.str_operand(value)?;
                Some(entries.contains_key(&key))
            },
            Data::List(items) => {
                // as with `==`, integers are equal regardless of their sign
                for item in items {
                    if *item == value.data || item.convert_sign().as_ref() == Some(&value.data) {
                        return Some(true);
                    }
                }
                Some(false)
            },
            _ => {
                self.invalid_operand(&collection, "list or dict", false);
                None
            },
        }
    }
    /// Take an argument of `run`, which is an expression, or an environment variable as `name: expression`.
    ///
    /// The name of an environment variable is a name, as taken by [`Self::parse_name`]. As an expression can also be
    /// an identifier or start with `{`, it is only a name if it is followed by `: `.
    fn parse_argument(&mut self, parser: &mut Parser) -> Option<Argument> {
        if !is_named_entry(parser) {
            return Some(Argument::Positional(self.parse_binary(parser, 0)?));
        }
        let Name { name, span, .. } = self.parse_name(parser)?;
        parser.offset += 2;
        Some(Argument::Named { name, span, value: self.parse_binary(parser, 0)? })
    }
    /// Returns whether a version meets a requirement, as cargo compares a version to a dependency's default
    /// requirement, from the arguments of `semver-compatible`, which are the version and the requirement, and
    /// `ignore-pre-release: <bool>`, which compares them by their numbers alone, so that a nightly version of rustc is
    /// compatible as its release would be.
    fn semver_compatible(&mut self, arguments: Vec<Argument>, expected: Arguments, span: Span) -> Option<Data> {
        let mut positional = Vec::new();
        let mut ignore_pre_release = None;
        let mut failed = false;
        for argument in arguments {
            match argument {
                Argument::Positional(operand) => positional.push(operand),
                Argument::Named { name, span, value } if name == "ignore-pre-release" => {
                    if ignore_pre_release.is_some() {
                        self.diagnostics.push(Diagnostic {
                            span: Some(span),
                            message: Message::RepeatedKeywordArgument(name),
                            severity: Severity::Error,
                        });
                        failed = true;
                        continue;
                    }
                    match self.bool_operand(value) {
                        Some(value) => ignore_pre_release = Some(value),
                        None => failed = true,
                    }
                },
                Argument::Named { name, span, .. } => {
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::UnknownKeywordArgument { function: "semver-compatible", name },
                        severity: Severity::Error,
                    });
                    failed = true;
                },
            }
        }
        if positional.len() != 2 {
            self.argument_count("semver-compatible", expected, positional.len(), span);
            return None;
        }
        let mut positional = positional.into_iter();
        let (version, requirement) = (positional.next()?, positional.next()?);
        let spans = [version.span, requirement.span];
        let version = self.str_operand(version);
        let requirement = self.str_operand(requirement);
        let (Some(version), Some(requirement)) = (version, requirement) else {
            return None;
        };
        if failed {
            return None;
        }
        match semver::compatible(&version, &requirement, ignore_pre_release.unwrap_or(false)) {
            Ok(compatible) => Some(Data::Bool(compatible)),
            Err((which, reason)) => {
                let (span, value) = match which {
                    semver::Which::Version => (spans[0], version),
                    semver::Which::Requirement => (spans[1], requirement),
                };
                let requirement = which == semver::Which::Requirement;
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::InvalidVersion { value, requirement, reason },
                    severity: Severity::Error,
                });
                None
            },
        }
    }
    /// Run a program, returning its output without trailing newlines.
    ///
    /// The first argument is the program, followed by its arguments and then any environment variables.
    fn run(&mut self, arguments: Vec<Argument>, span: Span) -> Option<Operand> {
        let mut command_line = Vec::new();
        let mut env = Vec::new();
        let mut failed = false;
        // the path of a program which is a reference, which is already resolved
        let mut program_path = None;
        for argument in arguments {
            if command_line.is_empty() && let Argument::Positional(operand) = &argument
                && let Some(reference) = leading_reference(&operand.data)
            {
                program_path = reference.paths.first().cloned();
            }
            match argument {
                Argument::Positional(operand) if !env.is_empty() => {
                    self.diagnostics.push(Diagnostic {
                        span: Some(operand.span),
                        message: Message::PositionalAfterEnv,
                        severity: Severity::Error,
                    });
                    failed = true;
                },
                // each item of a list is a separate argument
                Argument::Positional(Operand { data: Data::List(items), .. }) => {
                    command_line.extend(Data::List(items).spread());
                },
                Argument::Positional(operand) => match self.str_operand(operand) {
                    Some(string) => command_line.push(string),
                    None => failed = true,
                },
                Argument::Named { name, value: operand, .. } => match self.str_operand(operand) {
                    Some(value) => env.push((name, value)),
                    None => failed = true,
                },
            }
        }
        if failed {
            return None;
        }
        let Some((program, args)) = command_line.split_first() else {
            self.argument_count("run", Arguments::AtLeast(1), 0, span);
            return None;
        };
        // as in `sh`, a program with a `/` is a path, here relative to the file, otherwise it is searched for in `PATH`
        if program_path.is_none() && program.contains('/') {
            program_path = Some(normalize(&self.file(span.file).relative_path(program)));
        }
        let program = &if let Some(mut path) = program_path {
            self.depend_on_path(&path);
            // a path in the same directory must still have a `/`, so that it is not searched for in `PATH`
            if path.is_relative() {
                path = Path::new(".").join(path);
            }
            path.display().to_string()
        } else {
            program.clone()
        };
        let mut command = std::process::Command::new(program);
        // with the toolchain the project is configured with, unless the program's own environment replaces it
        crate::toolchain::apply(&mut command, self.toolchain.as_deref());
        let output = command.args(args).envs(env).stdin(std::process::Stdio::null()).output();
        let message = match output {
            Err(error) => Message::RunSpawn { program: program.clone(), error: error.to_string() },
            Ok(output) if !output.status.success() => Message::RunFailed {
                program: program.clone(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim_end().into(),
            },
            Ok(output) => match String::from_utf8(output.stdout) {
                Ok(stdout) => {
                    let stdout = stdout.trim_end_matches(['\n', '\r']).into();
                    return Some(Operand { data: Data::Str(stdout), span, coerce: Coercion::Strict, origin: None });
                },
                Err(_) => Message::RunOutput { program: program.clone() },
            },
        };
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message,
            severity: Severity::Error,
        });
        None
    }
    /// Report a call to a function whose value can change without the build being reconfigured, with a suggestion to
    /// allow it for the statement with `@with`.
    ///
    /// The function is still applied, in case the diagnostic is allowed by the `diagnostic/impure` property.
    fn impure(&mut self, function: &'static str, span: Span) {
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::Impure(function),
            severity: Severity::Error,
        });
        if let Some(insertion) = self.meta_insertion {
            self.diagnostics.push(Diagnostic {
                span: Some(insertion),
                message: Message::Insertion {
                    description: "allow it for the statement with `@with`",
                    line: "@with diagnostic/impure as 'allow'".into(),
                },
                severity: Severity::Suggestion,
            });
        }
    }
    /// Search the directories in `PATH` for an executable file.
    ///
    /// The directories searched before it is found are dependencies, as the program could be added to one of them.
    pub(super) fn which(&mut self, name: &str) -> Result<PathBuf, Message> {
        use std::os::unix::fs::PermissionsExt;
        if name.is_empty() || name.contains('/') {
            return Err(Message::InvalidProgramName(name.into()));
        }
        let search = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&search) {
            // an empty entry means the working directory, which is not a reliable place to find a program
            if dir.as_os_str().is_empty() {
                continue;
            }
            let path = dir.join(name);
            self.depend(dir);
            if path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0) {
                return Ok(path);
            }
        }
        Err(Message::ProgramNotFound(name.into()))
    }
    /// Add a path which the configuration depends on, if it exists.
    ///
    /// A missing dependency is always out of date, which would regenerate the configuration endlessly, so a path which
    /// does not exist is recorded as missing instead, which is checked before ninja runs.
    fn depend_on_path(&mut self, path: &Path) {
        if path.exists() {
            self.depend(path.to_path_buf());
        } else if !self.missing.iter().any(|missing| missing == path) {
            self.missing.push(path.to_path_buf());
        }
    }
    /// Add a file which the configuration depends on, so that it is regenerated when the file changes.
    pub(super) fn depend(&mut self, path: PathBuf) {
        if !self.dependencies.contains(&path) {
            self.dependencies.push(path);
        }
    }
    /// Take a list of arguments separated by `, `, after the opening parenthesis or bracket, up to the closing one.
    pub(super) fn parse_arguments<'a, T>(
        &mut self,
        parser: &mut Parser<'a>,
        close: char,
        mut parse: impl FnMut(&mut Self, &mut Parser<'a>) -> Option<T>,
    ) -> Option<Vec<T>> {
        let mut arguments = Vec::new();
        if parser.expect(close) {
            return Some(arguments);
        }
        loop {
            arguments.push(parse(self, parser)?);
            if parser.expect(close) {
                return Some(arguments);
            }
            if !parser.as_str().starts_with(", ") {
                self.diagnostics.push(Diagnostic {
                    span: Some(parser.span_char()),
                    message: Message::ExpectedSeparator(close),
                    severity: Severity::Error,
                });
                return None;
            }
            parser.offset += 2;
        }
    }
    fn argument_count(&mut self, function: &str, expected: Arguments, found: usize, span: Span) {
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::ArgumentCount { function: function.into(), expected, found },
            severity: Severity::Error,
        });
    }
    /// Returns the value of the first property which has one.
    ///
    /// Every property must have been declared, or forward declared by a `set`, so that a misspelling is not ignored.
    pub(super) fn select(&mut self, names: &[(String, Span)], span: Span) -> Option<Operand> {
        let mut undeclared = false;
        for (name, name_span) in names {
            if !self.properties_idx.contains_key(name) {
                self.diagnostics.push(Diagnostic {
                    span: Some(*name_span),
                    message: Message::UndeclaredProperty(name.clone()),
                    severity: Severity::Error,
                });
                undeclared = true;
            }
        }
        if undeclared {
            return None;
        }
        let selected = names.iter().find_map(|(name, _)| self.value(name, span.file));
        let Some(value) = selected else {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::NoneSelected(names.iter().map(|(name, _)| name.clone()).collect()),
                severity: Severity::Error,
            });
            return None;
        };
        Some(Operand { data: value.data.clone(), span, coerce: Coercion::Strict, origin: value.origin })
    }
}

/// Returns the values of arguments, where each item of a list is a value, which may be signed or unsigned like a
/// literal.
fn values(arguments: impl Iterator<Item = Operand>) -> impl Iterator<Item = Operand> {
    arguments.flat_map(|argument| match argument.data {
        Data::List(items) => {
            let (span, origin) = (argument.span, argument.origin);
            items.into_iter().map(|data| Operand { data, span, coerce: Coercion::Sign, origin }).collect()
        },
        _ => vec![argument],
    })
}
