//! The stable codegen options of rustc, `-C <option>=<value>`, which are properties named `codegen/<option>`, and
//! `codegen/<target>/<option>` for the crates for a target.

use super::syntax::Span;
use super::value::{Coercion, Data, Operand, Property, Type};
use super::{Project, rust};

/// A codegen option which can be given to a crate.
struct CodegenOption {
    name: &'static str,
    /// The name of the option's property, `codegen/<name>`.
    property: &'static str,
    list: ListKind,
}

/// A [`CodegenOption`] with a name and how it takes a list.
macro_rules! option {
    ($name:literal, $list:expr) => {
        CodegenOption { name: $name, property: concat!("codegen/", $name), list: $list }
    };
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
    option!("code-model", ListKind::None),
    option!("codegen-units", ListKind::None),
    option!("control-flow-guard", ListKind::None),
    option!("debug-assertions", ListKind::None),
    option!("debuginfo", ListKind::None),
    option!("default-linker-libraries", ListKind::None),
    option!("embed-bitcode", ListKind::None),
    option!("force-frame-pointers", ListKind::None),
    option!("force-unwind-tables", ListKind::None),
    option!("instrument-coverage", ListKind::None),
    option!("link-arg", ListKind::Repeated),
    option!("link-dead-code", ListKind::None),
    option!("link-self-contained", ListKind::None),
    option!("linker", ListKind::None),
    option!("linker-flavor", ListKind::None),
    option!("linker-plugin-lto", ListKind::None),
    option!("llvm-args", ListKind::Joined(" ")),
    option!("lto", ListKind::None),
    option!("no-prepopulate-passes", ListKind::None),
    option!("no-redzone", ListKind::None),
    option!("no-vectorize-loops", ListKind::None),
    option!("no-vectorize-slp", ListKind::None),
    option!("opt-level", ListKind::None),
    option!("overflow-checks", ListKind::None),
    option!("panic", ListKind::None),
    option!("passes", ListKind::Joined(" ")),
    option!("prefer-dynamic", ListKind::None),
    option!("profile-generate", ListKind::None),
    option!("profile-use", ListKind::None),
    option!("relocation-model", ListKind::None),
    option!("relro-level", ListKind::None),
    option!("remark", ListKind::Joined(",")),
    option!("rpath", ListKind::None),
    option!("save-temps", ListKind::None),
    option!("split-debuginfo", ListKind::None),
    option!("strip", ListKind::None),
    option!("symbol-mangling-version", ListKind::None),
    option!("target-cpu", ListKind::None),
    option!("target-feature", ListKind::Joined(",")),
];

/// The codegen options which describe the machine crates are built for, or how they are linked for it.
/// `codegen/<option>` does not give these to a procedural macro, which is built for the host, where rustc loads it.
/// Setting one for a whole project, such as for a target without an operating system, therefore does not stop its
/// procedural macros linking or loading.
const TARGET_OPTIONS: &[&str] = &[
    "code-model",
    "default-linker-libraries",
    "link-arg",
    "link-self-contained",
    "linker",
    "linker-flavor",
    "no-redzone",
    "relocation-model",
    "target-cpu",
    "target-feature",
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
            let property = Property::new(option.property.into(), Type::Any).info(&info);
            self.declare(option.property.into(), Some(property));
        }
    }
    /// Declare the property of a codegen option for a target, `codegen/<target>/<option>`, which replaces
    /// `codegen/<option>` for the crates for that target. It is declared if `name` is the name of one, its target is
    /// builtin or has been declared, and it is not declared yet. Returns whether `name` is the name of one.
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
            option.name, option.property,
        );
        let property = Property::new(name.into(), Type::Any).info(&info);
        self.declare(name.into(), Some(property));
        true
    }
    /// Returns the codegen options, as `<option>=<value>`, of a crate declared at `span`, for each of its targets.
    /// Each option comes from the crate's attribute, or failing that from the property for the target, or failing
    /// that from the property. Values which cannot be given to rustc are reported.
    ///
    /// The properties can be set for a whole project, as with cargo, so some are not applied to a procedural macro:
    /// `lto` and `panic` from any property, as rustc refuses LTO for one and it must unwind, and the options of
    /// [`TARGET_OPTIONS`] from `codegen/<option>`, as it is built for the host. `codegen/<host>/<option>` and an
    /// attribute of the crate still apply to it.
    pub(super) fn codegen_options(
        &mut self,
        span: Span,
        kind: rust::Kind,
        attributes: &[CodegenAttribute],
        targets: &[String],
    ) -> Vec<Vec<String>> {
        let mut options = vec![Vec::new(); targets.len()];
        // the name of each target's property of an option, `codegen/<target>/<option>`, written over the last
        let mut target_property = String::new();
        for option in CODEGEN_OPTIONS {
            // the last attribute for the option replaces the others, and the properties for the targets
            let attribute = attributes.iter().rev().find(|attribute| attribute.option == option.name);
            let proc_macro = kind == rust::Kind::ProcMacro && attribute.is_none();
            if proc_macro && matches!(option.name, "lto" | "panic") {
                continue;
            }
            let operand = if let Some(CodegenAttribute { operand, .. }) = attribute {
                let Operand { data, span, coerce, origin } = operand;
                Some(Operand { data: data.clone(), span: *span, coerce: *coerce, origin: *origin })
            } else if proc_macro && TARGET_OPTIONS.contains(&option.name) {
                // only for the host, by `codegen/<host>/<option>`
                None
            } else {
                self.property_operand(option.property, span)
            };
            let values = operand.and_then(|operand| self.codegen_values(option, &operand));
            for (target, options) in targets.iter().zip(&mut options) {
                // an attribute replaces the properties, and the target's property replaces the general one
                let target_operand = if attribute.is_some() {
                    None
                } else {
                    target_property.clear();
                    target_property.extend(["codegen/", target, "/", option.name]);
                    self.property_operand(&target_property, span)
                };
                let values = match target_operand {
                    Some(operand) => self.codegen_values(option, &operand),
                    None => values.clone(),
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
    /// Returns the values of a codegen option, which is given once for each, reporting a value which is not one.
    fn codegen_values(&mut self, option: &CodegenOption, operand: &Operand) -> Option<Vec<String>> {
        match (&operand.data, option.list) {
            (Data::List(items), ListKind::Joined(separator)) => Some(vec![strings(items).join(separator)]),
            (Data::List(items), ListKind::Repeated) => Some(strings(items)),
            (Data::List(_) | Data::Dict(_) | Data::None, _) => {
                let expected = if matches!(option.list, ListKind::None) { "str" } else { "str or list" };
                self.invalid_operand(operand, expected, false);
                None
            },
            (data, _) => Some(vec![data.coerce_string()]),
        }
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
