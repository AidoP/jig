//! Static libraries of C and assembly, which are compiled by a C compiler, archived, and linked into the crates which
//! use them, and the properties which give their compilers, archivers and flags, `c/<name>` and `asm/<name>`, and
//! `c/<target>/<name>` for a target.

use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
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
    /// C++, by `.cc`, `.cpp` and `.cxx`.
    Cxx,
}

impl Language {
    /// Returns the language of a source by its extension, if it has one.
    pub fn of(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "c" => Some(Self::C),
            "s" | "S" | "sx" => Some(Self::Assembly),
            "cc" | "cpp" | "cxx" => Some(Self::Cxx),
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

/// What is built from C and assembly, the objects of whose sources are archived or linked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeKind {
    /// A static library, `lib<name>.a`, which crates, shared libraries and executables link.
    StaticLibrary,
    /// A shared library, linked by the C compiler, which crates, shared libraries and executables link, and load as
    /// they run.
    SharedLibrary,
    /// A program, linked by the C compiler.
    Executable,
}

impl NativeKind {
    pub(super) fn item(self) -> Item {
        match self {
            Self::StaticLibrary => Item::StaticLibrary,
            Self::SharedLibrary => Item::SharedLibrary,
            Self::Executable => Item::Executable,
        }
    }
    /// Whether it is linked by the compiler, as a shared library or an executable is, rather than archived.
    pub fn is_linked(self) -> bool {
        self != Self::StaticLibrary
    }
}

/// A static library or an executable, of objects compiled from C and assembly, for a target.
pub struct Native {
    pub kind: NativeKind,
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
    /// What identifies the compiler in rig's cache, if rig caches and it is found, as
    /// [`Project::identify_static_libraries`] finds it once the configuration is evaluated.
    pub cache: Option<String>,
    /// The C++ compiler, and its first arguments, if it has C++ sources.
    pub cxx: Vec<String>,
    /// What identifies the C++ compiler in rig's cache, as `cache` does the C compiler.
    pub cxx_cache: Option<String>,
    /// The arguments for C++ sources, from `cxx/flags`, after those derived from the codegen options.
    pub cxx_flags: Vec<String>,
    /// The library of the C++ runtime, from `cxx/runtime`, which what links a static library with C++ sources links.
    pub cxx_runtime: String,
    /// The archiver, and its first arguments.
    pub archiver: Vec<String>,
    /// The arguments for C sources, from `c/flags`, after those derived from the codegen options.
    pub c_flags: Vec<String>,
    /// The arguments for assembly sources, from `asm/flags`, after those derived from the codegen options.
    pub asm_flags: Vec<String>,
    /// The arguments of the compiler as it links an executable, from `c/link-flags`, followed by its own.
    pub link_flags: Vec<String>,
    /// The linker script of an executable or shared library, which replaces that of its target.
    pub link_script: Option<PathBuf>,
    /// The directories an executable or shared library looks for the shared libraries it uses in as it runs, as
    /// `-Wl,-rpath,<dir>`.
    pub rpath: Vec<String>,
    /// Where `jig install` installs an executable, if it does.
    pub install: Install,
}

impl Native {
    /// Whether any of its sources is C++, so that what links it as a static library links the C++ runtime.
    pub fn has_cxx(&self) -> bool {
        self.sources.iter().any(|source| source.language == Language::Cxx)
    }
    /// The archive of a static library, `lib<name>.a`, in its own directory, so that the linker finds it only where it
    /// is used, or the program of an executable, `<build-dir>/<target>/<name>`, as a binary crate's is.
    pub fn output(&self) -> PathBuf {
        match self.kind {
            NativeKind::StaticLibrary => self.dir.join(format!("lib{}.a", self.name)),
            // as the platform's linker finds it by its name
            NativeKind::SharedLibrary if self.target.contains("-windows") => self.dir.join(format!("{}.dll", self.name)),
            NativeKind::SharedLibrary if self.target.contains("-apple-") => self.dir.join(format!("lib{}.dylib", self.name)),
            NativeKind::SharedLibrary => self.dir.join(format!("lib{}.so", self.name)),
            NativeKind::Executable => {
                let suffix = if self.target.contains("-windows") { ".exe" } else { "" };
                self.dir.parent().unwrap().parent().unwrap().join(format!("{}{suffix}", self.name))
            },
        }
    }
    /// Whether its objects are compiled with `-fPIC`: those of a shared library, and of a static library for a target
    /// with an operating system, which a shared library may link, as Rust's are for such a target, but not for Windows,
    /// whose code is independent of where it is loaded without it.
    pub fn position_independent(&self) -> bool {
        let hosted = has_shared_libraries(&self.target) && !self.target.contains("-windows");
        match self.kind {
            NativeKind::SharedLibrary | NativeKind::StaticLibrary => hosted,
            NativeKind::Executable => false,
        }
    }
    /// What links a library: its output, but for a shared library for Windows, which is linked by its import library,
    /// `lib<name>.dll.a`, which its link writes beside it.
    pub fn link_output(&self) -> PathBuf {
        match self.kind {
            NativeKind::SharedLibrary if self.target.contains("-windows") => self.dir.join(format!("lib{}.dll.a", self.name)),
            _ => self.output(),
        }
    }
    /// The name of an executable for `jig run`, with its target if it is not the host target, as a crate's test is.
    pub fn short_name(&self, host: &str) -> String {
        if self.target == host { self.name.clone() } else { format!("{}@{}", self.name, self.target) }
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
    Tool {
        property: "c/link-flags",
        variable: "LDFLAGS",
        fallback: None,
        info: "the arguments of the C compiler as it links an executable, `LDFLAGS` by default",
    },
    Tool {
        property: "cxx/compiler",
        variable: "CXX",
        fallback: Some("c++"),
        info: "the C++ compiler, as a program or a list of it and its first arguments, `CXX` or `c++` by default",
    },
    Tool {
        property: "cxx/flags",
        variable: "CXXFLAGS",
        fallback: None,
        info: "the arguments of the C++ compiler for C++ sources, `CXXFLAGS` by default",
    },
    Tool {
        property: "cxx/runtime",
        variable: "",
        fallback: None,
        info: "the library of the C++ runtime, which what links a static library of C++ links, `c++` for Apple's and \
               the BSDs' targets and `stdc++` for others by default",
    },
];

/// The number of [`TOOLS`] which every library or executable needs, which are before those of C++, which only one with
/// C++ sources needs, so that `CXX` is not read for one without.
const C_TOOLS: usize = 5;

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
        // one which is not read from the environment, as `cxx/runtime`
        if tool.variable.is_empty() {
            return None;
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
    /// Take a `static-library` or `executable` statement, with `source`, `target`, `include`, `public-include`,
    /// `define`, `flags`, `use`, `depends`, `pass-env` and `default` attributes, and `whole-archive` for a static
    /// library, or `link-flags`, `link-script` and the install attributes for an executable.
    pub(super) fn parse_native(&mut self, parser: &mut Parser, kind: NativeKind) {
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
        let mut link_flags = Vec::new();
        let mut link_script = None;
        let mut rpath = Vec::new();
        // a shared library is executable, as a procedural macro is
        let mut install = if kind == NativeKind::Executable { Install::executable() } else { Install::library(0o755) };
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
                "link-flags" | "link-script" | "rpath" | "install" | "install-dir" | "install-mode" if kind.is_linked() => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        continue;
                    };
                    match attribute {
                        "link-flags" => link_flags.extend(self.arguments(&operand).unwrap_or_default()),
                        "rpath" => rpath.extend(self.arguments(&operand).unwrap_or_default()),
                        "link-script" => {
                            let base = self.files[parser.file].path.parent().unwrap().to_path_buf();
                            link_script = self.operand_path(operand, &base).or(link_script);
                        },
                        attribute => self.install_attribute(&mut install, attribute, operand),
                    }
                },
                "whole-archive" if kind.is_linked() => {
                    parser.skip_line();
                    self.diagnostics.push(Diagnostic {
                        span: Some(attribute_span),
                        message: Message::UnknownAttribute { keyword: kind.item().keyword(), name: attribute.into() },
                        severity: Severity::Error,
                    });
                    continue;
                },
                _ => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        continue;
                    };
                    self.native_attribute(kind, attribute, attribute_span, operand, &mut NativeAttributes {
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
        if kind != NativeKind::Executable && SYSTEM_LIBRARIES.contains(&name.as_str()) {
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
            for tool in &TOOLS[..C_TOOLS] {
                tools.push(self.tool_arguments(tool, target, span));
            }
            let [compiler, archiver, c_flags, asm_flags, c_link_flags] = <[_; C_TOOLS]>::try_from(tools).ok().unwrap();
            // C++'s, only for C++
            let has_cxx = sources.iter().any(|source| source.language == Language::Cxx);
            let (mut cxx, mut cxx_flags, mut cxx_runtime) = (Vec::new(), Vec::new(), String::new());
            if has_cxx {
                let [compiler, flags, runtime] = [&TOOLS[C_TOOLS], &TOOLS[C_TOOLS + 1], &TOOLS[C_TOOLS + 2]];
                let Some(compiler) = self.tool_arguments(compiler, target, span).filter(|cxx| !cxx.is_empty()) else {
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::NoCxxCompiler { target: target.clone() },
                        severity: Severity::Error,
                    });
                    continue;
                };
                cxx = compiler;
                cxx_flags = self.tool_arguments(flags, target, span).unwrap_or_default();
                let runtime = self.tool_arguments(runtime, target, span).and_then(|runtime| runtime.into_iter().next());
                cxx_runtime = runtime.unwrap_or_else(|| default_cxx_runtime(target).into());
            }
            // what is linked by the compiler needs no archiver
            let archiver = if kind.is_linked() { Some(archiver.unwrap_or_default()) } else { archiver };
            let archiver = archiver.filter(|a| !a.is_empty() || kind.is_linked());
            let (Some(compiler), Some(archiver)) = (compiler.filter(|c| !c.is_empty()), archiver) else {
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::NoCCompiler { target: target.clone() },
                    severity: Severity::Error,
                });
                continue;
            };
            let derived = derived_flags(&codegen[i]);
            let with_derived = |flags: Option<Vec<String>>| derived.iter().cloned().chain(flags.unwrap_or_default()).collect();
            let library = Native {
                kind,
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
                cache: None,
                archiver,
                c_flags: with_derived(c_flags),
                cxx,
                cxx_cache: None,
                cxx_flags: with_derived(Some(cxx_flags)),
                cxx_runtime,
                asm_flags: with_derived(asm_flags),
                link_flags: c_link_flags.unwrap_or_default().into_iter().chain(link_flags.iter().cloned()).collect(),
                link_script: link_script.clone(),
                rpath: rpath.clone(),
                install: install.clone(),
            };
            let key = (name.clone(), target.clone());
            if self.crates.contains_key(&key) {
                let previous = self.crates[&key].name_span;
                self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::RedeclareLibrary { item: kind.item(), name: name.clone(), target: target.clone(), previous },
                    severity: Severity::Error,
                });
                continue;
            }
            match self.natives.entry(key) {
                Entry::Occupied(entry) => {
                    let previous = entry.get().name_span;
                    self.diagnostics.push(Diagnostic {
                        span: Some(span),
                        message: Message::RedeclareLibrary { item: kind.item(), name: name.clone(), target: target.clone(), previous },
                        severity: Severity::Error,
                    });
                },
                Entry::Vacant(entry) => {
                    let output = entry.insert(library).output();
                    let referent = Referent::Native { name: name.clone(), target: target.clone() };
                    let property = format!("{target}/{name}");
                    self.declare_reference(property, span, kind.item(), Ref { referent, paths: vec![output] });
                },
            }
        }
    }
    /// Take an attribute of a `static-library` or `executable` statement whose value is one expression.
    fn native_attribute(&mut self, kind: NativeKind, attribute: &str, span: Span, operand: Operand, into: &mut NativeAttributes) {
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
                message: Message::UnknownAttribute { keyword: kind.item().keyword(), name: name.into() },
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
    /// Find what identifies the compiler of each static library in rig's cache, if rig caches, so that what it compiled
    /// is fetched only for the same compiler: the contents of its program, and what the command prints of its version,
    /// as with `ccache gcc` it is gcc's. The configuration depends on the program, so that it is configured again when
    /// the compiler is replaced. A compiler which is not found is not cached, and its compiles fail as they run.
    pub fn identify_static_libraries(&mut self) {
        if self.cache.is_none() {
            return;
        }
        let mut identities: std::collections::HashMap<Vec<String>, Option<String>> = std::collections::HashMap::new();
        let mut keys: Vec<_> = self.natives.keys().cloned().collect();
        keys.sort_unstable();
        for key in keys {
            let mut identify = |project: &mut Self, compiler: Vec<String>| {
                if compiler.is_empty() {
                    return None;
                }
                if let Some(identity) = identities.get(&compiler) {
                    return identity.clone();
                }
                let identity = project.compiler_identity(&compiler);
                identities.insert(compiler, identity.clone());
                identity
            };
            let (compiler, cxx) = (self.natives[&key].compiler.clone(), self.natives[&key].cxx.clone());
            let c_compiler = identify(self, compiler);
            let cxx_compiler = identify(self, cxx);
            let native = self.natives.get_mut(&key).unwrap();
            native.cache = c_compiler;
            native.cxx_cache = cxx_compiler;
        }
    }
    /// Returns what identifies a compiler, a program and its first arguments, if its program is found.
    fn compiler_identity(&mut self, compiler: &[String]) -> Option<String> {
        let program = if compiler[0].contains('/') || (cfg!(windows) && compiler[0].contains('\\')) {
            // relative to the project root, as rig runs it from the build directory rebased
            Some(self.root.join(&compiler[0]))
        } else {
            self.which(&compiler[0]).ok()
        }?;
        let hash = rig::blake3::hash_file(&program).ok()?;
        self.depend(program.clone());
        let version = std::process::Command::new(&program).args(&compiler[1..]).arg("--version").output();
        let version = version.map(|output| [output.stdout, output.stderr].concat()).unwrap_or_default();
        let mut key = rig::cache::Key::new("jig c compiler");
        key.hash(Some(&hash)).part(&version);
        Some(rig::cache::hex(&key.finish()))
    }
    /// Returns the static libraries and executables, ordered by name and target.
    pub fn natives(&self) -> Vec<&Native> {
        let mut natives: Vec<_> = self.natives.values().collect();
        natives.sort_unstable_by(|lhs, rhs| (&lhs.name, &lhs.target).cmp(&(&rhs.name, &rhs.target)));
        natives
    }
    /// Returns the static or shared library for a target with a name, if one is declared.
    pub fn library(&self, name: &str, target: &str) -> Option<&Native> {
        self.native(name, target).filter(|native| native.kind != NativeKind::Executable)
    }
    /// Returns the static library, shared library or executable for a target with a name, if one is declared.
    pub fn native(&self, name: &str, target: &str) -> Option<&Native> {
        self.natives.get(&(name.to_string(), target.to_string()))
    }
    /// Returns the diagnostic of what a crate, library or executable uses which is not a library for its target: an
    /// executable, which cannot be used, or nothing.
    pub(super) fn unknown_library(&self, name: &str, target: &str, span: Span) -> Diagnostic {
        let message = match self.native(name, target) {
            Some(native) => Message::UseExecutable { name: name.into(), declared: native.name_span },
            None => Message::UnknownLibrary { name: name.into(), target: target.into() },
        };
        Diagnostic { span: Some(span), message, severity: Severity::Error }
    }
    /// Report what the libraries and executables use which is not a library for their target, libraries which use
    /// each other, and shared libraries for a target without an operating system.
    pub(super) fn check_libraries(&mut self) {
        let mut keys: Vec<_> = self.natives.keys().cloned().collect();
        keys.sort_unstable();
        let mut reported = Vec::new();
        for key in &keys {
            let library = &self.natives[key];
            for used in &library.uses {
                if self.library(&used.name, &library.target).is_none() {
                    reported.push(self.unknown_library(&used.name, &library.target, used.span));
                }
            }
            if library.kind == NativeKind::SharedLibrary && !has_shared_libraries(&library.target) {
                reported.push(Diagnostic {
                    span: Some(library.name_span),
                    message: Message::SharedLibraryTarget { target: library.target.clone() },
                    severity: Severity::Error,
                });
            }
        }
        // a library which reaches itself through those it uses
        for key in &keys {
            let library = &self.natives[key];
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
                if seen.insert(name) && let Some(used) = self.library(name, &library.target) {
                    stack.extend(used.uses.iter().map(|used| used.name.as_str()));
                }
            }
        }
        self.diagnostics.extend(reported);
    }
    /// Returns the libraries a crate, library or executable uses directly, among its `uses`, by their names.
    pub fn used_libraries<'a>(&'a self, uses: &'a [rust::Use], target: &'a str) -> impl Iterator<Item = &'a Native> {
        uses.iter().filter_map(move |used| self.library(&used.name, target))
    }
    /// Returns every library used by these, through each other too, each once, as for the directories of their headers.
    pub fn reachable_libraries<'a>(&'a self, uses: &'a [rust::Use], target: &'a str) -> Vec<&'a Native> {
        let mut found: Vec<&Native> = Vec::new();
        let mut stack: Vec<&Native> = self.used_libraries(uses, target).collect();
        while let Some(library) = stack.pop() {
            if found.iter().any(|known| known.name == library.name) {
                continue;
            }
            found.push(library);
            stack.extend(self.used_libraries(&library.uses, &library.target));
        }
        found
    }
    /// Returns the libraries which what uses these links, each once, in an order where each comes before those it uses,
    /// as a single-pass linker needs: the libraries used through each other, but for the static libraries a shared
    /// library uses, which are linked into it, while the shared libraries it uses are needed by what links it, to
    /// resolve what it refers to.
    pub fn link_libraries<'a>(&'a self, uses: &'a [rust::Use], target: &'a str) -> Vec<&'a Native> {
        fn visit<'a>(
            project: &'a Project,
            library: &'a Native,
            within_shared: bool,
            visited: &mut std::collections::HashSet<(&'a str, bool)>,
            order: &mut Vec<&'a Native>,
        ) {
            if !visited.insert((&library.name, within_shared)) {
                return;
            }
            let shared = library.kind == NativeKind::SharedLibrary;
            let dependencies: Vec<_> = project.used_libraries(&library.uses, &library.target).collect();
            for dependency in dependencies.into_iter().rev() {
                visit(project, dependency, within_shared || shared, visited, order);
            }
            let linked = shared || !within_shared;
            if linked && !order.iter().any(|known| std::ptr::eq(*known, library)) {
                order.push(library);
            }
        }
        let mut order = Vec::new();
        let mut visited = std::collections::HashSet::new();
        // in reverse, as the order is reversed, so that libraries which do not use each other are in the order they
        // are used in
        let direct: Vec<_> = self.used_libraries(uses, target).collect();
        for library in direct.into_iter().rev() {
            visit(self, library, false, &mut visited, &mut order);
        }
        // each after those it uses, reversed, so that each comes before them
        order.reverse();
        order
    }
}

/// Returns the library of the C++ runtime of a target, which links libc++ on Apple's and the BSDs' targets, whose
/// compilers are clang, and libstdc++ elsewhere, as GCC does.
fn default_cxx_runtime(target: &str) -> &'static str {
    let llvm = ["-apple-", "freebsd", "openbsd", "netbsd"].iter().any(|system| target.contains(system));
    if llvm { "c++" } else { "stdc++" }
}

/// Whether a target has shared libraries, as one with an operating system does, while one without, such as
/// `riscv64gc-unknown-none-elf`, does not.
fn has_shared_libraries(target: &str) -> bool {
    !target.split('-').any(|part| part == "none")
}

/// The attributes of a `static-library` statement which are taken by [`Project::static_library_attribute`].
struct NativeAttributes<'a> {
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
        Data::Ref(Ref { referent: Referent::Native { name, .. } | Referent::Crate { name, .. }, .. }) => name.clone(),
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
        assert_eq!(Language::of(Path::new("main.cpp")), Some(Language::Cxx));
        assert_eq!(Language::of(Path::new("main.cc")), Some(Language::Cxx));
        assert_eq!(Language::of(Path::new("main.f90")), None);
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
