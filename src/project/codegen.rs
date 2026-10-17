//! The stable codegen options of rustc, `-C <option>=<value>`, which are properties named `codegen/<option>`.

use super::syntax::Span;
use super::value::{Coercion, Data, Operand, Property, Type};
use super::Project;

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

impl Project {
    /// Declare the property of each codegen option, which has no value until it is set.
    pub fn declare_codegen_options(&mut self) {
        for option in CODEGEN_OPTIONS {
            let info = format!("the value of `-C {}` for the crates declared while it is set", option.name);
            let property = Property::new(property_name(option), Type::Any).info(&info);
            self.declare(property_name(option), Some(property));
        }
    }
    /// Returns the codegen options of a crate declared at `span`, as `<option>=<value>`, from its attributes, or the
    /// properties which are set, reporting those whose values cannot be given to rustc.
    pub(super) fn codegen_options(&mut self, span: Span, attributes: &[CodegenAttribute]) -> Vec<String> {
        let mut options = Vec::new();
        for option in CODEGEN_OPTIONS {
            // the last attribute for the option replaces the others
            let attribute = attributes.iter().rev().find(|attribute| attribute.option == option.name);
            let operand = if let Some(CodegenAttribute { operand, .. }) = attribute {
                Operand { data: operand.data.clone(), span: operand.span, coerce: operand.coerce, origin: operand.origin }
            } else if let Some(value) = self.value(&property_name(option), span.file) {
                Operand { data: value.data.clone(), span, coerce: Coercion::Strict, origin: value.origin }
            } else {
                continue;
            };
            let values = match (&operand.data, option.list) {
                (Data::List(items), ListKind::Joined(separator)) => vec![strings(items).join(separator)],
                (Data::List(items), ListKind::Repeated) => strings(items),
                (Data::List(_) | Data::Dict(_) | Data::None, _) => {
                    let expected = if matches!(option.list, ListKind::None) { "str" } else { "str or list" };
                    self.invalid_operand(&operand, expected, false);
                    continue;
                },
                (data, _) => vec![data.coerce_string()],
            };
            for value in values {
                options.push(format!("{}={value}", option.name));
            }
        }
        options
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
