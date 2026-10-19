//! The values of properties and expressions, and their types.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::syntax::{Parser, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    None,
    Str(String),
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    List(Vec<Data>),
    /// Values by their names, which are kept in order, so that the order they were given in does not matter.
    Dict(BTreeMap<String, Data>),
    Ref(Ref),
}

/// A reference to what has paths which are already resolved, which it is converted to wherever a string is needed, so
/// that they are not resolved again relative to the file which uses them.
#[derive(Clone, Debug, PartialEq)]
pub struct Ref {
    pub(super) referent: Referent,
    /// The paths, relative to the project root, which are the outputs of a crate or custom command.
    pub(super) paths: Vec<PathBuf>,
}

/// What a [`Ref`] refers to.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Referent {
    Crate { name: String, target: String },
    /// A custom command, by its name.
    Custom(String),
    /// Only paths, such as the inputs and outputs of a custom command while it is read.
    Paths,
    /// A builtin file, such as `msrv`, by its name, which `use` processes.
    Builtin(String),
}
impl Data {
    /// Returns the name of the type as used by `declare`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Str(_) => "str",
            Self::Bool(_) => "bool",
            Self::Unsigned(_) => "unsigned",
            Self::Signed(_) => "signed",
            Self::List(_) => "list",
            Self::Dict(_) => "dict",
            Self::Ref(_) => "ref",
        }
    }
    pub fn is_integer(&self) -> bool {
        matches!(self, Self::Unsigned(_) | Self::Signed(_))
    }
    /// Convert to an integer: a `bool` is `0` or `1` and a `str` is parsed.
    pub fn coerce_integer(&self) -> Option<Self> {
        match self {
            Self::None => None,
            Self::Str(string) => {
                let mut parser = Parser::new(0, string);
                match parser.integer() {
                    Some((data, _)) if parser.is_empty() => Some(data),
                    _ => None,
                }
            },
            Self::Bool(value) => Some(Self::Unsigned(*value as u64)),
            Self::Unsigned(_) | Self::Signed(_) => Some(self.clone()),
            Self::List(_) | Self::Dict(_) | Self::Ref(_) => None,
        }
    }
    /// Convert to a boolean: an integer is true if it is not zero and a `str` must be `true` or `false`.
    pub fn coerce_bool(&self) -> Option<bool> {
        match self {
            Self::None => None,
            Self::Str(string) => match string.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            Self::Bool(value) => Some(*value),
            Self::Unsigned(i) => Some(*i != 0),
            Self::Signed(i) => Some(*i != 0),
            Self::List(_) | Self::Dict(_) | Self::Ref(_) => None,
        }
    }
    /// Convert to the type of `other`.
    pub fn coerce_like(&self, other: &Self) -> Option<Self> {
        match other {
            Self::None => None,
            Self::Str(_) => Some(Self::Str(self.coerce_string())),
            Self::Bool(_) => self.coerce_bool().map(Self::Bool),
            Self::Unsigned(_) | Self::Signed(_) => {
                let integer = self.coerce_integer()?;
                if integer.type_name() == other.type_name() {
                    Some(integer)
                } else {
                    integer.convert_sign()
                }
            },
            // a single value is not converted to a list, as it would be ambiguous with a list of values
            Self::List(_) | Self::Dict(_) | Self::Ref(_) => None,
        }
    }
    /// Convert between a signed and unsigned integer, if the value can be represented.
    pub fn convert_sign(&self) -> Option<Self> {
        match self {
            Self::Unsigned(i) => i64::try_from(*i).ok().map(Self::Signed),
            Self::Signed(i) => u64::try_from(*i).ok().map(Self::Unsigned),
            _ => None,
        }
    }
    /// Convert to a string, where the items of a list are separated by spaces, and the entries of a dict, as
    /// `name: value`, by `, `.
    pub fn coerce_string(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Str(s) => s.clone(),
            Self::Bool(b) => b.to_string(),
            Self::Unsigned(i) => i.to_string(),
            Self::Signed(i) => i.to_string(),
            Self::List(items) => {
                let items: Vec<_> = items.iter().map(Self::coerce_string).collect();
                items.join(" ")
            },
            Self::Dict(entries) => {
                let mut string = String::new();
                for (name, value) in entries {
                    if !string.is_empty() {
                        string.push_str(", ");
                    }
                    string.push_str(&format!("{name}: {}", value.coerce_string()));
                }
                string
            },
            Self::Ref(reference) => reference.strings().join(" "),
        }
    }
    /// Returns the strings of a value which is spread into several, such as arguments: each item of a list, or a single
    /// value.
    pub fn spread(&self) -> Vec<String> {
        match self {
            Self::List(items) => items.iter().flat_map(Self::spread).collect(),
            Self::Ref(reference) => reference.strings(),
            data => vec![data.coerce_string()],
        }
    }
}

impl Ref {
    /// The paths, as strings, or the name of a builtin file, which has no path.
    fn strings(&self) -> Vec<String> {
        if let Referent::Builtin(name) = &self.referent {
            return vec![name.clone()];
        }
        let mut strings = Vec::new();
        for path in &self.paths {
            strings.push(path.display().to_string());
        }
        strings
    }
}

pub enum Type {
    Any,
    String,
    Bool,
    Unsigned,
    Signed,
    List,
    Dict,
    Ref,
}
impl Type {
    /// The name of the type, as used by `declare`.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Type::Any => "any",
            Type::String => "str",
            Type::Bool => "bool",
            Type::Unsigned => "unsigned",
            Type::Signed => "signed",
            Type::List => "list",
            Type::Dict => "dict",
            Type::Ref => "ref",
        }
    }
    /// Returns the data if it has this type, or returns it as an error.
    ///
    /// An unsigned integer is converted for a signed type if it is in range, as a literal without a sign is unsigned.
    pub(super) fn accept(&self, data: Data) -> Result<Data, Data> {
        match (self, data) {
            (Type::Any, data)
            | (Type::String, data @ Data::Str(_))
            | (Type::Bool, data @ Data::Bool(_))
            | (Type::Unsigned, data @ Data::Unsigned(_))
            | (Type::Signed, data @ Data::Signed(_))
            | (Type::List, data @ Data::List(_))
            | (Type::Dict, data @ Data::Dict(_))
            | (Type::Ref, data @ Data::Ref(_)) => Ok(data),
            (Type::Signed, Data::Unsigned(i)) => i64::try_from(i).map(Data::Signed).map_err(|_| Data::Unsigned(i)),
            (_, data) => Err(data),
        }
    }
}

pub struct Property {
    pub(super) span: Option<Span>,
    pub(super) name: String,
    pub(super) ty: Type,
    /// Whether `set` is forbidden, leaving the property with its default or builtin value.
    pub(super) read_only: bool,
    /// A description of the property, included in diagnostics about it.
    pub(super) info: Option<String>,
    /// The value of a `default` attribute, which has the property's type.
    pub(super) default: Option<Data>,
}
impl Property {
    pub fn new(name: String, ty: Type) -> Self {
        Self {
            span: None,
            name,
            ty,
            read_only: false,
            info: None,
            default: None,
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn info(self, info: &str) -> Self {
        Self { info: Some(info.into()), ..self }
    }
    pub fn read_only(self) -> Self {
        Self { read_only: true, ..self }
    }
}

pub struct Value {
    pub(super) property: usize,
    pub(super) span: Option<Span>,
    pub(super) data: Data,
    pub(super) data_span: Option<Span>,
    /// Where the data was written, which is the origin of a referenced property rather than the reference itself.
    pub(super) origin: Option<Span>,
    pub(super) local: bool,
}
impl Value {
    pub fn new(property: usize, data: Data) -> Self {
        Self {
            property,
            span: None,
            data,
            data_span: None,
            origin: None,
            local: false,
        }
    }
}

/// An evaluated part of an expression.
pub(super) struct Operand {
    pub(super) data: Data,
    pub(super) span: Span,
    pub(super) coerce: Coercion,
    /// Where the value was set, if the operand is the value of a property.
    pub(super) origin: Option<Span>,
}

/// How far an operand may be converted to the type an operator requires.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Coercion {
    /// The operand must already have the required type.
    Strict,
    /// An integer literal, which may change between signed and unsigned.
    Sign,
    /// An expression in `{}`, which may be converted to any type.
    Any,
}
