use std::{collections::{HashMap, hash_map::Entry}, path::{Path, PathBuf}};

mod codegen;
mod crates;
mod customs;
mod diagnostic;
mod expressions;
mod functions;
mod install;
mod meta;
mod probe;
mod properties;
mod semver;
mod references;
pub mod rust;
mod statements;
mod syntax;
mod targets;
mod value;

pub use customs::CustomKind;
pub use diagnostic::{Diagnostic, Message, Severity, severity_override};
pub use install::Installed;
pub use syntax::Span;
pub use targets::{Field, Target, TargetKind};
pub use value::{Data, Property, Type, Value};

use customs::{Custom, Runner};

pub struct Project {
    root: PathBuf,
    /// The rustup toolchain the project is configured with, which the programs `run` runs are given.
    pub toolchain: Option<String>,
    pub files: Vec<File>,
    /// Files read by expressions, which the configuration depends on as well as `files`.
    pub dependencies: Vec<PathBuf>,
    /// Paths which expressions found were missing, which the configuration would depend on if they existed.
    pub missing: Vec<PathBuf>,
    files_idx: HashMap<FileId, usize>,
    properties: Vec<Option<Property>>,
    properties_idx: HashMap<String, usize>,
    values: Vec<Value>,
    /// The indices in `values` of the values assigned to each property, in the order they were assigned.
    property_values: HashMap<usize, Vec<usize>>,
    values_idx: HashMap<String, usize>,
    pub diagnostics: Vec<Diagnostic>,
    /// The number of diagnostics which have been given the severities of their `diagnostic/<id>` properties.
    overridden: usize,
    /// Whether a fatal diagnostic has been reported, which stops the configuration, so no more statements are
    /// processed, nor is the project checked or generated.
    pub fatal: bool,
    /// The directory of what is built, from the `build-dir` property, once it has been used.
    build_dir: Option<PathBuf>,
    /// The values given by `@with` which it has restored, which are no longer the values of their properties, but are
    /// checked by a later declaration, as other values set before it are.
    reverted: Vec<usize>,
    /// The start of the statement or meta-statement being processed, where a meta-statement can be inserted to apply to
    /// it, unless it is an `@else`, which must be first.
    meta_insertion: Option<Span>,
    /// The targets supported by the compiler, followed by those declared by `target` statements, in the order they were
    /// declared.
    pub targets: Vec<Target>,
    /// Whether the supported targets have been listed by a diagnostic.
    targets_listed: bool,

    /// Crates keyed by name and target, as a crate may be compiled for several targets.
    pub crates: HashMap<(String, String), rust::Crate>,
    /// Custom build commands, in the order they were declared.
    pub customs: Vec<Custom>,
    /// The indices in `customs` of the custom commands, by name.
    commands_idx: HashMap<String, usize>,
    /// The indices in `customs` of the tests, by name, which are separate from those of the custom commands.
    tests_idx: HashMap<String, usize>,
    /// The runners of targets, which targets and tests refer to by index.
    pub runners: Vec<Runner>,
    /// Whether the host can run what is built for each target which has been probed.
    can_run: HashMap<String, bool>,
}
impl Project {
    pub fn new(root: PathBuf, targets: Vec<String>) -> Self {
        Self {
            root,
            files: Vec::new(),
            dependencies: Vec::new(),
            missing: Vec::new(),
            files_idx: HashMap::new(),
            properties: Vec::new(),
            properties_idx: HashMap::new(),
            values: Vec::new(),
            property_values: HashMap::new(),
            values_idx: HashMap::new(),
            diagnostics: Vec::new(),
            overridden: 0,
            fatal: false,
            reverted: Vec::new(),
            meta_insertion: None,
            build_dir: None,
            targets: targets.into_iter().map(Target::builtin).collect(),
            targets_listed: false,
            crates: HashMap::new(),
            customs: Vec::new(),
            commands_idx: HashMap::new(),
            tests_idx: HashMap::new(),
            runners: Vec::new(),
            toolchain: None,
            can_run: HashMap::new(),
        }
    }
    pub fn file(&self, idx: usize) -> &File {
        &self.files[idx]
    }
    /// Process a file, unless it has already been processed.
    ///
    /// Returns the index of the file if it has already been processed.
    pub fn parse(&mut self, file: File) -> Option<usize> {
        let idx = self.files.len();
        // a file can be referred to by many paths, so identify it by its device and inode, which takes a single `stat`
        // rather than reading every symbolic link in its path, as finding its canonical path would
        use std::os::unix::fs::MetadataExt;
        // a builtin file, such as a profile, is processed each time it is used
        if !file.builtin {
            let id = match std::fs::metadata(&file.path) {
                Ok(metadata) => FileId::Inode { dev: metadata.dev(), ino: metadata.ino() },
                Err(_) => FileId::Path(file.path.clone()),
            };
            match self.files_idx.entry(id) {
                Entry::Occupied(entry) => return Some(*entry.get()),
                Entry::Vacant(entry) => entry.insert(idx),
            };
        }
        self.files.push(file);
        self.files[idx].active = true;
        self.parse_file(idx);
        self.files[idx].active = false;
        None
    }
    /// Declare a property, returning its ID.
    ///
    /// If `property` is [`None`], the proprty is forward-declared.
    pub fn declare(&mut self, name: String, property: Option<Property>) -> usize {
        match self.properties_idx.entry(name) {
            Entry::Occupied(entry) => {
                let id = *entry.get();
                let Some(property_ref) = &property else {
                    // forward declaration, permit an existing property
                    return id;
                };
                match &mut self.properties[id] {
                    Some(existing) => self.diagnostics.push(Diagnostic {
                        span: property_ref.span,
                        message: Message::RedeclareProperty {
                            id,
                            name: property_ref.name.clone(),
                            builtin: existing.span.is_none(),
                        },
                        severity: Severity::Error,
                    }),
                    value @ None => {
                        *value = property;
                    },
                }
                return id;
            },
            Entry::Vacant(entry) => {
                let id = self.properties.len();
                entry.insert(id);
                self.properties.push(property);
                return id;
            },
        }
    }
    /// Assign a value to a property, returning the ID of the value handle.
    pub fn set(&mut self, name: String, value: Value) -> usize {
        let id = self.values.len();
        if let Some(span) = &value.span {
            let file = &mut self.files[span.file];
            if value.local {
                file.locals.insert(name.clone(), id);
            } else {
                let _ = file.locals.remove(&name);
            }
        }
        self.push_value(value);
        self.values_idx.insert(name, id);
        id
    }
    /// Add a value, returning its index.
    fn push_value(&mut self, value: Value) -> usize {
        let id = self.values.len();
        // a local value, given by `@for`, is not a property's
        if value.property != usize::MAX {
            self.property_values.entry(value.property).or_default().push(id);
        }
        self.values.push(value);
        id
    }
    fn value(&self, name: &str, file: usize) -> Option<&Value> {
        let mut idx = self.files[file].locals.get(name);
        if idx.is_none() {
            idx = self.values_idx.get(name);
        }
        Some(&self.values[*idx?])
    }
    /// The directory of what is built, from the `build-dir` property, relative to the project root.
    ///
    /// The property is read-only once this is used, so that everything is built in the same directory.
    pub fn build_dir(&mut self) -> PathBuf {
        if let Some(dir) = &self.build_dir {
            return dir.clone();
        }
        let dir = match self.values_idx.get("build-dir") {
            Some(&value) => PathBuf::from(self.values[value].data.coerce_string()),
            None => PathBuf::from("build"),
        };
        if let Some(&id) = self.properties_idx.get("build-dir") && let Some(property) = &mut self.properties[id] {
            property.read_only = true;
        }
        self.build_dir = Some(dir.clone());
        dir
    }
    /// The target triple of the machine running the build, which is the value of the builtin `host-target` property.
    pub(super) fn host(&self) -> String {
        let Some(&value) = self.values_idx.get("host-target") else {
            return String::new();
        };
        self.values[value].data.coerce_string()
    }
    /// The directory `jig install` installs in by default, which is the value of the builtin `install-prefix` property.
    pub fn install_prefix(&self) -> String {
        let Some(&value) = self.values_idx.get("install-prefix") else {
            return String::new();
        };
        self.values[value].data.coerce_string()
    }
    /// Returns the keys of the crates in the order they were declared, as the crates are not kept in order.
    ///
    /// The crates declared by one statement, for several targets or with `@for`, are ordered by name and target.
    pub(super) fn crate_keys(&self) -> Vec<(String, String)> {
        let mut keys = Vec::new();
        for key in self.crates.keys() {
            keys.push(key.clone());
        }
        keys.sort_by(|lhs, rhs| {
            let (lhs_span, rhs_span) = (self.crates[lhs].name_span, self.crates[rhs].name_span);
            (lhs_span.file, lhs_span.offset, lhs).cmp(&(rhs_span.file, rhs_span.offset, rhs))
        });
        keys
    }
    /// Check for various conditions that cannot be evaluated as errors until reading has completed.
    /// - Unknown targets for crates, as a `target` statement may come later.
    /// - Missing declarations for `set` statements.
    /// - Missing crates for `use` attributes.
    /// - Outputs of custom commands which overlap other outputs.
    /// - Files which are installed at the same destination.
    pub fn check(&mut self) {
        let keys = self.crate_keys();
        for key in &keys {
            let c = &self.crates[key];
            let target = c.target.clone();
            let span = c.target_span;
            self.check_target(&target, span);
        }
        self.add_crate_tests();
        // in the order the values were set, as the properties are not kept in order
        // the codegen options of a target which is declared after they are set
        let names: Vec<_> = self.values_idx.keys().cloned().collect();
        for name in names {
            self.declare_target_codegen_option(&name);
        }
        let mut undeclared = Vec::new();
        for (name, &idx) in &self.values_idx {
            if self.properties[self.values[idx].property].is_none() {
                undeclared.push(UndeclaredValue { name: name.clone(), value: idx });
            }
        }
        // a value restored by `@with` is not the value of its property, which may have no other
        for &value in &self.reverted {
            let property = self.values[value].property;
            if self.properties[property].is_some() {
                continue;
            }
            let Some((name, _)) = self.properties_idx.iter().find(|&(_, &id)| id == property) else {
                continue;
            };
            if undeclared.iter().all(|undeclared| undeclared.name != *name) {
                undeclared.push(UndeclaredValue { name: name.clone(), value });
            }
        }
        undeclared.sort_by_key(|undeclared| undeclared.value);
        for UndeclaredValue { name, value } in undeclared {
            // a codegen option of a target which is not known
            let message = match self.target_of_codegen_option(&name) {
                Some(target) => {
                    let suggestion = diagnostic::suggest(target, &self.builtin_targets());
                    Message::UnknownTarget { target: target.into(), suggestion }
                },
                None => Message::UndeclaredProperty(name),
            };
            self.diagnostics.push(Diagnostic { span: self.values[value].span, message, severity: Severity::Error });
        }
        let host = self.host();
        for key in &keys {
            let c = &self.crates[key];
            for used in &c.uses {
                let mut d = self.crates.get(&(used.name.clone(), c.target.clone()));
                // a procedural macro is for the host target, but can be used by a crate for any target
                if d.is_none()
                    && let Some(host_crate) = self.crates.get(&(used.name.clone(), host.clone()))
                    && host_crate.kind == rust::Kind::ProcMacro
                {
                    d = Some(host_crate);
                }
                let message = match d {
                    None => Message::UnknownCrate { name: used.name.clone(), target: c.target.clone() },
                    Some(d) if d.kind == rust::Kind::Bin => {
                        Message::UseBinary { name: used.name.clone(), declared: d.name_span }
                    },
                    Some(_) => continue,
                };
                self.diagnostics.push(Diagnostic { span: Some(used.span), message, severity: Severity::Error });
            }
        }
        self.check_outputs();
        self.check_installs();
        self.override_severities();
    }
}

/// A property which is never declared, with the value it is reported at.
struct UndeclaredValue {
    name: String,
    value: usize,
}

/// Remove `.` and resolve `..` in a path without accessing the file system.
///
/// A `..` which cannot be resolved is kept in a relative path, and removed at the root of an absolute path.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => match normal.components().next_back() {
                Some(Component::Normal(_)) => {
                    normal.pop();
                },
                Some(Component::RootDir | Component::Prefix(_)) => (),
                _ => normal.push(".."),
            },
            component => normal.push(component),
        }
    }
    if normal.as_os_str().is_empty() {
        normal.push(".");
    }
    normal
}

/// What identifies a file which has been used, however its path was written.
#[derive(PartialEq, Eq, Hash)]
enum FileId {
    Inode { dev: u64, ino: u64 },
    /// The path of a file whose metadata could not be read.
    Path(PathBuf),
}

/// Returns a path with its `.` components removed, and each `..` resolved where it follows a directory which is not a
/// symbolic link, so that it still names the same file, and does not grow with each `..` in a chain of paths.
///
/// A `..` after a symbolic link leaves the directory the link points to, rather than the one containing the link, so it
/// is kept, as is one after a path which does not exist, which would fail to be read.
fn resolve_parents(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir
                if matches!(resolved.components().next_back(), Some(Component::Normal(_)))
                    && resolved.symlink_metadata().is_ok_and(|metadata| metadata.is_dir()) =>
            {
                resolved.pop();
            },
            component => resolved.push(component),
        }
    }
    if resolved.as_os_str().is_empty() {
        resolved.push(".");
    }
    resolved
}

pub struct File {
    pub path: PathBuf,
    data: String,
    locals: HashMap<String, usize>,
    /// Whether the file is currently being processed.
    active: bool,
    /// The `use` statement which caused the file to be processed.
    used: Option<Span>,
    /// Whether the file is part of jig, as `msrv` and the profiles are, rather than on disk.
    pub builtin: bool,
}
impl File {
    pub fn read(path: std::path::PathBuf) -> std::io::Result<Self> {
        let data = std::fs::read_to_string(&path)?;
        Ok(Self::new(path, data))
    }
    /// A file with its contents, which need not exist.
    pub fn new(path: PathBuf, data: String) -> Self {
        Self { data, path, locals: HashMap::new(), active: false, used: None, builtin: false }
    }
    /// The builtin file with the name, such as `msrv` or `profile/<name>`, if there is one.
    pub fn builtin(name: &str) -> Option<Self> {
        let data = BUILTINS.iter().find(|(builtin, _)| *builtin == name)?.1;
        Some(Self { builtin: true, ..Self::new(name.into(), data.into()) })
    }
}

/// The builtin files, which are used by name, as `use <name>`, rather than by path.
pub const BUILTINS: &[(&str, &str)] = &[
    ("msrv", include_str!("project/builtin/msrv.jig")),
    ("profile/debug", include_str!("project/builtin/profile/debug.jig")),
    ("profile/release", include_str!("project/builtin/profile/release.jig")),
];

impl File {
    /// Returns the path relative to the file.
    pub fn relative_path<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        self.path.parent().unwrap().join(path)
    }
    /// Returns the line and column of an offset within the file.
    pub fn position(&self, offset: usize) -> (usize, usize) {
        let before = &self.data[..offset];
        let line = before.matches('\n').count() + 1;
        let col = offset - before.rfind('\n').map_or(0, |i| i + 1) + 1;
        (line, col)
    }
}

#[cfg(test)]
mod benches {
    use super::*;
    use crate::bench::{assert_linear, configure};

    /// Configure the source `source` gives for each size, which must be valid.
    #[track_caller]
    fn assert_configures_linearly(name: &str, size: usize, source: impl Fn(usize) -> String) {
        let valid = configure(&source(1));
        assert!(valid.diagnostics.iter().all(|d| d.severity != Severity::Error), "the source of {name} is invalid");
        assert_linear(name, size, source, |source| {
            configure(source);
        });
    }

    #[test]
    fn statements() {
        assert_configures_linearly("statements", 2000, |n| {
            let mut source = String::from("declare a\n    default 1\n");
            for i in 0..n {
                source += &format!("set a (a * 3 + {i}) % 1000003\n");
            }
            source
        });
    }

    #[test]
    fn string_templates() {
        assert_configures_linearly("string-templates", 1000, |n| {
            let mut source = String::from("declare a\n    default 1\ndeclare s\n");
            for i in 0..n {
                source += &format!("set s '{{a}} {{upper('x{i}')}} {{len(split('a,b,c', ','))}} {{join(['x', a], '-')}}'\n");
            }
            source
        });
    }

    #[test]
    fn properties() {
        assert_configures_linearly("properties", 8000, |n| {
            let mut source = String::new();
            for i in 0..n {
                source += &format!("declare p{i}\n    default {i}\n");
                if i > 0 {
                    source += &format!("set p{i} p{} + 1\n", i - 1);
                }
            }
            source
        });
    }

    #[test]
    fn properties_set_before_declared() {
        assert_configures_linearly("set-before-declared", 8000, |n| {
            let sets = (0..n).map(|i| format!("set p{i} {i}\n"));
            let declarations = (0..n).map(|i| format!("declare p{i}\n    type 'unsigned'\n"));
            sets.chain(declarations).collect()
        });
    }

    #[test]
    fn for_loop() {
        assert_configures_linearly("for-loop", 5000, |n| {
            let values: Vec<_> = (0..n).map(|i| i.to_string()).collect();
            format!("declare total\n    default 0\n@for i in {}\nset total total + i\n", values.join(" "))
        });
    }

    #[test]
    fn crate_chain() {
        assert_configures_linearly("crate-chain", 500, |n| {
            let mut source = String::from("crate c0 'c.rs'\n");
            for i in 1..n {
                source += &format!("crate c{i} 'c.rs'\n    use c{}\n", i - 1);
            }
            source
        });
    }

    #[test]
    fn crates_using_one() {
        assert_configures_linearly("crates-using-one", 500, |n| {
            let mut source = String::from("crate base 'c.rs'\n");
            for i in 0..n {
                source += &format!("crate c{i} 'c.rs'\n    use base\n");
            }
            source
        });
    }

    #[test]
    fn custom_commands() {
        assert_configures_linearly("custom-commands", 1000, |n| {
            let mut source = String::from("custom k0\n    output 'gen/k0'\n    command 'touch' output\n");
            for i in 1..n {
                source += &format!("custom k{i}\n    input k{}\n    output 'gen/k{i}'\n    command 'cp' input output\n", i - 1);
            }
            source
        });
    }

    #[test]
    fn resolving_parents() {
        let dir = std::env::temp_dir().join(format!("jig-bench-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("d")).unwrap();
        assert_linear("resolve-parents", 200, |n| dir.join("d/../".repeat(n)).join("Jig"), |path| {
            resolve_parents(path);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
