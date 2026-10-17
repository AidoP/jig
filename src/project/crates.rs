//! Crates, and the tests of their test harnesses.

use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use super::codegen::{CodegenAttribute, is_codegen_option};
use super::customs::{Custom, CustomKind};
use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Data, Ref, Referent};
use super::{Project, rust};

/// The targets a crate is declared for, from its `target` attribute or the property in effect.
struct CrateTargets {
    names: Vec<String>,
    span: Span,
}

/// The name of the test of a crate, which is the name of the crate, with its target if that is not the host target.
fn crate_test_name(c: &rust::Crate, host: &str) -> String {
    if c.target == host { c.name.clone() } else { format!("{}@{}", c.name, c.target) }
}

impl Project {
    pub(super) fn parse_crate(&mut self, parser: &mut Parser) {
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { name, span, literal }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        // the name is passed to rustc, which rejects `-` even though it is valid in an identifier, while a `/` would be
        // part of the name it is referred to by, `<target>/<name>`
        if let Some((i, character)) = name.char_indices().find(|&(_, c)| c == '-' || c == '/') {
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
        let mut test = false;
        let mut targets = None;
        let mut uses = Vec::new();
        let mut kind = rust::Kind::Lib;
        let mut codegen = Vec::new();
        // the defaults of the install attributes depend on the crate type, so they are applied once it is known
        let mut install_attributes = Vec::new();
        while let Some(attribute) = self.next_attribute(parser, &["use"]) {
            match attribute.name {
                "builtin" => builtin = true,
                "default" => default = true,
                "test" => test = true,
                "crate-type" => {
                    let span = attribute.data_span;
                    let Some(crate_type) = self.str_operand(attribute.operand()) else {
                        continue;
                    };
                    match crate_type.as_str() {
                        "lib" => kind = rust::Kind::Lib,
                        "bin" => kind = rust::Kind::Bin,
                        "proc-macro" => kind = rust::Kind::ProcMacro,
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
                "use" => {
                    let span = attribute.data_span;
                    if let Some(name) = self.str_operand(attribute.operand()) {
                        uses.push(rust::Use { name, span });
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
            rust::Kind::Lib => Install::library(0o644),
            // a dynamic library, which is executable
            rust::Kind::ProcMacro => Install::library(0o755),
        };
        for (name, operand) in install_attributes {
            self.install_attribute(&mut install, name, operand);
        }
        // a procedural macro is loaded by rustc, so it is compiled for the host target, which is otherwise the `target`
        // property in effect at its declaration
        let host = self.property("host-target", span).map(|host| host.data.coerce_string()).unwrap_or_default();
        let targets = match targets {
            Some(targets) => targets,
            None => {
                let property = if kind == rust::Kind::ProcMacro { "host-target" } else { "target" };
                let target = self.property(property, span).map(|target| target.data.coerce_string());
                let target = target.unwrap_or_default();
                CrateTargets { names: vec![target], span }
            },
        };
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
        let codegen = self.codegen_options(span, &codegen);
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
                        uses: uses.clone(),
                        codegen: codegen.clone(),
                        install: install.clone(),
                    });
                    declared.push(c.target.clone());
                    // the test is forward declared, so that it can be referred to, and completed once the target is known
                    if test {
                        let name = crate_test_name(c, &host);
                        let harness = c.test_harness();
                        self.add_crate_test(name, span, harness);
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
    /// Register the test harness of a crate, like a `test` statement which runs it.
    ///
    /// The harness is run directly, until the runner of its target is known.
    fn add_crate_test(&mut self, name: String, span: Span, harness: PathBuf) {
        let command = vec![Path::new(".").join(&harness).display().to_string()];
        self.add_custom(Custom {
            name,
            name_span: span,
            inputs: vec![harness],
            command,
            program: None,
            kind: CustomKind::Test { runner: None },
        });
    }
    /// Complete the tests of the crates with a `test` attribute, which were forward declared, with the runner of their
    /// target. Tests run on the host target, or a custom target which allows tests, and the others are removed.
    pub(super) fn add_crate_tests(&mut self) {
        let host = self.host();
        for key in self.crate_keys() {
            let c = &self.crates[&key];
            if !c.test {
                continue;
            }
            let name = crate_test_name(c, &host);
            let span = c.name_span;
            let target_span = c.target_span;
            // the test which was forward declared, unless another had its name
            let declared = self.customs.iter().position(|test| {
                test.is_test() && test.name == name && (test.name_span.file, test.name_span.offset) == (span.file, span.offset)
            });
            let runner = match self.target(&c.target) {
                Some(target) if target.test => target.runner,
                _ if c.target == host => None,
                // an unknown target is reported anyway
                None => continue,
                Some(_) => {
                    self.diagnostics.push(Diagnostic {
                        span: Some(target_span),
                        message: Message::TestTarget { target: c.target.clone(), host: host.clone() },
                        severity: Severity::Error,
                    });
                    // nor is its harness built
                    self.crates.get_mut(&key).unwrap().test = false;
                    if let Some(declared) = declared {
                        self.customs.remove(declared);
                    }
                    continue;
                },
            };
            if let Some(declared) = declared {
                self.customs[declared].kind = CustomKind::Test { runner };
            }
        }
    }
}
