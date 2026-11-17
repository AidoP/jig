//! Crates, and the tests of their test harnesses.

use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use super::codegen::{CodegenAttribute, is_codegen_option};
use super::customs::{Custom, CustomKind, TestRun};
use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Coercion, Data, Operand, Ref, Referent};
use super::running::{ATTRIBUTES, Running};
use super::{Project, rust};

/// Which of a crate's targets its tests are built for, from its `test` attribute: all or none of them, or those named.
enum Tested {
    All(bool),
    Only(Vec<String>),
}
impl Tested {
    fn on(&self, target: &str) -> bool {
        match self {
            Self::All(tested) => *tested,
            Self::Only(targets) => targets.iter().any(|tested| tested == target),
        }
    }
}

/// The targets a crate is declared for, from its `target` attribute or the property in effect.
struct CrateTargets {
    names: Vec<String>,
    span: Span,
}

impl Project {
    pub(super) fn parse_crate(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { text: name, span, literal }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        // the name is passed to rustc, which rejects `-` even though it is valid in an identifier, and a `.`, which a
        // computed name may have, while a `/` would be part of the name it is referred to by, `<target>/<name>`
        if let Some((i, character)) = name.char_indices().find(|&(_, c)| matches!(c, '-' | '/' | '.')) {
            // point at the character if the name was written as an identifier, otherwise its position is unknown
            let span = if literal {
                Span { file: span.file, offset: span.offset + i, len: character.len_utf8() }
            } else {
                span
            };
            parser.skip_statement();
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::InvalidCrateName { name, character },
                severity: Severity::Error,
            });
            return;
        }
        let has_path = parser.peek() == Some(' ') && self.parse_space(parser);
        let path = if has_path && let Some((path, _)) = self.parse_path(parser) {
            path
        } else {
            self.files[parser.file].relative_path(&name).join("lib.rs")
        };
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        let mut builtin = false;
        let mut default = false;
        let mut tested = Tested::All(false);
        let mut doc = false;
        let mut tags = Vec::new();
        // how its test runs, from `timeout` and `serial`
        let mut test_run = self.default_test_run(parser.file);
        let mut targets = None;
        let mut uses = Vec::new();
        let mut depends = Vec::new();
        let mut cfg = Vec::new();
        // a crate is cached by default, when rig caches
        let mut running = Running::default();
        let mut kind = rust::Kind::Lib;
        let mut link_script = None;
        let mut codegen = Vec::new();
        // the defaults of the install attributes depend on the crate type, so they are applied once it is known
        let mut install_attributes = Vec::new();
        while let Some(attribute) = self.next_attribute(parser, &["use"]) {
            match attribute.name {
                "builtin" => {
                    if let Some(value) = self.flag(&attribute.operand()) {
                        builtin = value;
                    }
                },
                "default" => {
                    if let Some(value) = self.flag(&attribute.operand()) {
                        default = value;
                    }
                },
                // whether its tests are built, or for which of its targets, by their names
                "test" => {
                    let operand = attribute.operand();
                    // a `str` in `{}` which is `true` or `false` is coerced to a `bool`, as for a flag
                    let flag = operand.coerce == Coercion::Any && operand.data.coerce_bool().is_some();
                    match &operand.data {
                        Data::Str(target) if !flag => tested = Tested::Only(vec![target.clone()]),
                        Data::List(items) if items.iter().all(|item| matches!(item, Data::Str(_))) => {
                            tested = Tested::Only(items.iter().map(Data::coerce_string).collect());
                        },
                        Data::List(_) => self.invalid_operand(&operand, "bool, str or list of str", false),
                        _ => {
                            if let Some(value) = self.flag(&operand) {
                                tested = Tested::All(value);
                            }
                        },
                    }
                },
                "doc" => {
                    if let Some(value) = self.flag(&attribute.operand()) {
                        doc = value;
                    }
                },
                "tag" => {
                    if let Some(new) = self.tags(&attribute.operand()) {
                        tags.extend(new);
                    }
                },
                name @ ("timeout" | "serial") => self.test_run_attribute(&mut test_run, name, &attribute.operand()),
                "crate-type" => {
                    let span = attribute.data_span;
                    let Some(crate_type) = self.str_operand(attribute.operand()) else {
                        continue;
                    };
                    match crate_type.as_str() {
                        "lib" => kind = rust::Kind::Lib,
                        "bin" => kind = rust::Kind::Bin,
                        "proc-macro" => kind = rust::Kind::ProcMacro,
                        "staticlib" => kind = rust::Kind::StaticLib,
                        "cdylib" => kind = rust::Kind::CDylib,
                        _ => self.diagnostics.push(Diagnostic {
                            span: Some(span),
                            message: Message::InvalidCrateType(crate_type),
                            severity: Severity::Error,
                        }),
                    }
                },
                "target" => match attribute.data {
                    Data::Str(name) => targets = Some(CrateTargets { names: vec![name], span: attribute.data_span }),
                    // a crate is declared for each of a list of targets
                    Data::List(items) if items.iter().all(|item| matches!(item, Data::Str(_))) => {
                        let names = items.iter().map(Data::coerce_string).collect();
                        targets = Some(CrateTargets { names, span: attribute.data_span });
                    },
                    _ => self.invalid_operand(&attribute.operand(), "str", false),
                },
                "cfg" => {
                    if let Some(options) = self.cfg_options(&attribute.operand()) {
                        cfg.extend(options);
                    }
                },
                name if ATTRIBUTES.contains(&name) => self.running_attribute(&mut running, name, &attribute.operand()),
                "depends" => {
                    let base = self.files[parser.file].path.parent().unwrap().to_path_buf();
                    if let Some(paths) = self.operand_paths(attribute.operand(), &base) {
                        depends.extend(paths);
                    }
                },
                "link-script" => {
                    let base = self.files[parser.file].path.parent().unwrap().to_path_buf();
                    if let Some(path) = self.operand_path(attribute.operand(), &base) {
                        link_script = Some(path);
                    }
                },
                // a crate, or a list of them
                "use" => {
                    let span = attribute.data_span;
                    match attribute.data {
                        Data::List(names) => {
                            uses.extend(names.iter().map(|name| rust::Use { name: name.coerce_string(), span }));
                        },
                        data => uses.push(rust::Use { name: data.coerce_string(), span }),
                    }
                },
                name if let Some(option) = name.strip_prefix("codegen/") && is_codegen_option(option) => {
                    let option = option.to_string();
                    codegen.push(CodegenAttribute { option, operand: attribute.operand() });
                },
                name @ ("install" | "install-dir" | "install-mode") => install_attributes.push((name, attribute.operand())),
                name => self.diagnostics.push(Diagnostic {
                    span: Some(attribute.name_span),
                    message: Message::UnknownCrateAttribute {
                        name: name.into(),
                    },
                    severity: Severity::Error,
                }),
            }
        }
        let mut install = match kind {
            rust::Kind::Bin => Install::executable(),
            rust::Kind::Lib | rust::Kind::StaticLib => Install::library(0o644),
            rust::Kind::CDylib => Install::library(0o755),
            // a dynamic library, which is executable
            rust::Kind::ProcMacro => Install::library(0o755),
        };
        for (name, operand) in install_attributes {
            self.install_attribute(&mut install, name, operand);
        }
        // a procedural macro is loaded by rustc, so it is compiled for the host target, which is otherwise the `target`
        // property in effect at its declaration
        let host = self.property("host-target", span).map(|host| host.data.coerce_string()).unwrap_or_default();
        let targets = targets.unwrap_or_else(|| {
            let property = if kind == rust::Kind::ProcMacro { "host-target" } else { "target" };
            let target = self.property(property, span).map(|target| target.data.coerce_string());
            CrateTargets { names: vec![target.unwrap_or_default()], span }
        });
        let target_span = targets.span;
        let targets = targets.names;
        if kind == rust::Kind::ProcMacro && let Some(target) = targets.iter().find(|target| **target != host) {
            self.diagnostics.push(Diagnostic {
                span: Some(target_span),
                message: Message::ProcMacroTarget { target: target.clone(), host },
                severity: Severity::Error,
            });
            return;
        }
        let codegen = self.codegen_options(span, kind, &codegen, &targets);
        let lints = self.clippy_flags(span);
        let build_dir = self.build_dir();
        // the targets the crate is declared for, whose outputs are declared after its tests are registered
        let mut declared = Vec::new();
        for (i, target) in targets.iter().enumerate() {
            if targets[..i].contains(target) {
                self.diagnostics.push(Diagnostic {
                    span: Some(target_span),
                    message: Message::RepeatedTarget(target.clone()),
                    severity: Severity::Error,
                });
                continue;
            }
            let target = target.clone();
            let test = tested.on(&target);
            // whether the tests can run depends on the target, which may be declared later, so they are registered by
            // `check`
            match self.crates.entry((name.clone(), target.clone())) {
                Entry::Occupied(_) => self.diagnostics.push(Diagnostic {
                    span: Some(span),
                    message: Message::RedeclareCrate {
                        name: name.clone(),
                        target,
                    },
                    severity: Severity::Error,
                }),
                Entry::Vacant(entry) => {
                    let dir = build_dir.join(&target);
                    let c = entry.insert(rust::Crate {
                        name: name.clone(),
                        name_span: span,
                        path: path.clone(),
                        target,
                        target_span,
                        dir,
                        kind,
                        builtin,
                        default,
                        test,
                        doc,
                        uses: uses.clone(),
                        depends: depends.clone(),
                        cfg: cfg.clone(),
                        env: running.env.clone(),
                        pass_env: running.pass_env.clone(),
                        change_detection: running.change_detection.clone(),
                        cached: running.cache.unwrap_or(true),
                        codegen: codegen[i].clone(),
                        lints: lints.clone(),
                        link_script: link_script.clone(),
                        install: install.clone(),
                        worker_tags: running.worker_tags.clone(),
                    });
                    declared.push(c.target.clone());
                    // the test is forward declared, so that it can be referred to, and completed once the target is known
                    if test {
                        let name = c.short_name(&host);
                        let harness = c.test_harness();
                        let depends = c.depends.clone();
                        self.add_crate_test(name, span, harness, depends, tags.clone(), test_run.clone());
                    }
                },
            }
        }
        for target in declared {
            let c = &self.crates[&(name.clone(), target)];
            let output = c.output();
            // a crate is referred to as `<target>/<name>`, as it may be declared for several targets
            let property = format!("{}/{}", c.target, c.name);
            let referent = Referent::Crate { name: c.name.clone(), target: c.target.clone() };
            self.declare_reference(property, span, Item::Crate, Ref { referent, paths: vec![output] });
        }
    }
    /// Returns the configuration options of a `cfg` attribute, which is a name, a list of them, or a dict of names to
    /// values, or lists of them, as `<name>="<value>"`, reporting a value which is none of these.
    fn cfg_options(&mut self, operand: &Operand) -> Option<Vec<String>> {
        let mut options = Vec::new();
        let valid = match &operand.data {
            Data::Str(name) => {
                options.push(name.clone());
                true
            },
            Data::List(names) => names.iter().all(|name| match name {
                Data::Str(name) => {
                    options.push(name.clone());
                    true
                },
                _ => false,
            }),
            Data::Dict(entries) => entries.iter().all(|(name, value)| {
                let values = match value {
                    Data::List(values) => values.clone(),
                    value => vec![value.clone()],
                };
                values.iter().all(|value| match value {
                    Data::Str(_) | Data::Unsigned(_) | Data::Signed(_) | Data::Bool(_) => {
                        // as rustc reads a string literal
                        options.push(format!("{name}={:?}", value.coerce_string()));
                        true
                    },
                    _ => false,
                })
            }),
            _ => false,
        };
        if !valid {
            self.invalid_operand(operand, "str, list of str, or dict", false);
            return None;
        }
        Some(options)
    }
    /// Returns the environment variables of an `env` attribute, which is a dict of their names to their values,
    /// reporting a value which is not, or a name which cannot be an environment variable's.
    pub(super) fn env_variables(&mut self, operand: &Operand) -> Option<Vec<(String, String)>> {
        let Data::Dict(entries) = &operand.data else {
            self.invalid_operand(operand, "dict", false);
            return None;
        };
        let mut variables = Vec::new();
        for (name, value) in entries {
            // a variable is given to rustc by `sh`, so its name is letters, digits and `_`
            if let Some(character) = name.chars().find(|&c| !c.is_ascii_alphanumeric() && c != '_') {
                self.diagnostics.push(Diagnostic {
                    span: Some(operand.span),
                    message: Message::InvalidIdentifier { name: name.clone(), character },
                    severity: Severity::Error,
                });
                return None;
            }
            if matches!(value, Data::List(_) | Data::Dict(_) | Data::None) {
                self.invalid_operand(operand, "dict of str", false);
                return None;
            }
            let value = value.coerce_string();
            variables.push((name.clone(), value));
        }
        Some(variables)
    }
    /// Returns the names of environment variables, from a `pass-env` attribute, which is a name or a list of them, each
    /// of letters, digits and `_`, reporting one which is not.
    pub(super) fn variable_names(&mut self, operand: &Operand) -> Option<Vec<String>> {
        let names: Vec<String> = match &operand.data {
            Data::Str(name) => vec![name.clone()],
            Data::List(names) if names.iter().all(|name| matches!(name, Data::Str(_))) => {
                names.iter().map(Data::coerce_string).collect()
            },
            _ => {
                self.invalid_operand(operand, "str, or list of str", false);
                return None;
            },
        };
        for name in &names {
            let invalid = name.chars().find(|&c| !c.is_ascii_alphanumeric() && c != '_');
            if name.is_empty() || invalid.is_some() {
                self.diagnostics.push(Diagnostic {
                    span: Some(operand.span),
                    message: Message::InvalidIdentifier { name: name.clone(), character: invalid.unwrap_or(' ') },
                    severity: Severity::Error,
                });
                return None;
            }
        }
        Some(names)
    }
    /// Register the test harness of a crate, like a `test` statement which runs it.
    ///
    /// The harness is run directly, until the runner of its target is known.
    fn add_crate_test(
        &mut self,
        name: String,
        span: Span,
        harness: PathBuf,
        depends: Vec<PathBuf>,
        tags: Vec<String>,
        run: TestRun,
    ) {
        let command = vec![Path::new(".").join(&harness).display().to_string()];
        self.add_custom(Custom {
            name,
            name_span: span,
            inputs: vec![harness],
            depends,
            command,
            program: None,
            kind: CustomKind::Test { runner: None, harness: true, on_workers: None, run },
            tags,
            running: Running::default(),
            cache: None,
        });
    }
    /// Complete the tests of the crates with a `test` attribute, which were forward declared, now that the runner of
    /// each target is known. A test is kept if its target allows tests, which a builtin target does by default if the
    /// host can run what is built for it. The others are removed.
    pub(super) fn add_crate_tests(&mut self) {
        let host = self.host();
        // removed once the others are complete, so that the indices of the others stay the same until then
        let mut removed = Vec::new();
        for key in self.crate_keys() {
            let c = &self.crates[&key];
            if !c.test {
                continue;
            }
            let name = c.short_name(&host);
            let span = c.name_span;
            let target_span = c.target_span;
            // the test which was forward declared, unless another had its name
            let declared = self.tests_idx.get(&name).copied().filter(|&index| {
                let test = &self.customs[index];
                (test.name_span.file, test.name_span.offset) == (span.file, span.offset)
            });
            let target = c.target.clone();
            // a test which runs on a worker is run there directly, as the worker runs its target
            let on_workers = self.tests_on_workers(&target).then(|| target.clone());
            let runner = match self.target(&target).map(|target| target.runner) {
                _ if on_workers.is_some() => None,
                Some(runner) if self.can_test(&target) => runner,
                _ if target == host => None,
                // an unknown target is reported anyway
                None => continue,
                Some(_) => {
                    self.diagnostics.push(Diagnostic {
                        span: Some(target_span),
                        message: Message::TestTarget { target: target.clone(), host: host.clone() },
                        severity: Severity::Error,
                    });
                    // nor is its harness built
                    self.crates.get_mut(&key).unwrap().test = false;
                    removed.extend(declared);
                    continue;
                },
            };
            if let Some(declared) = declared {
                let run = self.customs[declared].test_run().cloned().unwrap_or_default();
                self.customs[declared].kind = CustomKind::Test { runner, harness: true, on_workers, run };
            }
        }
        if !removed.is_empty() {
            self.remove_customs(&removed);
        }
    }
}
