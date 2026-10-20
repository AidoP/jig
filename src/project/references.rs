//! References to crates, and to the attributes of what has been declared.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::customs::CustomKind;
use super::diagnostic::{Diagnostic, Item, Message, Severity, suggest};
use super::install::Install;
use super::syntax::{Parser, Span};
use super::targets::{TargetKind, target_option};
use super::value::{Coercion, Data, Operand, Referent, Value};
use super::{Project, rust};

impl Project {
    /// Returns the value of a property, or of a crate by its name, or reports what is missing.
    ///
    /// A missing name with a `/` is a crate for a target, `<target>/<name>`.
    pub(super) fn reference(&mut self, name: &str, span: Span) -> Option<Operand> {
        if let Some(value) = self.referenced_value(name, span.file) {
            return Some(Operand { data: value.data.clone(), span, coerce: Coercion::Strict, origin: value.origin });
        }
        let message = match name.split_once('/') {
            _ if self.declare_target_codegen_option(name) || self.properties_idx.contains_key(name) => {
                Message::UnsetProperty(name.into())
            },
            // a codegen option of a target which is not known
            _ if let Some(target) = self.target_of_codegen_option(name) => {
                Message::UnknownTarget { suggestion: suggest(target, &self.builtin_targets()), target: target.into() }
            },
            // a property in a namespace of properties, such as `profile/<name>`, rather than of crates
            Some((prefix, _)) if self.target(prefix).is_none() && self.is_property_namespace(prefix) => {
                self.undeclared_property(name)
            },
            Some((target, _)) if self.target(target).is_none() => Message::InvalidTarget(target.into()),
            Some((target, crate_name)) => Message::UnknownCrate { name: crate_name.into(), target: target.into() },
            None => Message::UnsetProperty(name.into()),
        };
        self.diagnostics.push(Diagnostic { span: Some(span), message, severity: Severity::Error });
        None
    }
    /// Whether a property named `<prefix>/<name>` has been declared.
    fn is_property_namespace(&self, prefix: &str) -> bool {
        self.properties_idx.keys().any(|name| name.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
    }
    /// Returns the properties named `<prefix>/<name>` which are set where `span` is, as a dict of their values by
    /// `<name>`, which may itself contain `/`.
    ///
    /// The prefix must be that of a property which is declared or set, or a target, whose crates are its properties,
    /// so that a misspelt prefix is reported rather than empty.
    pub(super) fn namespace(&mut self, prefix: &str, span: Span) -> Option<Data> {
        let file = span.file;
        let prefix = format!("{prefix}/");
        let locals = self.files[file].locals.keys();
        let names: BTreeSet<_> = self.values_idx.keys().chain(locals).filter(|name| name.starts_with(&prefix)).cloned().collect();
        // the codegen options of a target, which are declared when they are first used
        let target_codegen = prefix.strip_prefix("codegen/").is_some_and(|target| {
            target.strip_suffix('/').is_some_and(|target| !target.contains('/') && self.target(target).is_some())
        });
        let known = !names.is_empty()
            || target_codegen
            || self.properties_idx.keys().any(|name| name.starts_with(&prefix))
            || self.target(&prefix[..prefix.len() - 1]).is_some();
        if !known {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::UnknownNamespace(prefix),
                severity: Severity::Error,
            });
            return None;
        }
        let mut entries = BTreeMap::new();
        for name in names {
            if let Some(value) = self.value(&name, file) {
                entries.insert(name[prefix.len()..].to_string(), value.data.clone());
            }
        }
        Some(Data::Dict(entries))
    }
    /// Returns the value of a property, or of the property of a crate for the `target` in effect, which is aliased by
    /// the crate's name.
    pub(super) fn referenced_value(&self, name: &str, file: usize) -> Option<&Value> {
        if let Some(value) = self.value(name, file) {
            return Some(value);
        }
        if name.contains('/') {
            return None;
        }
        let target = self.value("target", file)?.data.coerce_string();
        self.value(&format!("{target}/{name}"), file)
    }
    /// Take the name of an attribute after a `.`, which is at the parser.
    fn parse_attribute_access<'a>(&mut self, parser: &mut Parser<'a>) -> Option<(&'a str, Span)> {
        parser.expect('.');
        self.parse_ident(parser)
    }
    /// Returns the value of an attribute as an operand, or reports an attribute which is unknown or not set.
    fn attribute_value(
        &mut self,
        item: Item,
        name: &str,
        attribute: &str,
        attribute_span: Span,
        value: AttributeValue,
        start: usize,
        parser: &Parser,
    ) -> Option<Operand> {
        let message = match value {
            AttributeValue::Set(data) => {
                return Some(Operand { data, span: parser.span_from(start), coerce: Coercion::Strict, origin: None });
            },
            AttributeValue::Unset => Message::AttributeNotSet { item, name: name.into(), attribute: attribute.into() },
            AttributeValue::Unknown => {
                Message::UnknownAttributeOf { item, name: name.into(), attribute: attribute.into() }
            },
        };
        self.diagnostics.push(Diagnostic { span: Some(attribute_span), message, severity: Severity::Error });
        None
    }
    /// Take the attribute of what a reference refers to, a property, a declared target or a test, after its name.
    ///
    /// A crate or custom command is referred to by the value of a property, such as the one with its name, so it is
    /// looked for first, while a test has its own names, so it is looked for last.
    pub(super) fn parse_named_attribute(&mut self, parser: &mut Parser, name: &str, span: Span, start: usize) -> Option<Operand> {
        let (attribute, attribute_span) = self.parse_attribute_access(parser)?;
        let referent = match self.referenced_value(name, span.file) {
            Some(Value { data: Data::Ref(reference), .. }) => Some(reference.referent.clone()),
            _ => None,
        };
        if let Some(Referent::Crate { name: crate_name, target }) = &referent
            && let Some(c) = self.crates.get(&(crate_name.clone(), target.clone()))
        {
            let value = crate_attribute(c, attribute);
            // the test of a crate has its name, so an attribute the crate does not have may be the test's
            let tested = self.custom(name, true).is_some();
            if !matches!(value, AttributeValue::Unknown) || !tested {
                let crate_name = crate_name.clone();
                return self.attribute_value(Item::Crate, &crate_name, attribute, attribute_span, value, start, parser);
            }
        }
        let custom = match &referent {
            Some(Referent::Custom(name)) => self.custom(name, false),
            _ => None,
        };
        if let Some(custom) = custom && let CustomKind::Command { outputs, default, install } = &custom.kind {
            let value = match attribute {
                "input" => AttributeValue::Set(paths(&custom.inputs)),
                "output" => AttributeValue::Set(paths(outputs)),
                "depends" => AttributeValue::Set(paths(&custom.depends)),
                "command" => AttributeValue::Set(strings(&custom.command)),
                "default" => AttributeValue::Set(Data::Bool(*default)),
                attribute => install_attribute(install, attribute),
            };
            let name = custom.name.clone();
            return self.attribute_value(Item::CustomCommand, &name, attribute, attribute_span, value, start, parser);
        }
        self.declare_target_codegen_option(name);
        if let Some(&id) = self.properties_idx.get(name) && let Some(property) = &self.properties[id] {
            let value = match attribute {
                "default" => AttributeValue::optional(property.default.clone()),
                "type" => AttributeValue::Set(Data::Str(property.ty.name().into())),
                "read-only" => AttributeValue::Set(Data::Bool(property.read_only)),
                "info" => AttributeValue::optional(property.info.clone().map(Data::Str)),
                _ => AttributeValue::Unknown,
            };
            return self.attribute_value(Item::Property, name, attribute, attribute_span, value, start, parser);
        }
        // a builtin target, or one declared by a `target` statement
        if let Some(target) = self.target(name) {
            let custom = target.is_custom();
            // what is found from the toolchain, or by running a probe
            let value = match attribute {
                "has-std" => Some(!custom && self.has_std(name)),
                "can-run" => Some(self.can_run(name)),
                "test" => Some(self.can_test(name)),
                _ => None,
            };
            if let Some(value) = value {
                let value = AttributeValue::Set(Data::Bool(value));
                return self.attribute_value(Item::Target, name, attribute, attribute_span, value, start, parser);
            }
            let target = self.target(name).unwrap();
            let value = match (attribute, &target.kind) {
                ("inherit", TargetKind::Custom { inherit, .. }) => AttributeValue::optional(inherit.clone().map(Data::Str)),
                ("inherit", TargetKind::Builtin { .. }) => AttributeValue::Unset,
                ("override", kind) => {
                    AttributeValue::Set(Data::Bool(matches!(kind, TargetKind::Builtin { overridden: Some(_), .. })))
                },
                ("runner", _) => match target.runner {
                    Some(runner) => AttributeValue::Set(strings(&self.runners[runner].command)),
                    None => AttributeValue::Unset,
                },
                ("link-script", _) => AttributeValue::optional(target.link_script.as_deref().map(path)),
                // a field of the specification, which is named after its option
                (option, _) => match target_option(option) {
                    Some(option) => AttributeValue::optional(target.field(option.field).cloned()),
                    None => AttributeValue::Unknown,
                },
            };
            return self.attribute_value(Item::Target, name, attribute, attribute_span, value, start, parser);
        }
        // a test has its own name, which is looked for last
        if let Some(test) = self.custom(name, true) {
            let value = match attribute {
                "input" => AttributeValue::Set(paths(&test.inputs(&self.runners))),
                "depends" => AttributeValue::Set(paths(&test.depends)),
                "command" => AttributeValue::Set(strings(&test.command(&self.runners))),
                _ => AttributeValue::Unknown,
            };
            return self.attribute_value(Item::Test, name, attribute, attribute_span, value, start, parser);
        }
        // a crate for a target, which is reported as it would be without the attribute
        if name.contains('/') {
            self.reference(name, span);
            return None;
        }
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::UnknownReference(name.into()),
            severity: Severity::Error,
        });
        None
    }
}

/// Returns the value of an attribute of a crate.
fn crate_attribute(c: &rust::Crate, attribute: &str) -> AttributeValue {
    match attribute {
        "target" => AttributeValue::Set(Data::Str(c.target.clone())),
        "crate-type" => AttributeValue::Set(Data::Str(c.kind.crate_type().into())),
        "path" => AttributeValue::Set(Data::Str(c.path.display().to_string())),
        "output" => AttributeValue::Set(Data::Str(c.output().display().to_string())),
        "builtin" => AttributeValue::Set(Data::Bool(c.builtin)),
        "default" => AttributeValue::Set(Data::Bool(c.default)),
        "test" => AttributeValue::Set(Data::Bool(c.test)),
        "doc" => AttributeValue::Set(Data::Bool(c.doc)),
        "use" => {
            let mut names = Vec::new();
            for used in &c.uses {
                names.push(Data::Str(used.name.clone()));
            }
            AttributeValue::Set(Data::List(names))
        },
        "depends" => AttributeValue::Set(paths(&c.depends)),
        "cfg" => AttributeValue::Set(strings(&c.cfg)),
        "link-script" => AttributeValue::optional(c.link_script.as_deref().map(path)),
        "env" => {
            let entries = c.env.iter().map(|(name, value)| (name.clone(), Data::Str(value.clone()))).collect();
            AttributeValue::Set(Data::Dict(entries))
        },
        attribute => install_attribute(&c.install, attribute),
    }
}

/// Returns the value of an install attribute of a crate or custom command.
fn install_attribute(install: &Install, attribute: &str) -> AttributeValue {
    match attribute {
        "install" => AttributeValue::Set(Data::Bool(install.enabled)),
        "install-dir" => AttributeValue::Set(Data::Str(install.dir.clone())),
        "install-mode" => AttributeValue::Set(Data::Unsigned(install.mode.into())),
        _ => AttributeValue::Unknown,
    }
}

/// The value of an attribute of something which is referred to.
enum AttributeValue {
    Set(Data),
    /// The attribute is known, but has no value.
    Unset,
    /// There is no attribute with the name.
    Unknown,
}
impl AttributeValue {
    /// The value of an attribute which may not be set.
    fn optional(data: Option<Data>) -> Self {
        match data {
            Some(data) => Self::Set(data),
            None => Self::Unset,
        }
    }
}

/// Returns a path as a string.
fn path(path: &Path) -> Data {
    Data::Str(path.display().to_string())
}

/// Returns paths as a list of strings.
fn paths(paths: &[PathBuf]) -> Data {
    let mut items = Vec::new();
    for path in paths {
        items.push(Data::Str(path.display().to_string()));
    }
    Data::List(items)
}

/// Returns strings as a list.
fn strings(strings: &[String]) -> Data {
    let mut items = Vec::new();
    for string in strings {
        items.push(Data::Str(string.clone()));
    }
    Data::List(items)
}
