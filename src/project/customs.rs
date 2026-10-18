//! Custom build commands and tests, which run a command.

use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Data, Operand, Ref, Referent};
use super::{Project, normalize};

/// A command, with its program if that is a path, which it depends on.
#[derive(Clone, Default)]
pub struct Runner {
    pub command: Vec<String>,
    pub program: Option<PathBuf>,
}

/// A custom build command, or a test.
pub struct Custom {
    pub name: String,
    pub name_span: Span,
    pub inputs: Vec<PathBuf>,
    /// What is built before the command or test, but is not an input, so it is not rebuilt when they change.
    pub depends: Vec<PathBuf>,
    /// The program and its arguments, where a program with a `/` is relative to the project root.
    pub command: Vec<String>,
    /// The program, if it is a path, which the outputs depend on.
    pub program: Option<PathBuf>,
    pub kind: CustomKind,
}

/// Whether a [`Custom`] is a build command or a test, with what only that kind has.
pub enum CustomKind {
    /// A build command, which ninja runs to build its outputs.
    Command {
        outputs: Vec<PathBuf>,
        /// Whether the outputs are built by `ninja` without targets.
        default: bool,
        /// Where `jig install` installs the outputs, if it does.
        install: Install,
    },
    /// A test, which has no outputs and is run by `jig test` rather than ninja.
    Test {
        /// The index of the runner of the test's target, which runs its command, as with the test harness of a crate.
        runner: Option<usize>,
        /// Whether the command is the test harness of a crate, built by libtest.
        harness: bool,
    },
}

impl Custom {
    pub fn is_test(&self) -> bool {
        matches!(self.kind, CustomKind::Test { .. })
    }
    /// The index of the runner of a test, if it has one.
    fn runner(&self) -> Option<usize> {
        match self.kind {
            CustomKind::Test { runner, .. } => runner,
            CustomKind::Command { .. } => None,
        }
    }
    /// The command which is run, by the runner, if there is one.
    pub fn command(&self, runners: &[Runner]) -> Vec<String> {
        let Some(runner) = self.runner() else {
            return self.command.clone();
        };
        let mut command = runners[runner].command.clone();
        command.extend(self.command.iter().cloned());
        command
    }
    /// Whether the test is the test harness of a crate which is run directly, so that it takes libtest's arguments,
    /// while a runner takes the harness as its last argument.
    pub fn is_libtest(&self) -> bool {
        matches!(self.kind, CustomKind::Test { runner: None, harness: true })
    }
    /// The inputs, which include the program of the runner, if there is one.
    pub fn inputs(&self, runners: &[Runner]) -> Vec<PathBuf> {
        let mut inputs = self.inputs.clone();
        if let Some(runner) = self.runner() && let Some(program) = &runners[runner].program {
            inputs.push(program.clone());
        }
        inputs
    }
}

impl Project {
    /// Take a custom build command, with `input`, `output`, `depends`, `command`, `default` and install attributes, or
    /// a test, which has `input`, `depends` and `command` attributes.
    ///
    /// The output of a command is named after it in the build directory, unless an `output` attribute replaces it.
    /// While its attributes are evaluated, the paths of the inputs and outputs are the values of the `input` and
    /// `output` properties, as lists of references, so that they are not resolved again.
    pub(super) fn parse_custom(&mut self, parser: &mut Parser, test: bool) {
        let item = if test { Item::Test } else { Item::CustomCommand };
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { name, span, .. }) = self.parse_name(parser) else {
            parser.skip_statement();
            return;
        };
        if !self.parse_line_end(parser) {
            parser.skip_statement();
        }
        let mut inputs = Vec::new();
        let mut depends = Vec::new();
        let mut outputs = Vec::new();
        if !test {
            outputs.push(normalize(&self.build_dir().join(&name)));
            self.set_local("output", path_refs(&outputs), parser.file);
        }
        let mut command: Option<Runner> = None;
        let mut default = false;
        let mut install = Install::data();
        // an attribute which failed has been reported, so it is not also reported as missing
        let mut failed = false;
        while parser.indent().is_some() {
            let Some((attribute, attribute_span)) = self.parse_ident(parser) else {
                parser.skip_line();
                continue;
            };
            match attribute {
                "input" | "output" if attribute == "input" || !test => {
                    let output = attribute == "output";
                    let Some(new) = self.parse_custom_paths(parser, output) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    // the inputs are added to, while the outputs, which have a default, are replaced
                    let paths = if output { &mut outputs } else { &mut inputs };
                    if output {
                        paths.clear();
                    }
                    paths.extend(new);
                    self.set_local(attribute, path_refs(paths), parser.file);
                },
                "depends" => {
                    let Some(new) = self.parse_custom_paths(parser, false) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    depends.extend(new);
                },
                "default" | "install" | "install-dir" | "install-mode" if !test => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    if attribute == "default" {
                        if let Some(value) = self.flag(operand) {
                            default = value;
                        }
                    } else {
                        self.install_attribute(&mut install, attribute, operand);
                    }
                },
                "command" => {
                    let Some(arguments) = self.parse_command(parser) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    if command.replace(arguments).is_some() {
                        self.diagnostics.push(Diagnostic {
                            span: Some(attribute_span),
                            message: Message::RepeatedAttribute(attribute.into()),
                            severity: Severity::Error,
                        });
                    }
                },
                name => {
                    parser.skip_line();
                    self.diagnostics.push(Diagnostic {
                        span: Some(attribute_span),
                        message: Message::UnknownAttribute { keyword: item.keyword(), name: name.into() },
                        severity: Severity::Error,
                    });
                    continue;
                },
            }
            if !self.parse_line_end(parser) {
                parser.skip_line();
            }
        }
        for local in ["input", "output"] {
            self.files[parser.file].locals.remove(local);
        }
        let missing = if outputs.is_empty() && !test {
            Some("output")
        } else if command.is_none() {
            Some("command")
        } else {
            None
        };
        if let Some(attribute) = missing {
            if failed {
                return;
            }
            self.diagnostics.push(Diagnostic {
                span: Some(span),
                message: Message::MissingAttribute(attribute),
                severity: Severity::Error,
            });
            return;
        }
        let runner = command.unwrap_or_default();
        // the outputs are declared as a property named after the command, once it is known to be unique
        let reference = Ref { referent: Referent::Custom(name.clone()), paths: outputs.clone() };
        let kind = if test { CustomKind::Test { runner: None, harness: false } } else { CustomKind::Command { outputs, default, install } };
        let custom = Custom {
            name,
            name_span: span,
            inputs,
            depends,
            command: runner.command,
            program: runner.program,
            kind,
        };
        let declared = custom.name.clone();
        if self.add_custom(custom) && !test {
            self.declare_reference(declared, span, Item::CustomCommand, reference);
        }
    }
    /// Returns a command whose program, as in `run`, is a path relative to a file if it has a `/`, and which it then
    /// depends on, while otherwise it is searched for in `PATH`.
    pub(super) fn resolve_command(&self, mut command: Vec<String>, file: usize) -> Runner {
        let mut program = None;
        if command[0].contains('/') {
            let mut path = normalize(&self.file(file).relative_path(&command[0]));
            program = Some(path.clone());
            if path.is_relative() {
                path = Path::new(".").join(path);
            }
            command[0] = path.display().to_string();
        }
        Runner { command, program }
    }
    /// Add a custom build command or a test, unless one of the same kind has the same name.
    pub(super) fn add_custom(&mut self, custom: Custom) -> bool {
        if let Some(previous) = self.custom(&custom.name, custom.is_test()) {
            let previous = previous.name_span;
            self.diagnostics.push(Diagnostic {
                span: Some(custom.name_span),
                message: Message::RedeclareCustom {
                    item: if custom.is_test() { Item::Test } else { Item::CustomCommand },
                    name: custom.name,
                    previous,
                },
                severity: Severity::Error,
            });
            return false;
        }
        let index = if custom.is_test() { &mut self.tests_idx } else { &mut self.commands_idx };
        index.insert(custom.name.clone(), self.customs.len());
        self.customs.push(custom);
        true
    }
    /// Returns the custom command, or the test if `test`, with a name.
    pub(super) fn custom(&self, name: &str, test: bool) -> Option<&Custom> {
        let index = if test { &self.tests_idx } else { &self.commands_idx };
        index.get(name).map(|&index| &self.customs[index])
    }
    /// Remove custom commands and tests, given by their indices in `customs`.
    pub(super) fn remove_customs(&mut self, removed: &[usize]) {
        let removed: HashSet<_> = removed.iter().copied().collect();
        let mut index = 0;
        self.customs.retain(|_| {
            index += 1;
            !removed.contains(&(index - 1))
        });
        self.commands_idx.clear();
        self.tests_idx.clear();
        for (index, custom) in self.customs.iter().enumerate() {
            let indices = if custom.is_test() { &mut self.tests_idx } else { &mut self.commands_idx };
            indices.insert(custom.name.clone(), index);
        }
    }
    /// Take the paths of an `input` or `output` attribute, which are expressions separated by spaces, each a path or a
    /// list of them.
    ///
    /// An input is relative to the file, and an output to the build directory, while the paths of a reference are
    /// already resolved.
    fn parse_custom_paths(&mut self, parser: &mut Parser, output: bool) -> Option<Vec<PathBuf>> {
        let mut paths = Vec::new();
        let build_dir = self.build_dir();
        loop {
            if !self.parse_space(parser) {
                return None;
            }
            let operand = self.parse_expression(parser)?;
            let base = if output { build_dir.clone() } else { self.file(parser.file).path.parent().unwrap().into() };
            paths.extend(self.operand_paths(operand, &base)?);
            if parser.peek() != Some(' ') {
                return Some(paths);
            }
        }
    }
    /// Returns the paths of a value, which is a path relative to `base`, a reference, whose paths are already
    /// resolved, or a list of them.
    pub(super) fn operand_paths(&mut self, operand: Operand, base: &Path) -> Option<Vec<PathBuf>> {
        let items = match operand.data {
            Data::List(items) => items,
            Data::Ref(_) => vec![operand.data],
            _ => vec![Data::Str(self.str_operand(operand)?)],
        };
        let mut paths = Vec::new();
        for item in items {
            match item {
                Data::Ref(reference) => paths.extend(reference.paths),
                item => paths.push(normalize(&base.join(item.coerce_string()))),
            }
        }
        Some(paths)
    }
    /// Returns the path of a value which is one file, a path relative to `base` or a reference to a crate or custom
    /// command with one output.
    pub(super) fn operand_path(&mut self, operand: Operand, base: &Path) -> Option<PathBuf> {
        match &operand.data {
            Data::Ref(reference) if reference.paths.len() == 1 => Some(reference.paths[0].clone()),
            Data::Ref(_) | Data::List(_) => {
                self.invalid_operand(&operand, "str, or a reference to one file", false);
                None
            },
            _ => Some(normalize(&base.join(self.str_operand(operand)?))),
        }
    }
    /// Take the arguments of a `command` attribute, which are expressions separated by spaces, returning the command
    /// with its program resolved as by [`Self::resolve_command`], unless it is a reference, whose path is resolved.
    ///
    /// Any value is an argument, converted to a string as in a string template, while each item of a list is a separate
    /// argument.
    fn parse_command(&mut self, parser: &mut Parser) -> Option<Runner> {
        let mut arguments = Vec::new();
        let mut program = None;
        while parser.peek() == Some(' ') || arguments.is_empty() {
            if !self.parse_space(parser) {
                return None;
            }
            let operand = self.parse_expression(parser)?;
            if arguments.is_empty() && let Some(reference) = leading_reference(&operand.data) {
                program = reference.paths.first().cloned();
            }
            for argument in operand.data.spread() {
                // an argument is written to `build.ninja` on a single line
                if argument.contains('\n') {
                    self.diagnostics.push(Diagnostic {
                        span: Some(operand.span),
                        message: Message::NewlineInArgument,
                        severity: Severity::Error,
                    });
                    return None;
                }
                arguments.push(argument);
            }
        }
        let Some(program) = program else {
            return Some(self.resolve_command(arguments, parser.file));
        };
        // the program is run from the project root, as a path rather than from `PATH`
        arguments[0] = Path::new(".").join(&program).display().to_string();
        Some(Runner { command: arguments, program: Some(program) })
    }
    /// Report the outputs of custom commands which are, or are within or contain, the output of a crate or of another
    /// custom command, which ninja would build twice, or in place of a directory.
    ///
    /// The outputs are kept in order of their paths as well, in which the paths within a path come directly after it, so
    /// that each is compared with its ancestors and the paths after it, rather than with every other output.
    pub(super) fn check_outputs(&mut self) {
        let mut owned = Vec::new();
        for key in self.crate_keys() {
            let c = &self.crates[&key];
            let name = format!("{}/{}", c.target, c.name);
            let mut paths = vec![c.output()];
            if c.test {
                paths.push(c.test_harness());
            }
            for path in paths {
                owned.push(Output { path, item: Item::Crate, name: name.clone(), span: c.name_span });
            }
        }
        // the first output with each path, by index in `owned`
        let mut sorted = BTreeMap::new();
        for (index, output) in owned.iter().enumerate() {
            sorted.entry(output.path.clone()).or_insert(index);
        }
        for custom in &self.customs {
            let CustomKind::Command { outputs, .. } = &custom.kind else {
                continue;
            };
            for path in outputs {
                // of the outputs it overlaps, the one which was owned first is reported
                let within = path.ancestors().filter_map(|ancestor| sorted.get(ancestor));
                let containing = sorted.range::<Path, _>((Bound::Excluded(path.as_path()), Bound::Unbounded))
                    .take_while(|(other, _)| other.starts_with(path))
                    .map(|(_, index)| index);
                if let Some(&index) = within.chain(containing).min() {
                    let other = &owned[index];
                    self.diagnostics.push(Diagnostic {
                        span: Some(custom.name_span),
                        message: Message::OverlappingOutput {
                            path: path.display().to_string(),
                            other: other.path.display().to_string(),
                            item: other.item,
                            name: other.name.clone(),
                            declared: other.span,
                        },
                        severity: Severity::Error,
                    });
                    continue;
                }
                sorted.insert(path.clone(), owned.len());
                owned.push(Output { path: path.clone(), item: Item::CustomCommand, name: custom.name.clone(), span: custom.name_span });
            }
        }
    }
}

/// An output of a crate or custom command, which no other may overlap.
struct Output {
    path: PathBuf,
    item: Item,
    /// The name of the crate, with its target, or the custom command.
    name: String,
    span: Span,
}

/// Returns paths as a list of references to them, which are not resolved again where they are used.
fn path_refs(paths: &[PathBuf]) -> Data {
    let mut items = Vec::new();
    for path in paths {
        items.push(Data::Ref(Ref { referent: Referent::Paths, paths: vec![path.clone()] }));
    }
    Data::List(items)
}

/// Returns the reference which a value starts with, which is the program if it is the first argument of a command.
pub(super) fn leading_reference(data: &Data) -> Option<&Ref> {
    match data {
        Data::Ref(reference) => Some(reference),
        Data::List(items) => leading_reference(items.first()?),
        _ => None,
    }
}
