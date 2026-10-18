//! The targets crates are compiled for, which are builtin or declared by `target` statements.

use super::diagnostic::{Diagnostic, Item, Message, Severity, suggest};
use super::statements::{Attribute, Name};
use super::syntax::{Parser, Span};
use super::value::Data;
use super::{Project, normalize, probe};

/// An attribute of a `target` statement which replaces a field of its specification.
pub(super) struct TargetOption {
    pub(super) attribute: &'static str,
    /// The field of the specification.
    pub(super) field: &'static str,
    pub(super) ty: OptionType,
}

/// The type of the value of a [`TargetOption`].
#[derive(Clone, Copy)]
pub(super) enum OptionType {
    Str,
    /// A flag, which is set by the attribute without a value.
    Bool,
    /// Features, as a `str` of features separated by `,`, or a list of them.
    Features,
}
impl OptionType {
    /// The type of value expected, as in a diagnostic.
    fn expected(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Str | Self::Features => "str",
        }
    }
}

/// The attributes of a `target` statement which replace a field of its specification.
const TARGET_OPTIONS: &[TargetOption] = &[
    TargetOption { attribute: "env", field: "env", ty: OptionType::Str },
    TargetOption { attribute: "os", field: "os", ty: OptionType::Str },
    TargetOption { attribute: "arch", field: "arch", ty: OptionType::Str },
    TargetOption { attribute: "linker", field: "linker", ty: OptionType::Str },
    // stable codegen options
    TargetOption { attribute: "code-model", field: "code-model", ty: OptionType::Str },
    TargetOption { attribute: "relocation-model", field: "relocation-model", ty: OptionType::Str },
    TargetOption { attribute: "target-cpu", field: "cpu", ty: OptionType::Str },
    TargetOption { attribute: "target-feature", field: "features", ty: OptionType::Features },
    TargetOption { attribute: "linker-flavor", field: "linker-flavor", ty: OptionType::Str },
    TargetOption { attribute: "panic", field: "panic-strategy", ty: OptionType::Str },
    TargetOption { attribute: "no-redzone", field: "disable-redzone", ty: OptionType::Bool },
    // the linker script, rather than its path
    TargetOption { attribute: "link-script", field: "link-script", ty: OptionType::Str },
];

/// Returns the option set by an attribute of a `target` statement, if it sets one.
pub(super) fn target_option(attribute: &str) -> Option<&'static TargetOption> {
    for option in TARGET_OPTIONS {
        if option.attribute == attribute {
            return Some(option);
        }
    }
    None
}

/// A field of a target specification.
pub struct Field {
    pub name: String,
    pub value: Data,
}

/// A target which crates can be compiled for, which is a builtin target of rustc or declared by a `target` statement.
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    /// Whether crates for the target can be tested, from its `test` attribute, or by default, as given by
    /// [`Project::can_test`]. Those for the host target always can be.
    pub test: Option<bool>,
    /// The index of the runner of a test harness for the target, which is its last argument, or none to run it
    /// directly.
    pub runner: Option<usize>,
}

/// Whether a [`Target`] is builtin or has its own specification.
pub enum TargetKind {
    /// A target of rustc, whose `test`, `runner` and linker may be set by a `target` statement with `override`, at
    /// `overridden`.
    Builtin {
        overridden: Option<Span>,
        /// The `linker` and `linker-flavor` fields, which are passed to rustc as the codegen options of the crates for
        /// the target, as its specification is not written.
        fields: Vec<Field>,
    },
    /// A target declared with its own specification, which is written to `<build-dir>/targets/<name>.json`.
    Custom {
        span: Span,
        /// The builtin target whose specification is extended.
        inherit: Option<String>,
        /// Fields of the target specification, in order, which replace those inherited.
        fields: Vec<Field>,
    },
}

impl Target {
    /// A builtin target, before any `target` statement overrides it.
    pub fn builtin(name: String) -> Self {
        Self { name, kind: TargetKind::Builtin { overridden: None, fields: Vec::new() }, test: None, runner: None }
    }
    pub fn is_custom(&self) -> bool {
        matches!(self.kind, TargetKind::Custom { .. })
    }
    /// The `target` statement which declared or overrode the target, if there is one.
    pub(super) fn declaration(&self) -> Option<Span> {
        match self.kind {
            TargetKind::Builtin { overridden, .. } => overridden,
            TargetKind::Custom { span, .. } => Some(span),
        }
    }
    /// Returns the value of a field of the specification, if it is replaced.
    pub(super) fn field(&self, name: &str) -> Option<&Data> {
        let (TargetKind::Builtin { fields, .. } | TargetKind::Custom { fields, .. }) = &self.kind;
        for field in fields {
            if field.name == name {
                return Some(&field.value);
            }
        }
        None
    }
    /// The codegen options of the crates for a builtin target, as `<option>=<value>`, from its linker fields.
    pub fn codegen(&self) -> Vec<String> {
        let TargetKind::Builtin { fields, .. } = &self.kind else {
            return Vec::new();
        };
        fields.iter().map(|field| format!("{}={}", field.name, field.value.coerce_string())).collect()
    }
}

/// The fields of a builtin target's specification which a `target` statement with `override` can set, as they are
/// codegen options too.
const OVERRIDE_FIELDS: &[&str] = &["linker", "linker-flavor"];

impl Project {
    /// Take a custom target, whose attributes are `inherit`, the builtin target whose specification it is based on, and
    /// the fields of its specification which may be replaced.
    ///
    /// Besides `env`, `os`, `arch` and `linker`, the fields are those with stable codegen options, named after the
    /// options rather than the fields.
    pub(super) fn parse_target(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { name, span, .. }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        // a crate for the target is referred to as `<target>/<name>`
        if name.contains('/') {
            parser.skip_statement();
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::InvalidIdentifier { name, character: '/' },
                severity: Severity::Error,
            });
            return;
        }
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        let mut inherit = None;
        let mut fields: Vec<Field> = Vec::new();
        let mut test = None;
        let mut runner = None;
        let mut overrides = false;
        // the attributes which describe the specification, which a builtin target has already, are reported if it turns
        // out to be overridden
        let mut override_errors = Vec::new();
        while let Some(attribute) = self.next_attribute(parser, &[]) {
            if !matches!(attribute.name, "override" | "test" | "runner") && !OVERRIDE_FIELDS.contains(&attribute.name) {
                override_errors.push(Diagnostic {
                    span: Some(attribute.name_span),
                    message: Message::OverrideAttribute(attribute.name.into()),
                    severity: Severity::Error,
                });
            }
            match (attribute.name, attribute.data) {
                ("override", Data::None) => overrides = true,
                ("inherit", Data::Str(base)) => {
                    if self.target(&base).is_some_and(|target| !target.is_custom()) {
                        inherit = Some(base);
                    } else {
                        self.diagnostics.push(Diagnostic {
                            span: Some(attribute.data_span),
                            message: Message::UnknownTarget {
                                suggestion: suggest(&base, &self.builtin_targets()),
                                target: base,
                            },
                            severity: Severity::Error,
                        });
                    }
                },
                // tests are only run where they are allowed, and without a runner, they are run directly
                ("test", Data::None) => test = Some(true),
                ("test", Data::Bool(value)) => test = Some(value),
                ("runner", Data::Str(program)) => runner = Some(self.add_runner(vec![program], parser.file)),
                ("runner", Data::List(items)) if !items.is_empty() && items.iter().all(|item| matches!(item, Data::Str(_))) => {
                    runner = Some(self.add_runner(items.iter().map(Data::coerce_string).collect(), parser.file));
                },
                (name @ ("inherit" | "test" | "runner" | "override"), data) => {
                    let expected = match name {
                        "test" => "bool",
                        "runner" => "str or a list of str",
                        "override" => "none",
                        _ => "str",
                    };
                    self.invalid_operand(&Attribute { data, ..attribute }.operand(), expected, false);
                },
                (name, data) => {
                    let Some(option) = target_option(name) else {
                        self.diagnostics.push(Diagnostic {
                            span: Some(attribute.name_span),
                            message: Message::UnknownAttribute { keyword: Item::Target.keyword(), name: name.into() },
                            severity: Severity::Error,
                        });
                        continue;
                    };
                    let value = match (option.ty, data) {
                        // a flag without a value is set
                        (OptionType::Bool, Data::None) => Data::Bool(true),
                        // features may be given separately
                        (OptionType::Features, Data::List(items))
                            if items.iter().all(|item| matches!(item, Data::Str(_))) =>
                        {
                            let features: Vec<_> = items.iter().map(Data::coerce_string).collect();
                            Data::Str(features.join(","))
                        },
                        (OptionType::Str | OptionType::Features, data @ Data::Str(_))
                        | (OptionType::Bool, data @ Data::Bool(_)) => data,
                        (ty, data) => {
                            self.invalid_operand(&Attribute { data, ..attribute }.operand(), ty.expected(), false);
                            continue;
                        },
                    };
                    match fields.iter_mut().find(|field| field.name == option.field) {
                        Some(field) => field.value = value,
                        None => fields.push(Field { name: option.field.into(), value }),
                    }
                },
            }
        }
        if overrides {
            // a builtin target is only given the attributes which do not describe its specification
            self.diagnostics.extend(override_errors);
        }
        let existing = self.targets.iter().position(|target| target.name == name);
        let message = match existing.map(|index| (index, &self.targets[index].kind)) {
            None if overrides => {
                Message::UnknownTarget { suggestion: suggest(&name, &self.builtin_targets()), target: name }
            },
            None => {
                let kind = TargetKind::Custom { span, inherit, fields };
                self.targets.push(Target { name, kind, test, runner });
                return;
            },
            Some((index, TargetKind::Builtin { overridden: None, .. })) if overrides => {
                // a linker which is a path is relative to the file, as a runner is, and the others are found in `PATH`
                let fields = fields.into_iter().filter(|field| OVERRIDE_FIELDS.contains(&&*field.name));
                let mut fields: Vec<_> = fields.collect();
                for field in &mut fields {
                    if let Data::Str(linker) = &field.value && field.name == "linker" && linker.contains('/') {
                        let path = normalize(&self.file(parser.file).relative_path(linker));
                        field.value = Data::Str(path.display().to_string());
                    }
                }
                let target = &mut self.targets[index];
                target.kind = TargetKind::Builtin { overridden: Some(span), fields };
                target.test = test;
                target.runner = runner;
                return;
            },
            Some((_, TargetKind::Builtin { .. })) if !overrides => Message::BuiltinTargetName(name),
            Some((index, _)) => {
                let previous = self.targets[index].declaration().unwrap();
                Message::RedeclareCustom { item: Item::Target, name, previous }
            },
        };
        self.diagnostics.push(Diagnostic { span: Some(span), message, severity: Severity::Error });
    }
    /// Returns whether the sysroot of the toolchain has the standard library for a builtin target, as its `rlib`, which
    /// rustup installs with `rustup target add`.
    ///
    /// The configuration depends on the directory of the target's libraries, so that it is generated again once the
    /// standard library is installed.
    pub(super) fn has_std(&mut self, target: &str) -> bool {
        let Some(&value) = self.values_idx.get("rustc/sysroot") else {
            return false;
        };
        let sysroot = std::path::PathBuf::from(self.values[value].data.coerce_string());
        let dir = sysroot.join("lib/rustlib").join(target).join("lib");
        self.depend_on_path(&dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("libstd-") && name.ends_with(".rlib")
        })
    }
    /// Returns whether the host can run what is built for a target, which it can for the host target, while for
    /// another, a probe for its architecture is run, with its runner if it has one, as described in [`probe`].
    ///
    /// The result is kept for the configuration, as a probe is a program which is run.
    pub(super) fn can_run(&mut self, name: &str) -> bool {
        if name == self.host() {
            return true;
        }
        if let Some(&can_run) = self.can_run.get(name) {
            return can_run;
        }
        let runner = match self.target(name).and_then(|target| target.runner) {
            Some(runner) => self.runners[runner].command.clone(),
            None => Vec::new(),
        };
        let can_run = probe::can_run(name, &runner, self.toolchain.as_deref());
        self.can_run.insert(name.into(), can_run);
        can_run
    }
    /// Returns whether crates for a target can be tested, which is given by its `test` attribute, or is whether the
    /// host can run what is built for a builtin target, while a custom target's specification may describe anything.
    pub(super) fn can_test(&mut self, name: &str) -> bool {
        match self.target(name).map(|target| (target.test, target.is_custom())) {
            Some((Some(test), _)) => test,
            Some((None, false)) => self.can_run(name),
            Some((None, true)) | None => false,
        }
    }
    /// Returns a target which is builtin or has been declared.
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.targets.iter().find(|target| target.name == name)
    }
    /// Returns the names of the builtin targets.
    pub(super) fn builtin_targets(&self) -> Vec<String> {
        let mut names = Vec::new();
        for target in &self.targets {
            if !target.is_custom() {
                names.push(target.name.clone());
            }
        }
        names
    }
    /// Report a target which is neither builtin nor declared, listing the builtin targets the first time.
    pub(super) fn check_target(&mut self, target: &str, span: Span) {
        if self.target(target).is_some() {
            return;
        }
        let builtin = self.builtin_targets();
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::UnknownTarget {
                target: target.into(),
                suggestion: suggest(target, &builtin),
            },
            severity: Severity::Error,
        });
        if !self.targets_listed {
            self.targets_listed = true;
            self.diagnostics.push(Diagnostic {
                span: None,
                message: Message::AvailableTargets(builtin),
                severity: Severity::Note,
            });
        }
    }
    /// Add the runner of a target, returning its index.
    fn add_runner(&mut self, command: Vec<String>, file: usize) -> usize {
        let runner = self.resolve_command(command, file);
        self.runners.push(runner);
        self.runners.len() - 1
    }
}

