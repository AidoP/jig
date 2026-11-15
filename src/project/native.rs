//! Static libraries of C and assembly, which are compiled by a C compiler, archived, and linked into the crates which
//! use them, and the properties which give their compilers, archivers and flags, `c/<name>` and `asm/<name>`, and
//! `c/<target>/<name>` for a target.

use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::rust;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Data, Operand, Property, Ref, Referent, Type};
use super::Project;

/// The language of a source, by its extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    C,
    /// Assembly, which is preprocessed, by `.S` and `.sx`, or is not, by `.s`.
    Assembly,
}

impl Language {
    /// Returns the language of a source by its extension, if it has one.
    pub fn of(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "c" => Some(Self::C),
            "s" | "S" | "sx" => Some(Self::Assembly),
            _ => None,
        }
    }
}

/// A source of a library, with its language.
#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub language: Language,
}

/// A static library, `lib<name>.a`, of objects compiled from C and assembly, for a target.
pub struct StaticLibrary {
    pub name: String,
    pub name_span: Span,
    pub target: String,
    /// The directory of the library's objects and archive, `<build-dir>/<target>/c/<name>`.
    pub dir: PathBuf,
    pub sources: Vec<Source>,
    /// The directories passed with `-I`.
    pub include: Vec<PathBuf>,
    /// The directories which what uses the library is compiled with too.
    pub public_include: Vec<PathBuf>,
    /// The macros passed with `-D`, as `<name>` or `<name>=<value>`.
    pub defines: Vec<String>,
    /// The arguments passed to the compiler for every source, after those of the properties.
    pub flags: Vec<String>,
    /// The other libraries for the same target which this one uses.
    pub uses: Vec<rust::Use>,
    /// Whether every object is linked, even one nothing refers to.
    pub whole_archive: bool,
    /// What is built before the library's sources are compiled, but is not an input.
    pub depends: Vec<PathBuf>,
    /// The environment variables the compiler and archiver inherit, whose values are inputs.
    pub pass_env: Vec<String>,
    /// Whether the library is built by `jig build` without targets.
    pub default: bool,
    /// The compiler, and its first arguments.
    pub compiler: Vec<String>,
    /// The archiver, and its first arguments.
    pub archiver: Vec<String>,
    /// The arguments for C sources, from `c/flags`, after those derived from the codegen options.
    pub c_flags: Vec<String>,
    /// The arguments for assembly sources, from `asm/flags`, after those derived from the codegen options.
    pub asm_flags: Vec<String>,
}

impl StaticLibrary {
    /// The archive, `lib<name>.a`, in the library's own directory, so that the linker finds it only where it is used.
    pub fn output(&self) -> PathBuf {
        self.dir.join(format!("lib{}.a", self.name))
    }
    /// The object compiled from a source, in the library's directory, named by its path, so that sources with the same
    /// name in different directories are different objects, and are different members of the archive, which are named
    /// by their file names.
    pub fn object(&self, source: &Path) -> PathBuf {
        let mut name = String::new();
        for component in source.components() {
            let part = match component {
                std::path::Component::Normal(part) => part.to_string_lossy(),
                std::path::Component::ParentDir => "up".into(),
                _ => continue,
            };
            if !name.is_empty() {
                name.push('-');
            }
            name.push_str(&part);
        }
        self.dir.join(format!("{name}.o"))
    }
}

/// The properties of the tools and flags for C and assembly, with the environment variables which give their defaults.
pub(super) struct Tool {
    pub(super) property: &'static str,
    pub(super) variable: &'static str,
    /// The program used for the host target where neither the property nor the variable is set, for a tool.
    pub(super) fallback: Option<&'static str>,
    pub(super) info: &'static str,
}

pub(super) const TOOLS: &[Tool] = &[
    Tool {
        property: "c/compiler",
        variable: "CC",
        fallback: Some("cc"),
        info: "the C compiler, as a program or a list of it and its first arguments, `CC` or `cc` by default",
    },
    Tool {
        property: "c/archiver",
        variable: "AR",
        fallback: Some("ar"),
        info: "the archiver of static libraries, as a program or a list of it and its first arguments, `AR` or `ar` by default",
    },
    Tool {
        property: "c/flags",
        variable: "CFLAGS",
        fallback: None,
        info: "the arguments of the C compiler for C sources, `CFLAGS` by default",
    },
    Tool {
        property: "asm/flags",
        variable: "ASFLAGS",
        fallback: None,
        info: "the arguments of the C compiler for assembly sources, `ASFLAGS` by default",
    },
];

/// Returns the target and the tool of the name of a property for a target, `c/<target>/<name>` or `asm/<target>/<name>`,
/// whether or not the target is known.
pub(super) fn target_tool(name: &str) -> Option<(&str, &'static Tool)> {
    let (namespace, rest) = name.split_once('/')?;
    let (target, tool) = rest.split_once('/')?;
    let property = TOOLS.iter().find(|candidate| candidate.property == format!("{namespace}/{tool}"))?;
    Some((target, property))
}

/// Returns the name of the property of a tool for a target.
fn target_property(target: &str, tool: &Tool) -> String {
    let (namespace, name) = tool.property.split_once('/').unwrap();
    format!("{namespace}/{target}/{name}")
}

/// Returns the name of an environment variable for a target, as the cc crate reads them, such as `CC_x86_64_unknown_linux_gnu`.
fn target_variable(variable: &str, target: &str) -> String {
    let target: String = target.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("{variable}_{target}")
}

/// The names of libraries which the standard library of a target links, which a project's library replaces where it is
/// used.
const SYSTEM_LIBRARIES: &[&str] = &["c", "m", "dl", "rt", "pthread", "util", "gcc", "gcc_s", "unwind"];

impl Project {
    /// Declare the properties of the tools and flags for C and assembly, which have no value until they are set, as their
    /// defaults are read from the environment only when a library needs them.
    pub fn declare_native_properties(&mut self) {
        for tool in TOOLS {
            let property = Property::new(tool.property.into(), Type::Any).info(tool.info);
            self.declare(tool.property.into(), Some(property));
        }
    }
    /// Declare the property of a tool for a target, `c/<target>/<name>`, if it is the name of one for a target which is
    /// builtin or has been declared, and it has not been declared. Returns whether it is the name of one.
    pub(super) fn declare_target_tool(&mut self, name: &str) -> bool {
        let Some((target, tool)) = target_tool(name) else {
            return false;
        };
        if self.target(target).is_none() {
            return false;
        }
        if self.properties_idx.get(name).is_some_and(|&id| self.properties[id].is_some()) {
            return true;
        }
        let info = format!("{} for `{target}`, which replaces `{}`", tool.info, tool.property);
        self.declare(name.into(), Some(Property::new(name.into(), Type::Any).info(&info)));
        true
    }
    /// Returns the arguments a tool or flags property gives for a target where `span` is: those of
    /// `c/<target>/<name>`, or `c/<name>`, if set, or else of the variable for the target, `<variable>_<target>`, or,
    /// for the host target, of the variable, or the fallback, where a variable's value is split at whitespace.
    fn tool_arguments(&mut self, tool: &Tool, target: &str, span: Span) -> Option<Vec<String>> {
        for name in [target_property(target, tool), tool.property.to_string()] {
            if let Some(value) = self.value(&name, span.file) {
                let operand = Operand { data: value.data.clone(), span, coerce: super::value::Coercion::Strict, origin: value.origin };
                return self.arguments(&operand);
            }
        }
        let split = |value: std::ffi::OsString| -> Vec<String> {
            value.to_string_lossy().split_whitespace().map(String::from).collect()
        };
        if let Some(value) = self.read_env(&target_variable(tool.variable, target)) {
            return Some(split(value));
        }
        if target != self.host() {
            return None;
        }
        if let Some(value) = self.read_env(tool.variable) {
            return Some(split(value));
        }
        tool.fallback.map(|program| vec![program.to_string()])
    }
    /// Returns the arguments of a value, a `str`, which is one, or a list of them, reporting another value.
    fn arguments(&mut self, operand: &Operand) -> Option<Vec<String>> {
        match &operand.data {
            Data::List(items) if items.iter().all(|item| !matches!(item, Data::List(_) | Data::Dict(_) | Data::None)) => {
                Some(items.iter().map(Data::coerce_string).collect())
            },
            Data::List(_) | Data::Dict(_) | Data::None => {
                self.invalid_operand(operand, "str, or list of str", false);
                None
            },
            data => Some(vec![data.coerce_string()]),
        }
    }
    /// Take a `static-library` statement, with `source`, `target`, `include`, `public-include`, `define`, `flags`,
    /// `use`, `whole-archive`, `depends`, `pass-env` and `default` attributes.
    pub(super) fn parse_static_library(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { text: name, span, literal }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        // the name is part of the file the linker finds, `lib<name>.a`, and of the property `<target>/<name>`
        if let Some((i, character)) = name.char_indices().find(|&(_, c)| matches!(c, '/' | '.')) {
            let span = if literal { Span { file: span.file, offset: span.offset + i, len: 1 } } else { span };
            parser.skip_statement();
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::InvalidCrateName { name, character },
                severity: Severity::Error,
            });
            return;
        }
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        let mut sources = Vec::new();
        let mut include = Vec::new();
        let mut public_include = Vec::new();
        let mut depends = Vec::new();
        let mut defines = Vec::new();
        let mut flags = Vec::new();
        let mut uses = Vec::new();
        let mut whole_archive = false;
        let mut default = false;
        let mut pass_env = Vec::new();
        let mut targets: Option<(Vec<String>, Span)> = None;
        while parser.indent().is_some() {
            let Some((attribute, attribute_span)) = self.parse_ident(parser) else {
                parser.skip_line();
                continue;
            };
            match attribute {
                "source" | "include" | "public-include" | "depends" => {
                    let Some(paths) = self.parse_custom_paths(parser, false) else {
                        parser.skip_line();
                        continue;
                    };
                    match attribute {
                        "source" => {
                            for path in paths {
                                match Language::of(&path) {
                                    Some(language) => sources.push(Source { path, language }),
                                    None => self.diagnostics.push(Diagnostic {
                                        span: Some(attribute_span),
                                        message: Message::UnknownSourceLanguage(path),
                                        severity: Severity::Error,
                                    }),
                                }
                            }
                        },
                        "include" => include.extend(paths),
                        "public-include" => public_include.extend(paths),
                        _ => depends.extend(paths),
                    }
                },
                // the names of libraries, as a crate's `use` takes the names of crates
                "use" => {
                    let names = if self.parse_space(parser) { self.parse_crate_names(parser) } else { None };
                    let Some((data, span)) = names else {
                        parser.skip_line();
                        continue;
                    };
                    let names = match &data {
                        Data::List(names) => names.iter().map(library_name).collect(),
                        data => vec![library_name(data)],
                    };
                    uses.extend(names.into_iter().map(|name| rust::Use { name, span }));
                },
                _ => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        continue;
                    };
                    self.static_library_attribute(attribute, attribute_span, operand, &mut StaticAttributes {
                        defines: &mut defines,
                        flags: &mut flags,
                        whole_archive: &mut whole_archive,
                        default: &mut default,
                        pass_env: &mut pass_env,
                        targets: &mut targets,
                    });
                },
            }
            if !self.parse_line_end(parser) {
                parser.skip_line();
            }
        }
        if sources.is_empty() {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::MissingAttribute("source"),
                severity: Severity::Error,
            });
            return;
        }
        if SYSTEM_LIBRARIES.contains(&name.as_str()) {
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::NameShadowsSystem(name.clone()),
                severity: Severity::Note,
            });
        }
        let (targets, target_span) = targets.unwrap_or_else(|| {
            let target = self.property("target", span).map(|target| target.data.coerce_string());
            (vec![target.unwrap_or_default()], span)
        });
        // `-O` and `-g` from the codegen options where the library is declared, so that a profile reaches C too
        let codegen = self.codegen_options(span, rust::Kind::Lib, &[], &targets);
        let build_dir = self.build_dir();
        for (i, target) in targets.iter().enumerate() {
            if targets[..i].contains(target) {
                self.diagnostics.push(Diagnostic {
                    span: Some(target_span),
                    message: Message::RepeatedTarget(target.clone()),
                    severity: Severity::Error,
                });
                continue;
            }
            let mut tools = Vec::new();
            for tool in TOOLS {
                tools.push(self.tool_arguments(tool, target, span));
            }
            let [compiler, archiver, c_flags, asm_flags] = <[_; 4]>::try_from(tools).ok().unwrap();
            let (Some(compiler), Some(archiver)) = (compiler.filter(|c| !c.is_empty()), archiver.filter(|a| !a.is_empty())) else {
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::NoCCompiler { target: target.clone() },
                    severity: Severity::Error,
                });
                continue;
            };
            let derived = derived_flags(&codegen[i]);
            let with_derived = |flags: Option<Vec<String>>| derived.iter().cloned().chain(flags.unwrap_or_default()).collect();
            let library = StaticLibrary {
                name: name.clone(),
                name_span: span,
                target: target.clone(),
                dir: build_dir.join(target).join("c").join(&name),
                sources: sources.clone(),
                include: include.clone(),
                public_include: public_include.clone(),
                defines: defines.clone(),
                flags: flags.clone(),
                uses: uses.clone(),
                whole_archive,
                depends: depends.clone(),
                pass_env: pass_env.clone(),
                default,
                compiler,
                archiver,
                c_flags: with_derived(c_flags),
                asm_flags: with_derived(asm_flags),
            };
            let key = (name.clone(), target.clone());
            if self.crates.contains_key(&key) {
                let previous = self.crates[&key].name_span;
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::RedeclareLibrary { name: name.clone(), target: target.clone(), previous },
                    severity: Severity::Error,
                });
                continue;
            }
            match self.static_libraries.entry(key) {
                Entry::Occupied(entry) => {
                    let previous = entry.get().name_span;
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::RedeclareLibrary { name: name.clone(), target: target.clone(), previous },
                        severity: Severity::Error,
                    });
                },
                Entry::Vacant(entry) => {
                    let output = entry.insert(library).output();
                    let referent = Referent::StaticLibrary { name: name.clone(), target: target.clone() };
                    let property = format!("{target}/{name}");
                    self.declare_reference(property, span, Item::StaticLibrary, Ref { referent, paths: vec![output] });
                },
            }
        }
    }
    /// Take an attribute of a `static-library` statement whose value is one expression.
    fn static_library_attribute(&mut self, attribute: &str, span: Span, operand: Operand, into: &mut StaticAttributes) {
        match attribute {
            "define" => {
                if let Some(defines) = self.defines(&operand) {
                    into.defines.extend(defines);
                }
            },
            "flags" => {
                if let Some(flags) = self.arguments(&operand) {
                    into.flags.extend(flags);
                }
            },
            "whole-archive" | "default" => {
                if let Some(value) = self.flag(&operand) {
                    *if attribute == "default" { &mut *into.default } else { &mut *into.whole_archive } = value;
                }
            },
            "pass-env" => {
                if let Some(names) = self.variable_names(&operand) {
                    into.pass_env.extend(names);
                }
            },
            "target" => match operand.data {
                Data::Str(name) => *into.targets = Some((vec![name], operand.span)),
                Data::List(items) if items.iter().all(|item| matches!(item, Data::Str(_))) => {
                    *into.targets = Some((items.iter().map(Data::coerce_string).collect(), operand.span));
                },
                _ => self.invalid_operand(&operand, "str, or list of str", false),
            },
            name => self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::UnknownAttribute { keyword: Item::StaticLibrary.keyword(), name: name.into() },
                severity: Severity::Error,
            }),
        }
    }
    /// Returns the macros of a `define` attribute, which is a name, a list of them, or a dict of names to values, as
    /// `<name>` or `<name>=<value>`.
    fn defines(&mut self, operand: &Operand) -> Option<Vec<String>> {
        let scalar = |data: &Data| matches!(data, Data::Str(_) | Data::Unsigned(_) | Data::Signed(_) | Data::Bool(_));
        match &operand.data {
            Data::Str(name) => Some(vec![name.clone()]),
            Data::List(names) if names.iter().all(|name| matches!(name, Data::Str(_))) => {
                Some(names.iter().map(Data::coerce_string).collect())
            },
            Data::Dict(entries) if entries.values().all(scalar) => {
                Some(entries.iter().map(|(name, value)| format!("{name}={}", value.coerce_string())).collect())
            },
            _ => {
                self.invalid_operand(operand, "str, list of str, or dict", false);
                None
            },
        }
    }
    /// Returns the static libraries, ordered by name and target.
    pub fn static_libraries(&self) -> Vec<&StaticLibrary> {
        let mut libraries: Vec<_> = self.static_libraries.values().collect();
        libraries.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
        libraries
    }
    /// Returns the static library for a target with a name, if one is declared.
    pub fn static_library(&self, name: &str, target: &str) -> Option<&StaticLibrary> {
        self.static_libraries.get(&(name.to_string(), target.to_string()))
    }
    /// Report what the static libraries and the crates use which is not a static library for their target, and
    /// libraries which use each other.
    pub(super) fn check_static_libraries(&mut self) {
        let mut keys: Vec<_> = self.static_libraries.keys().cloned().collect();
        keys.sort_unstable();
        let mut reported = Vec::new();
        for key in &keys {
            let library = &self.static_libraries[key];
            for used in &library.uses {
                if self.static_library(&used.name, &library.target).is_none() {
                    reported.push(Diagnostic {
                        span: Some(used.span),
                        message: Message::UnknownStaticLibrary { name: used.name.clone(), target: library.target.clone() },
                        severity: Severity::Error,
                    });
                }
            }
        }
        // a library which reaches itself through those it uses
        for key in &keys {
            let library = &self.static_libraries[key];
            let mut stack: Vec<&str> = library.uses.iter().map(|used| used.name.as_str()).collect();
            let mut seen = std::collections::HashSet::new();
            while let Some(name) = stack.pop() {
                if name == library.name {
                    reported.push(Diagnostic {
                        span: Some(library.name_span),
                        message: Message::LibraryCycle(library.name.clone()),
                        severity: Severity::Error,
                    });
                    break;
                }
                if seen.insert(name) && let Some(used) = self.static_library(name, &library.target) {
                    stack.extend(used.uses.iter().map(|used| used.name.as_str()));
                }
            }
        }
        self.diagnostics.extend(reported);
    }
    /// Returns the static libraries a crate or library uses directly, among its `uses`, by their names.
    pub fn used_static_libraries<'a>(&'a self, uses: &'a [rust::Use], target: &'a str) -> impl Iterator<Item = &'a StaticLibrary> {
        uses.iter().filter_map(move |used| self.static_library(&used.name, target))
    }
    /// Returns the static libraries used by these, through each other too, each once, in an order where each comes
    /// before those it uses, as a single-pass linker needs.
    pub fn transitive_static_libraries<'a>(&'a self, uses: &'a [rust::Use], target: &'a str) -> Vec<&'a StaticLibrary> {
        fn visit<'a>(
            project: &'a Project,
            library: &'a StaticLibrary,
            visited: &mut std::collections::HashSet<&'a str>,
            order: &mut Vec<&'a StaticLibrary>,
        ) {
            if !visited.insert(&library.name) {
                return;
            }
            for used in project.used_static_libraries(&library.uses, &library.target) {
                visit(project, used, visited, order);
            }
            order.push(library);
        }
        let mut order = Vec::new();
        let mut visited = std::collections::HashSet::new();
        for library in self.used_static_libraries(uses, target) {
            visit(self, library, &mut visited, &mut order);
        }
        // each after those it uses, reversed, so that each comes before them
        order.reverse();
        order
    }
}

/// The attributes of a `static-library` statement which are taken by [`Project::static_library_attribute`].
struct StaticAttributes<'a> {
    defines: &'a mut Vec<String>,
    flags: &'a mut Vec<String>,
    whole_archive: &'a mut bool,
    default: &'a mut bool,
    pass_env: &'a mut Vec<String>,
    targets: &'a mut Option<(Vec<String>, Span)>,
}

/// Returns the name of a library from a `use` attribute, which is its name, or a reference to it.
fn library_name(data: &Data) -> String {
    match data {
        Data::Ref(Ref { referent: Referent::StaticLibrary { name, .. } | Referent::Crate { name, .. }, .. }) => name.clone(),
        data => data.coerce_string(),
    }
}

/// Returns the flags of the C compiler derived from codegen options, `<option>=<value>`: `-O<level>` from `opt-level`
/// and `-g` from a `debuginfo` which is not none.
fn derived_flags(codegen: &[String]) -> Vec<String> {
    let mut flags = Vec::new();
    for option in codegen {
        if let Some(level) = option.strip_prefix("opt-level=") {
            flags.push(format!("-O{level}"));
        } else if let Some(level) = option.strip_prefix("debuginfo=")
            && !matches!(level, "0" | "none")
        {
            flags.push("-g".into());
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        assert_eq!(Language::of(Path::new("a/uart.c")), Some(Language::C));
        assert_eq!(Language::of(Path::new("boot.S")), Some(Language::Assembly));
        assert_eq!(Language::of(Path::new("boot.s")), Some(Language::Assembly));
        assert_eq!(Language::of(Path::new("boot.sx")), Some(Language::Assembly));
        assert_eq!(Language::of(Path::new("main.cpp")), None);
        assert_eq!(Language::of(Path::new("Makefile")), None);
    }

    #[test]
    fn derived() {
        let codegen = ["opt-level=s".to_string(), "debuginfo=2".into(), "panic=abort".into()];
        assert_eq!(derived_flags(&codegen), ["-Os", "-g"]);
        assert_eq!(derived_flags(&["debuginfo=0".into()]), Vec::<String>::new());
    }

    #[test]
    fn target_tools() {
        let (target, tool) = target_tool("c/aarch64-unknown-linux-musl/compiler").unwrap();
        assert_eq!((target, tool.property), ("aarch64-unknown-linux-musl", "c/compiler"));
        assert_eq!(target_tool("asm/x86_64-unknown-linux-gnu/flags").unwrap().1.property, "asm/flags");
        assert!(target_tool("c/compiler").is_none());
        assert!(target_tool("codegen/x/opt-level").is_none());
        assert_eq!(target_variable("CC", "thumbv8m.main-none-eabihf"), "CC_thumbv8m_main_none_eabihf");
    }
}
