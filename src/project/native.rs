//! Libraries and executables of C, C++ and assembly. Their sources are compiled by a C compiler, then archived or
//! linked, and the libraries are linked into the crates which use them.
//!
//! Also the properties which give their compilers, archivers and flags: `c/<name>` and `asm/<name>`, and
//! `c/<target>/<name>` for a single target.

use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
use super::running::{ATTRIBUTES, Running};
use super::rust;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Data, Operand, Property, Ref, Referent, Type};
use super::Project;

/// The language of a source, by its extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    C,
    /// Assembly: preprocessed for `.S` and `.sx`, and not for `.s`.
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
    /// Whether a file is a header of C or C++, by its extension.
    pub fn is_header(path: &Path) -> bool {
        path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
            matches!(extension, "h" | "hh" | "hpp" | "hxx" | "inc")
        })
    }
}

/// A source of a library, with its language.
#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub language: Language,
}

/// The kind of thing built from C and assembly, which decides whether its objects are archived or linked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeKind {
    /// A static library, `lib<name>.a`, which crates, shared libraries and executables link.
    StaticLibrary,
    /// A shared library, linked by the C compiler. Crates, shared libraries and executables link it, and load it as
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

/// A library or an executable for a target, made of objects compiled from C, C++ and assembly.
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
    /// The include directories which are also passed when compiling whatever uses the library.
    pub public_include: Vec<PathBuf>,
    /// The macros passed with `-D`, as `<name>` or `<name>=<value>`.
    pub defines: Vec<String>,
    /// The arguments passed to the compiler for every source, after those of the properties.
    pub flags: Vec<String>,
    /// The other libraries for the same target which this one uses.
    pub uses: Vec<rust::Use>,
    /// Whether every object is linked, even one nothing refers to.
    pub whole_archive: bool,
    /// Whether a static library is linked as its objects, in the order of its sources, rather than from an archive.
    /// No archive is then made.
    pub link_objects: bool,
    /// What is built before the library's sources are compiled, but is not an input.
    pub depends: Vec<PathBuf>,
    /// The attributes of the commands which compile, archive and link it: their environment, what they inherit, the
    /// tags of the workers they may be sent to, how their files are stamped, by contents unless `change-detection`
    /// says, and whether they are cached, as they are unless `cache false`.
    pub running: Running,
    /// Whether the library is built by `jig build` without targets.
    pub default: bool,
    /// The checks of clang-tidy which `jig lint` runs on its sources, from `c/tidy` where it is declared, if any.
    pub tidy: Option<String>,
    /// The compiler, and its first arguments.
    pub compiler: Vec<String>,
    /// The identity of the compiler in rig's cache, if rig caches and the compiler is found.
    /// [`Project::identify_static_libraries`] sets it once the configuration is evaluated.
    pub cache: Option<String>,
    /// The C++ compiler, and its first arguments, if it has C++ sources.
    pub cxx: Vec<String>,
    /// The identity of the C++ compiler in rig's cache, as `cache` is that of the C compiler.
    pub cxx_cache: Option<String>,
    /// The arguments for C++ sources, from `cxx/flags`, after those derived from the codegen options.
    pub cxx_flags: Vec<String>,
    /// The library of the C++ runtime, from `cxx/runtime`. Whatever links a static library with C++ sources links it.
    pub cxx_runtime: String,
    /// The archiver, and its first arguments.
    pub archiver: Vec<String>,
    /// The arguments for C sources, from `c/flags`, after those derived from the codegen options.
    pub c_flags: Vec<String>,
    /// The arguments for assembly sources, from `asm/flags`, after those derived from the codegen options.
    pub asm_flags: Vec<String>,
    /// The arguments the compiler is given when it links, from `c/link-flags`, followed by the executable's own.
    pub link_flags: Vec<String>,
    /// The linker script of an executable or shared library, which replaces that of its target.
    pub link_script: Option<PathBuf>,
    /// The directories where an executable or shared library looks, as it runs, for the shared libraries it uses,
    /// passed as `-Wl,-rpath,<dir>`.
    pub rpath: Vec<String>,
    /// Where `jig install` installs it, if it does.
    pub install: Install,
    /// The headers of a static library's public include directories, each with its directory, which `jig install`
    /// installs in `include`, under the path it has in the directory, with `install-headers`.
    pub headers: Vec<(PathBuf, PathBuf)>,
}

impl Native {
    /// Whether any of its sources is C++, so that what links it as a static library links the C++ runtime.
    pub fn has_cxx(&self) -> bool {
        self.sources.iter().any(|source| source.language == Language::Cxx)
    }
    /// What is built. For a static library it is the archive, `lib<name>.a`, in the library's own directory, so that
    /// the linker only finds it where it is used. For an executable it is the program, `<build-dir>/<target>/<name>`,
    /// as for a binary crate.
    pub fn output(&self) -> PathBuf {
        match self.kind {
            NativeKind::StaticLibrary => self.dir.join(format!("lib{}.a", self.name)),
            // named as the platform's linker looks for it
            NativeKind::SharedLibrary if self.target.contains("-windows") => self.dir.join(format!("{}.dll", self.name)),
            NativeKind::SharedLibrary if self.target.contains("-apple-") => self.dir.join(format!("lib{}.dylib", self.name)),
            NativeKind::SharedLibrary => self.dir.join(format!("lib{}.so", self.name)),
            NativeKind::Executable => {
                let suffix = if self.target.contains("-windows") { ".exe" } else { "" };
                self.dir.parent().unwrap().parent().unwrap().join(format!("{}{suffix}", self.name))
            },
        }
    }
    /// Whether its objects are compiled with `-fPIC`. Those of a shared library are. So are those of a static library
    /// for a target with an operating system, as a shared library may link it; Rust's own objects are built the same
    /// way for such targets. Windows is the exception, as its code is position-independent without the flag.
    pub fn position_independent(&self) -> bool {
        let hosted = has_shared_libraries(&self.target) && !self.target.contains("-windows");
        match self.kind {
            NativeKind::SharedLibrary | NativeKind::StaticLibrary => hosted,
            NativeKind::Executable => false,
        }
    }
    /// The files which whatever links it is given: its objects, for a static library linked as its objects, its
    /// import library, for a shared library for Windows, and otherwise its output.
    pub fn linked_files(&self) -> Vec<PathBuf> {
        if self.link_objects {
            return self.sources.iter().map(|source| self.object(&source.path)).collect();
        }
        vec![self.link_output()]
    }
    /// The file a library is linked by: its output, except for a shared library for Windows, which is linked by its
    /// import library, `lib<name>.dll.a`. Linking the shared library writes the import library beside it.
    pub fn link_output(&self) -> PathBuf {
        match self.kind {
            NativeKind::SharedLibrary if self.target.contains("-windows") => self.dir.join(format!("lib{}.dll.a", self.name)),
            _ => self.output(),
        }
    }
    /// The name `jig run` knows an executable by: its name, with its target if that is not the host target, as for a
    /// crate's test.
    pub fn short_name(&self, host: &str) -> String {
        if self.target == host { self.name.clone() } else { format!("{}@{}", self.name, self.target) }
    }
    /// The object compiled from a source, in the library's directory. It is named after the source's whole path, not
    /// only its file name, as members of an archive are named by file name alone: two sources with the same name in
    /// different directories must give different objects and different members.
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

/// The number of [`TOOLS`] which every library or executable needs. They come first. Those after them are for C++,
/// and are only read for something with C++ sources, so that `CXX` is not read otherwise.
const C_TOOLS: usize = 5;

/// Parses the name of a tool's property for a target, `c/<target>/<name>` or `asm/<target>/<name>`, into the target
/// and the tool. The target need not be a known one.
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

/// The names of the libraries which the standard library of a target links. A project's library with one of these
/// names replaces it wherever it is used.
const SYSTEM_LIBRARIES: &[&str] = &["c", "m", "dl", "rt", "pthread", "util", "gcc", "gcc_s", "unwind"];

impl Project {
    /// Declare the properties of the tools and flags for C and assembly. They have no value until they are set: their
    /// defaults are only read from the environment when a library needs them.
    pub fn declare_native_properties(&mut self) {
        for tool in TOOLS {
            let property = Property::new(tool.property.into(), Type::Any).info(tool.info);
            self.declare(tool.property.into(), Some(property));
        }
    }
    /// Declare the property of a tool for a target, `c/<target>/<name>`, if `name` is the name of one, its target is
    /// builtin or has been declared, and it is not declared yet. Returns whether `name` is the name of one.
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
    /// Returns the arguments which a tool or flags property gives for a target, as seen from `span`. The first of
    /// these which is set is used:
    ///
    /// 1. the property for the target, `c/<target>/<name>`;
    /// 2. the property, `c/<name>`;
    /// 3. the environment variable for the target, `<variable>_<target>`;
    /// 4. for the host target only, the environment variable, and then the fallback.
    ///
    /// The value of a variable is split at whitespace.
    fn tool_arguments(&mut self, tool: &Tool, target: &str, span: Span) -> Option<Vec<String>> {
        for name in [target_property(target, tool), tool.property.to_string()] {
            if let Some(value) = self.value(&name, span.file) {
                let operand = Operand { data: value.data.clone(), span, coerce: super::value::Coercion::Strict, origin: value.origin };
                return self.arguments(&operand);
            }
        }
        // a property with no variable, such as `cxx/runtime`, is not read from the environment
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
    /// Returns the arguments a value gives: a `str` is one argument, and a list is one for each item. Any other value
    /// is reported.
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
    /// Take a `static-library`, `shared-library` or `executable` statement. All take the `source`, `target`,
    /// `include`, `public-include`, `define`, `flags`, `use`, `depends`, `default`, the install attributes, and those
    /// of a command which runs, `env`, `pass-env`, `worker-tag`, `change-detection` and `cache`. A static library also
    /// takes `link`, `whole-archive` and `install-headers`, and the others `link-flags`, `link-script` and `rpath`.
    pub(super) fn parse_native(&mut self, parser: &mut Parser, kind: NativeKind) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { text: name, span, literal }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        // the name becomes part of a file name, `lib<name>.a`, and of a property's name, `<target>/<name>`
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
        let mut link_objects = false;
        let mut default = false;
        let mut running = Running::default();
        let mut link_flags = Vec::new();
        let mut link_script = None;
        let mut rpath = Vec::new();
        // a shared library is installed as executable, as a procedural macro is, and an archive is not
        let mut install = match kind {
            NativeKind::Executable => Install::executable(),
            NativeKind::SharedLibrary => Install::library(0o755),
            NativeKind::StaticLibrary => Install::library(0o644),
        };
        let mut install_headers = false;
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
                // `use` takes the names of libraries, as a crate's `use` takes the names of crates
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
                "install" | "install-dir" | "install-mode" | "install-headers" if kind == NativeKind::StaticLibrary => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        continue;
                    };
                    if attribute == "install-headers" {
                        install_headers = self.flag(&operand).unwrap_or(install_headers);
                    } else {
                        self.install_attribute(&mut install, attribute, operand);
                    }
                },
                // whether a static library is linked from its archive, or as its objects
                "link" if kind == NativeKind::StaticLibrary => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        continue;
                    };
                    match &operand.data {
                        Data::Str(link) if link == "archive" || link == "objects" => link_objects = link == "objects",
                        _ => self.invalid_operand(&operand, "`'archive'` or `'objects'`", false),
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
                name if ATTRIBUTES.contains(&name) => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        continue;
                    };
                    self.running_attribute(&mut running, name, &operand);
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
        // the headers which are installed with the archive, found now, so that the project is configured again when
        // one is added
        let headers = if install.enabled && install_headers { self.headers(&public_include) } else { Vec::new() };
        let (targets, target_span) = targets.unwrap_or_else(|| {
            let target = self.property("target", span).map(|target| target.data.coerce_string());
            (vec![target.unwrap_or_default()], span)
        });
        // derive `-O` and `-g` from the codegen options in effect where the library is declared, so that a profile
        // applies to C too
        let codegen = self.codegen_options(span, rust::Kind::Lib, &[], &targets);
        let tidy = self.value("c/tidy", span.file).map(|checks| checks.data.coerce_string());
        let tidy = tidy.filter(|checks| !checks.is_empty());
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
            // the C++ tools are only read for a library with C++ sources
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
            // something linked by the compiler, or linked as its objects, needs no archiver
            let unarchived = kind.is_linked() || link_objects;
            let archiver = if unarchived { Some(archiver.unwrap_or_default()) } else { archiver };
            let archiver = archiver.filter(|a| !a.is_empty() || unarchived);
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
                link_objects,
                depends: depends.clone(),
                running: running.clone(),
                default,
                tidy: tidy.clone(),
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
                headers: headers.clone(),
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
                    let paths = entry.insert(library).linked_files();
                    let referent = Referent::Native { name: name.clone(), target: target.clone() };
                    let property = format!("{target}/{name}");
                    self.declare_reference(property, span, kind.item(), Ref { referent, paths });
                },
            }
        }
    }
    /// Returns the headers in directories and those within them, each with its directory, in order, which the
    /// configuration depends on the directories of, so that it is configured again when a header is added.
    fn headers(&mut self, dirs: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
        let mut headers = Vec::new();
        for dir in dirs {
            let mut pending = vec![dir.clone()];
            while let Some(current) = pending.pop() {
                self.depend_on_path(&current);
                let Ok(entries) = std::fs::read_dir(&current) else {
                    continue;
                };
                let mut found = Vec::new();
                for entry in entries.flatten() {
                    let path = current.join(entry.file_name());
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        pending.push(path);
                    } else if Language::is_header(&path) {
                        found.push((path, dir.clone()));
                    }
                }
                headers.extend(found);
            }
        }
        headers.sort();
        headers.dedup();
        headers
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
    /// Returns the macros of a `define` attribute, each as `<name>` or `<name>=<value>`. The attribute is a name, a
    /// list of names, or a dict from names to values.
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
    /// Find the identity in rig's cache of the compiler of each library, if rig caches, so that what one compiler
    /// compiled is never fetched for another.
    ///
    /// The identity is made from the contents of the compiler's program and what the command prints as its version.
    /// With `ccache gcc`, for example, the version printed is gcc's. The configuration is made to depend on the
    /// program, so that the project is configured again when the compiler is replaced. A compiler which is not found
    /// gets no identity, so it is not cached, and its compiles fail when they run.
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
            // one with `cache false` has no identity, so that it is not cached
            if self.natives[&key].running.cache == Some(false) {
                continue;
            }
            let (compiler, cxx) = (self.natives[&key].compiler.clone(), self.natives[&key].cxx.clone());
            let c_compiler = identify(self, compiler);
            let cxx_compiler = identify(self, cxx);
            let native = self.natives.get_mut(&key).unwrap();
            native.cache = c_compiler;
            native.cxx_cache = cxx_compiler;
        }
    }
    /// Returns the identity of a compiler, given as its program and first arguments, if the program is found.
    fn compiler_identity(&mut self, compiler: &[String]) -> Option<String> {
        let program = if compiler[0].contains('/') || (cfg!(windows) && compiler[0].contains('\\')) {
            // a path is relative to the project root; rig runs it from the build directory, with the path rebased
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
    /// Returns the diagnostic for a `use` of something which is not a library for the target. It says whether the
    /// name is an executable, which cannot be used, a Rust crate, or nothing at all.
    pub(super) fn unknown_library(&self, name: &str, target: &str, span: Span) -> Diagnostic {
        let rust = self.crates.get(&(name.to_string(), target.to_string()));
        let message = match self.native(name, target) {
            Some(native) => Message::UseExecutable { name: name.into(), declared: native.name_span },
            None if let Some(c) = rust => Message::UseRustCrate { name: name.into(), declared: c.name_span },
            None => Message::UnknownLibrary { name: name.into(), target: target.into() },
        };
        Diagnostic { span: Some(span), message, severity: Severity::Error }
    }
    /// Report three kinds of error: a library or executable which uses something that is not a library for its
    /// target, libraries which use each other in a cycle, and a shared library for a target without an operating
    /// system.
    pub(super) fn check_libraries(&mut self) {
        let mut keys: Vec<_> = self.natives.keys().cloned().collect();
        keys.sort_unstable();
        let mut reported = Vec::new();
        for key in &keys {
            let library = &self.natives[key];
            for used in &library.uses {
                let for_c = self.crate_for_c(&used.name, &library.target).is_some();
                if self.library(&used.name, &library.target).is_none() && !for_c {
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
        // report a library which reaches itself through the libraries it uses
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
    /// Returns the crate for C, a `staticlib` or `cdylib`, for a target with a name, if one is declared.
    pub fn crate_for_c(&self, name: &str, target: &str) -> Option<&rust::Crate> {
        self.crates.get(&(name.to_string(), target.to_string())).filter(|c| c.kind.is_for_c())
    }
    /// Returns the names of the crates for C which a library or executable links. Those are whatever it uses, or the
    /// static libraries it links use, which is not a library of C: `check_libraries` has checked that such a name is
    /// a crate for C. Each name is returned once, its own before those of its static libraries.
    ///
    /// Only names are returned, as the caller looks the crates up: generating the graph takes the crates out of the
    /// project.
    pub fn linked_crates<'a>(&'a self, native: &'a Native) -> Vec<&'a str> {
        let statics = self.link_libraries(&native.uses, &native.target);
        let statics = statics.into_iter().filter(|library| library.kind == NativeKind::StaticLibrary);
        let mut names: Vec<&str> = Vec::new();
        for used in native.uses.iter().chain(statics.flat_map(|library| &library.uses)) {
            if self.native(&used.name, &native.target).is_none() && !names.contains(&used.name.as_str()) {
                names.push(&used.name);
            }
        }
        names
    }
    /// Returns the libraries a crate, library or executable uses directly, among its `uses`, by their names.
    pub fn used_libraries<'a>(&'a self, uses: &'a [rust::Use], target: &'a str) -> impl Iterator<Item = &'a Native> {
        uses.iter().filter_map(move |used| self.library(&used.name, target))
    }
    /// Returns every library these `uses` reach, directly or through other libraries, each once. Their public
    /// include directories are needed, for example.
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
    /// Returns the libraries to link for something with these `uses`, each once, ordered so that each library comes
    /// before the libraries it uses, as a single-pass linker needs.
    ///
    /// Those are the libraries reached directly or through other libraries, with one exception: the static libraries
    /// which a shared library uses are already linked into it, so they are left out. The shared libraries which a
    /// shared library uses are kept, as the linker needs them to resolve what it refers to.
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
        // The order is built with each library after those it uses, and reversed at the end. So the direct uses are
        // visited in reverse here, which leaves libraries that do not use each other in the order they were used.
        let direct: Vec<_> = self.used_libraries(uses, target).collect();
        for library in direct.into_iter().rev() {
            visit(self, library, false, &mut visited, &mut order);
        }
        // reverse the order, so that each library comes before those it uses
        order.reverse();
        order
    }
}

/// Returns the default library of the C++ runtime for a target: libc++ on Apple's and the BSDs' targets, whose
/// compilers are clang, and libstdc++ elsewhere, as with GCC.
fn default_cxx_runtime(target: &str) -> &'static str {
    let llvm = ["-apple-", "freebsd", "openbsd", "netbsd"].iter().any(|system| target.contains(system));
    if llvm { "c++" } else { "stdc++" }
}

/// Whether a target has shared libraries. One with an operating system does, and one without, such as
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
    targets: &'a mut Option<(Vec<String>, Span)>,
}

/// Returns the name of a library from a `use` attribute, which is its name, or a reference to it.
fn library_name(data: &Data) -> String {
    match data {
        Data::Ref(Ref { referent: Referent::Native { name, .. } | Referent::Crate { name, .. }, .. }) => name.clone(),
        data => data.coerce_string(),
    }
}

/// Returns the C compiler flags derived from codegen options, which are given as `<option>=<value>`: `-O<level>`
/// from `opt-level`, and `-g` from any `debuginfo` but none.
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
