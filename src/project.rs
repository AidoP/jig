use std::{collections::{HashMap, hash_map::Entry}, path::{Path, PathBuf}};

mod codegen;
mod crates;
mod customs;
mod diagnostic;
mod expressions;
mod functions;
mod install;
mod meta;
mod properties;
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
    pub files: Vec<File>,
    /// Files read by expressions, which the configuration depends on as well as `files`.
    pub dependencies: Vec<PathBuf>,
    /// Paths which expressions found were missing, which the configuration would depend on if they existed.
    pub missing: Vec<PathBuf>,
    files_idx: HashMap<PathBuf, usize>,
    properties: Vec<Option<Property>>,
    properties_idx: HashMap<String, usize>,
    values: Vec<Value>,
    values_idx: HashMap<String, usize>,
    pub diagnostics: Vec<Diagnostic>,
    /// The number of diagnostics which have been given the severities of their `diagnostic/<id>` properties.
    overridden: usize,
    /// The directory of what is built, from the `build-dir` property, once it has been used.
    build_dir: Option<PathBuf>,
    /// The values given by `@with` which it has restored, which are no longer the values of their properties, but are
    /// checked by a later declaration, as other values set before it are.
    reverted: Vec<usize>,
    /// The targets supported by the compiler, followed by those declared by `target` statements, in the order they were
    /// declared.
    pub targets: Vec<Target>,
    /// Whether the supported targets have been listed by a diagnostic.
    targets_listed: bool,

    /// Crates keyed by name and target, as a crate may be compiled for several targets.
    pub crates: HashMap<(String, String), rust::Crate>,
    /// Custom build commands, in the order they were declared.
    pub customs: Vec<Custom>,
    /// The runners of targets, which targets and tests refer to by index.
    pub runners: Vec<Runner>,
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
            values_idx: HashMap::new(),
            diagnostics: Vec::new(),
            overridden: 0,
            reverted: Vec::new(),
            build_dir: None,
            targets: targets.into_iter().map(Target::builtin).collect(),
            targets_listed: false,
            crates: HashMap::new(),
            customs: Vec::new(),
            runners: Vec::new(),
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
        // a file can be referred to by many paths, so identify it by its canonical path
        let path = std::fs::canonicalize(&file.path).unwrap_or_else(|_| file.path.clone());
        match self.files_idx.entry(path) {
            Entry::Occupied(entry) => return Some(*entry.get()),
            Entry::Vacant(entry) => entry.insert(idx),
        };
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
        self.values.push(value);
        self.values_idx.insert(name, id);
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
            self.diagnostics.push(Diagnostic {
                span: self.values[value].span,
                message: Message::UndeclaredProperty(name),
                severity: Severity::Error,
            });
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

pub struct File {
    pub path: PathBuf,
    data: String,
    locals: HashMap<String, usize>,
    /// Whether the file is currently being processed.
    active: bool,
    /// The `use` statement which caused the file to be processed.
    used: Option<Span>,
}
impl File {
    pub fn read(path: std::path::PathBuf) -> std::io::Result<Self> {
        Ok(Self {
            data: std::fs::read_to_string(&path)?,
            path,
            locals: HashMap::new(),
            active: false,
            used: None,
        })
    }
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
