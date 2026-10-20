//! The stable codegen options of rustc, `-C <option>=<value>`, which are properties named `codegen/<option>`, and
//! `codegen/<target>/<option>` for the crates for a target.

use super::diagnostic::{Diagnostic, Message, Severity};
use super::syntax::Span;
use super::value::{Coercion, Data, Operand, Property, Type};
use super::{Project, has_line_break};

/// A codegen option which can be given to a crate.
struct CodegenOption {
    name: &'static str,
    list: ListKind,
}

/// How a list is given as the value of a [`CodegenOption`], which is otherwise a single value.
#[derive(Clone, Copy)]
enum ListKind {
    /// A list is not accepted.
    None,
    /// The items are joined by a separator into one value.
    Joined(&'static str),
    /// Each item is given with the option, which is repeated.
    Repeated,
}

/// The stable codegen options which are not decided by the build itself, as `--emit` and `extra-filename` are.
const CODEGEN_OPTIONS: &[CodegenOption] = &[
    CodegenOption { name: "code-model", list: ListKind::None },
    CodegenOption { name: "codegen-units", list: ListKind::None },
    CodegenOption { name: "control-flow-guard", list: ListKind::None },
    CodegenOption { name: "debug-assertions", list: ListKind::None },
    CodegenOption { name: "debuginfo", list: ListKind::None },
    CodegenOption { name: "default-linker-libraries", list: ListKind::None },
    CodegenOption { name: "embed-bitcode", list: ListKind::None },
    CodegenOption { name: "force-frame-pointers", list: ListKind::None },
    CodegenOption { name: "force-unwind-tables", list: ListKind::None },
    CodegenOption { name: "instrument-coverage", list: ListKind::None },
    CodegenOption { name: "link-arg", list: ListKind::Repeated },
    CodegenOption { name: "link-dead-code", list: ListKind::None },
    CodegenOption { name: "link-self-contained", list: ListKind::None },
    CodegenOption { name: "linker", list: ListKind::None },
    CodegenOption { name: "linker-flavor", list: ListKind::None },
    CodegenOption { name: "linker-plugin-lto", list: ListKind::None },
    CodegenOption { name: "llvm-args", list: ListKind::Joined(" ") },
    CodegenOption { name: "lto", list: ListKind::None },
    CodegenOption { name: "no-prepopulate-passes", list: ListKind::None },
    CodegenOption { name: "no-redzone", list: ListKind::None },
    CodegenOption { name: "no-vectorize-loops", list: ListKind::None },
    CodegenOption { name: "no-vectorize-slp", list: ListKind::None },
    CodegenOption { name: "opt-level", list: ListKind::None },
    CodegenOption { name: "overflow-checks", list: ListKind::None },
    CodegenOption { name: "panic", list: ListKind::None },
    CodegenOption { name: "passes", list: ListKind::Joined(" ") },
    CodegenOption { name: "prefer-dynamic", list: ListKind::None },
    CodegenOption { name: "profile-generate", list: ListKind::None },
    CodegenOption { name: "profile-use", list: ListKind::None },
    CodegenOption { name: "relocation-model", list: ListKind::None },
    CodegenOption { name: "relro-level", list: ListKind::None },
    CodegenOption { name: "remark", list: ListKind::Joined(",") },
    CodegenOption { name: "rpath", list: ListKind::None },
    CodegenOption { name: "save-temps", list: ListKind::None },
    CodegenOption { name: "split-debuginfo", list: ListKind::None },
    CodegenOption { name: "strip", list: ListKind::None },
    CodegenOption { name: "symbol-mangling-version", list: ListKind::None },
    CodegenOption { name: "target-cpu", list: ListKind::None },
    CodegenOption { name: "target-feature", list: ListKind::Joined(",") },
];

/// The value of a codegen option from an attribute of a crate, `codegen/<option>`, which replaces its property.
pub(super) struct CodegenAttribute {
    pub(super) option: String,
    pub(super) operand: Operand,
}

/// Returns whether there is a codegen option with the name.
pub(super) fn is_codegen_option(name: &str) -> bool {
    CODEGEN_OPTIONS.iter().any(|option| option.name == name)
}

/// Returns the name of the property of a codegen option.
fn property_name(option: &CodegenOption) -> String {
    format!("codegen/{}", option.name)
}

/// Returns the name of the property of a codegen option for a target.
fn target_property_name(target: &str, option: &CodegenOption) -> String {
    format!("codegen/{target}/{}", option.name)
}

/// Returns the target and the option of the name of a property of a codegen option for a target,
/// `codegen/<target>/<option>`, where a target cannot have a `/`.
fn target_codegen_option(name: &str) -> Option<(&str, &'static CodegenOption)> {
    let (target, option) = name.strip_prefix("codegen/")?.split_once('/')?;
    let option = CODEGEN_OPTIONS.iter().find(|candidate| candidate.name == option)?;
    Some((target, option))
}

/// Returns the target of the name of a property of a codegen option for a target, `codegen/<target>/<option>`, if it
/// names an option, whether or not the target is known.
pub(super) fn target_of_codegen_option(name: &str) -> Option<&str> {
    target_codegen_option(name).map(|(target, _)| target)
}

impl Project {
    /// Declare the property of each codegen option, which has no value until it is set.
    ///
    /// Those for each target, `codegen/<target>/<option>`, are declared when they are first used, by
    /// [`Self::declare_target_codegen_option`], as there are many targets.
    pub fn declare_codegen_options(&mut self) {
        for option in CODEGEN_OPTIONS {
            let info = format!("the value of `-C {}` for the crates declared while it is set", option.name);
            let property = Property::new(property_name(option), Type::Any).info(&info);
            self.declare(property_name(option), Some(property));
        }
    }
    /// Declare the property of a codegen option for a target, `codegen/<target>/<option>`, which replaces
    /// `codegen/<option>` for the crates for the target, if it is the name of one for a target which is builtin or has
    /// been declared, and it has not been declared. Returns whether it is the name of one.
    pub(super) fn declare_target_codegen_option(&mut self, name: &str) -> bool {
        let Some((target, option)) = target_codegen_option(name) else {
            return false;
        };
        if self.target(target).is_none() {
            return false;
        }
        if self.properties_idx.get(name).is_some_and(|&id| self.properties[id].is_some()) {
            return true;
        }
        let info = format!(
            "the value of `-C {}` for the crates for `{target}` declared while it is set, which replaces `{}`",
            option.name,
            property_name(option),
        );
        let property = Property::new(name.into(), Type::Any).info(&info);
        self.declare(name.into(), Some(property));
        true
    }
    /// Returns the codegen options of a crate declared at `span` for each of its targets, as `<option>=<value>`, from
    /// its attributes, or the properties which are set, those for the target replacing the others, reporting those
    /// whose values cannot be given to rustc.
    pub(super) fn codegen_options(
        &mut self,
        span: Span,
        attributes: &[CodegenAttribute],
        targets: &[String],
    ) -> Vec<Vec<String>> {
        let mut options = vec![Vec::new(); targets.len()];
        for option in CODEGEN_OPTIONS {
            // the last attribute for the option replaces the others, and the properties for the targets
            let attribute = attributes.iter().rev().find(|attribute| attribute.option == option.name);
            let operand = if let Some(CodegenAttribute { operand, .. }) = attribute {
                let Operand { data, span, coerce, origin } = operand;
                Some(Operand { data: data.clone(), span: *span, coerce: *coerce, origin: *origin })
            } else {
                self.property_operand(&property_name(option), span)
            };
            let values = operand.and_then(|operand| self.codegen_values(option, &operand));
            for (target, options) in targets.iter().zip(&mut options) {
                let values = match attribute {
                    Some(_) => values.clone(),
                    None => match self.property_operand(&target_property_name(target, option), span) {
                        Some(operand) => self.codegen_values(option, &operand),
                        None => values.clone(),
                    },
                };
                for value in values.into_iter().flatten() {
                    options.push(format!("{}={value}", option.name));
                }
            }
        }
        options
    }
    /// Returns the value of a property where `span` is as an operand, if it is set.
    fn property_operand(&self, name: &str, span: Span) -> Option<Operand> {
        let value = self.value(name, span.file)?;
        Some(Operand { data: value.data.clone(), span, coerce: Coercion::Strict, origin: value.origin })
    }
    /// Returns the values of a codegen option, which is given once for each, reporting a value which cannot be given
    /// to rustc.
    fn codegen_values(&mut self, option: &CodegenOption, operand: &Operand) -> Option<Vec<String>> {
        let values = match (&operand.data, option.list) {
            (Data::List(items), ListKind::Joined(separator)) => Some(vec![strings(items).join(separator)]),
            (Data::List(items), ListKind::Repeated) => Some(strings(items)),
            (Data::List(_) | Data::Dict(_) | Data::None, _) => {
                let expected = if matches!(option.list, ListKind::None) { "str" } else { "str or list" };
                self.invalid_operand(operand, expected, false);
                None
            },
            (data, _) => Some(vec![data.coerce_string()]),
        };
        // the options are arguments of rustc
        if values.as_ref().is_some_and(|values| values.iter().any(|value| has_line_break(value))) {
            self.diagnostics.push(Diagnostic {
                span: Some(operand.span),
                message: Message::NewlineInArgument,
                severity: Severity::Error,
            });
            return None;
        }
        values
    }
}

/// Returns the items of a list as strings.
fn strings(items: &[Data]) -> Vec<String> {
    let mut strings = Vec::new();
    for item in items {
        strings.push(item.coerce_string());
    }
    strings
}
