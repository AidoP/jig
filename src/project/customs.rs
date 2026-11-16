//! Custom build commands and tests, which run a command.

use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use super::diagnostic::{Diagnostic, Item, Message, Severity};
use super::install::Install;
use super::statements::Name;
use super::syntax::{Parser, Span};
use super::value::{Data, Operand, Ref, Referent};
use super::{Project, has_line_break, normalize};

/// A command, and its program if the program is a path, in which case the command depends on it.
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
    /// What is built before the command or test runs, without being an input: a change to it does not run them again.
    pub depends: Vec<PathBuf>,
    /// The program and its arguments. A program with a `/` is a path relative to the project root.
    pub command: Vec<String>,
    /// The program, if it is a path, which the outputs depend on.
    pub program: Option<PathBuf>,
    pub kind: CustomKind,
    /// The tags of a test, by which `jig test` chooses it. A test with tags only runs without being asked for if one
    /// of its tags is in `test-tags`.
    pub tags: Vec<String>,
    /// How rig detects that the command's files have changed, if this is given, in place of `change-detection`.
    pub change_detection: Option<String>,
    /// Whether the command has the `cache` attribute.
    pub cached: bool,
    /// The identity of the command in rig's cache, if it has the `cache` attribute and rig caches. It covers the
    /// toolchain, and the program if it is found in `PATH`. [`Project::identify_cached_customs`] sets it once the
    /// configuration is evaluated.
    pub cache: Option<String>,
    /// The environment variables the command reads from those it inherits, whose values are its inputs.
    pub pass_env: Vec<String>,
}

/// Whether a [`Custom`] is a build command or a test, with what only that kind has.
pub enum CustomKind {
    /// A build command, which rig runs to build its outputs.
    Command {
        outputs: Vec<PathBuf>,
        /// Whether the outputs are built by `jig build` without targets.
        default: bool,
        /// Where `jig install` installs the outputs, if it does.
        install: Install,
    },
    /// A test, which has no outputs and is run by `jig test` rather than by rig.
    Test {
        /// The index of the runner of the test's target, which runs the test's command, as for a crate's test harness.
        runner: Option<usize>,
        /// Whether the command is the test harness of a crate, built by libtest.
        harness: bool,
        /// For a crate's test harness which runs on a worker, rather than here: its target, which the worker must be
        /// able to run.
        on_workers: Option<String>,
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
    /// Whether the test is a crate's test harness which is run directly, and so takes libtest's arguments. With a
    /// runner, the runner is what is run, and the harness is its last argument.
    pub fn is_libtest(&self) -> bool {
        matches!(self.kind, CustomKind::Test { runner: None, harness: true, .. })
    }
    /// The target of a test which runs on a worker rather than here, if it does.
    pub fn on_workers(&self) -> Option<&str> {
        match &self.kind {
            CustomKind::Test { on_workers, .. } => on_workers.as_deref(),
            CustomKind::Command { .. } => None,
        }
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
    /// Take a `custom` statement, with `input`, `output`, `depends`, `command`, `default` and install attributes, or a
    /// `test` statement, with `input`, `depends`, `command` and `tag` attributes.
    ///
    /// A command's output is a file named after it in the build directory, unless an `output` attribute replaces it.
    /// While the attributes are evaluated, the `input` and `output` properties hold the paths of the inputs and
    /// outputs as lists of references, so that using them does not resolve the paths a second time.
    pub(super) fn parse_custom(&mut self, parser: &mut Parser, test: bool) {
        let item = if test { Item::Test } else { Item::CustomCommand };
        if !self.parse_space(parser) {
            parser.skip_statement();
            return;
        }
        let Some(Name { text: name, span, .. }) = self.parse_name(parser) else {
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
        let mut tags = Vec::new();
        let mut change_detection = None;
        let mut cache = false;
        let mut pass_env = Vec::new();
        let mut install = Install::data();
        // an attribute which failed has already been reported, so it is not reported as missing too
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
                    // each `input` adds to the inputs, while an `output` replaces the outputs, which start as the
                    // default
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
                "change-detection" if !test => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    let value = match operand.data {
                        Data::Str(ref value) if value == "timestamp" || value == "hash" => Some(value.clone()),
                        _ => None,
                    };
                    if value.is_none() {
                        self.invalid_operand(&operand, "`'timestamp'` or `'hash'`", false);
                    }
                    change_detection = value.or(change_detection);
                },
                "pass-env" if !test => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    pass_env.extend(self.variable_names(&operand).unwrap_or_default());
                },
                "tag" if test => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    tags.extend(self.tags(&operand).unwrap_or_default());
                },
                "default" | "cache" | "install" | "install-dir" | "install-mode" if !test => {
                    let Some(operand) = self.parse_attribute_value(parser, attribute_span) else {
                        parser.skip_line();
                        failed = true;
                        continue;
                    };
                    if attribute == "default" || attribute == "cache" {
                        if let Some(value) = self.flag(&operand) {
                            *if attribute == "default" { &mut default } else { &mut cache } = value;
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
        // declare the outputs as a property named after the command, now that the name is known to be unique
        let reference = Ref { referent: Referent::Custom(name.clone()), paths: outputs.clone() };
        let kind = if test {
            CustomKind::Test { runner: None, harness: false, on_workers: None }
        } else {
            CustomKind::Command { outputs, default, install }
        };
        let custom = Custom {
            name,
            name_span: span,
            inputs,
            depends,
            command: runner.command,
            program: runner.program,
            kind,
            tags,
            change_detection,
            cached: cache,
            cache: None,
            pass_env,
        };
        let declared = custom.name.clone();
        if self.add_custom(custom) && !test {
            self.declare_reference(declared, span, Item::CustomCommand, reference);
        }
    }
    /// Set the identity in rig's cache of each command with the `cache` attribute, if rig caches, once the
    /// configuration is evaluated and the project's `cache` identifies its toolchain. [`Self::custom_identity`]
    /// computes each.
    pub fn identify_cached_customs(&mut self) {
        for index in 0..self.customs.len() {
            if self.customs[index].cached {
                let runner = Runner { command: self.customs[index].command.clone(), program: self.customs[index].program.clone() };
                self.customs[index].cache = self.custom_identity(&runner);
            }
        }
    }
    /// Returns the identity in rig's cache of a command with the `cache` attribute, if rig caches. It is a hash of
    /// the identity of the toolchain the command runs with, and of the contents of its program if that is found in
    /// `PATH`. The configuration is then made to depend on the program, so that the identity is found again when
    /// the program changes. A program given as a path is not hashed, as it is an input of the command. A program
    /// which is not found is identified as missing.
    fn custom_identity(&mut self, runner: &Runner) -> Option<String> {
        let toolchain = self.cache.clone()?;
        let mut key = rig::cache::Key::new("jig custom");
        key.part(toolchain.as_bytes());
        if runner.program.is_none() && let Some(name) = runner.command.first() {
            let program = self.which(name).ok();
            let hash = program.as_deref().and_then(|program| rig::blake3::hash_file(program).ok());
            key.hash(hash.as_ref());
            if let Some(program) = program.filter(|_| hash.is_some()) {
                self.depend(program);
            }
        }
        Some(rig::cache::hex(&key.finish()))
    }
    /// Returns the tags of a `tag` attribute, which is a tag or a list of tags. A tag which is empty or has
    /// whitespace is reported.
    pub(super) fn tags(&mut self, operand: &Operand) -> Option<Vec<String>> {
        let tags = match &operand.data {
            Data::Str(tag) => vec![tag.clone()],
            Data::List(tags) if tags.iter().all(|tag| matches!(tag, Data::Str(_))) => {
                tags.iter().map(Data::coerce_string).collect()
            },
            _ => Vec::new(),
        };
        if tags.is_empty() || tags.iter().any(|tag| tag.is_empty() || tag.contains(char::is_whitespace)) {
            self.invalid_operand(operand, "tag, or list of tags, which are not empty and have no whitespace", false);
            return None;
        }
        Some(tags)
    }
    /// Returns a command with its program resolved, as for `run`. A program with a `/` is a path relative to the
    /// file, and the command depends on it. Any other is a name to search for in `PATH`.
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
    /// Take the paths of an `input` or `output` attribute: expressions separated by spaces, each a path or a list of
    /// paths.
    ///
    /// An input is relative to the file, and an output to the build directory. The paths of a reference are already
    /// resolved.
    pub(super) fn parse_custom_paths(&mut self, parser: &mut Parser, output: bool) -> Option<Vec<PathBuf>> {
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
    /// Returns the paths of a value: a path relative to `base`, a reference, whose paths are already resolved, or a
    /// list of either.
    pub(super) fn operand_paths(&mut self, operand: Operand, base: &Path) -> Option<Vec<PathBuf>> {
        let operand = self.computed_reference(operand);
        let span = operand.span;
        let items = match operand.data {
            Data::List(items) => items,
            Data::Ref(_) => vec![operand.data],
            _ => vec![Data::Str(self.str_operand(operand)?)],
        };
        let mut paths = Vec::new();
        for item in items {
            match item {
                Data::Ref(reference) => paths.extend(reference.paths),
                item => paths.push(self.relative_path(base, &item.coerce_string(), span)?),
            }
        }
        for path in &paths {
            self.check_path(path, span)?;
        }
        Some(paths)
    }
    /// Returns a path relative to `base`. An empty path is reported, as it would be `base` itself.
    fn relative_path(&mut self, base: &Path, path: &str, span: Span) -> Option<PathBuf> {
        if path.is_empty() {
            self.diagnostics.push(Diagnostic { span: Some(span), message: Message::EmptyPath, severity: Severity::Error });
            return None;
        }
        Some(normalize(&base.join(path)))
    }
    /// Returns the path, or reports it and returns [`None`] if it has a newline, which a depfile cannot hold.
    pub(super) fn check_path(&mut self, path: &Path, span: Span) -> Option<()> {
        if !has_line_break(&path.to_string_lossy()) {
            return Some(());
        }
        self.diagnostics.push(Diagnostic {
            span: Some(span),
            message: Message::NewlineInPath(path.into()),
            severity: Severity::Error,
        });
        None
    }
    /// Returns the path of a value which stands for one file: a path relative to `base`, or a reference to a crate or
    /// custom command with exactly one output.
    pub(super) fn operand_path(&mut self, operand: Operand, base: &Path) -> Option<PathBuf> {
        let operand = self.computed_reference(operand);
        let span = operand.span;
        let path = match &operand.data {
            Data::Ref(reference) if reference.paths.len() == 1 => reference.paths[0].clone(),
            Data::Ref(_) | Data::List(_) => {
                self.invalid_operand(&operand, "str, or a reference to one file", false);
                return None;
            },
            _ => {
                let path = self.str_operand(operand)?;
                self.relative_path(base, &path, span)?
            },
        };
        self.check_path(&path, span)?;
        Some(path)
    }
    /// Take the arguments of a `command` attribute, which are expressions separated by spaces, and return the
    /// command. Its program is resolved as by [`Self::resolve_command`], unless it is a reference, which already
    /// has a resolved path.
    ///
    /// Any value is an argument, converted to a string as in a string template. Each item of a list is a separate
    /// argument.
    fn parse_command(&mut self, parser: &mut Parser) -> Option<Runner> {
        let mut operands = Vec::new();
        while parser.peek() == Some(' ') || operands.iter().all(|operand: &Operand| operand.data.spread().is_empty()) {
            if !self.parse_space(parser) {
                return None;
            }
            operands.push(self.parse_expression(parser)?);
        }
        self.operands_command(&operands, parser.file)
    }
    /// Returns the command whose arguments are the values of the operands, with each item of a list a separate
    /// argument. The program is resolved as by [`Self::resolve_command`]. If it is a reference, such as to a crate
    /// or a custom command, the referenced path is run, and the command depends on it.
    ///
    /// The operands must give at least one argument, and no argument may have a newline.
    pub(super) fn operands_command(&mut self, operands: &[Operand], file: usize) -> Option<Runner> {
        let operands: Vec<Operand> = operands
            .iter()
            .map(|operand| {
                let Operand { data, span, coerce, origin } = operand;
                self.computed_reference(Operand { data: data.clone(), span: *span, coerce: *coerce, origin: *origin })
            })
            .collect();
        let mut arguments = Vec::new();
        let mut program = None;
        for operand in &operands {
            if arguments.is_empty() && let Some(reference) = leading_reference(&operand.data) {
                program = reference.paths.first().cloned();
            }
            for argument in operand.data.spread() {
                arguments.push(argument);
            }
        }
        if arguments.is_empty() {
            let operand = operands.last()?;
            self.invalid_operand(operand, "a program, or a list of it and its arguments", false);
            return None;
        }
        let Some(program) = program else {
            return Some(self.resolve_command(arguments, file));
        };
        // a referenced program is given as a path from the project root, so that it is not searched for in `PATH`
        arguments[0] = Path::new(".").join(&program).display().to_string();
        Some(Runner { command: arguments, program: Some(program) })
    }
    /// Report each output of a custom command or generated file which overlaps another output: it is the same as,
    /// inside, or contains the output of a crate, another custom command or another generated file. rig would build
    /// such a path twice, or as both a file and a directory, or would overwrite a file written while configuring.
    ///
    /// The outputs are also kept sorted by path. Paths inside a path sort directly after it, so each output is only
    /// compared with its ancestors and with the paths just after it, not with every other output.
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
        // the first output with each path, as its index in `owned`
        let mut sorted = BTreeMap::new();
        for (index, output) in owned.iter().enumerate() {
            sorted.entry(output.path.clone()).or_insert(index);
        }
        let mut checked = Vec::new();
        for custom in &self.customs {
            if let CustomKind::Command { outputs, .. } = &custom.kind {
                for path in outputs {
                    checked.push(Output { path: path.clone(), item: Item::CustomCommand, name: custom.name.clone(), span: custom.name_span });
                }
            }
        }
        for file in &self.generated {
            checked.push(Output { path: file.output.clone(), item: Item::File, name: file.name.clone(), span: file.name_span });
        }
        for output in checked {
            let path = &output.path;
            // if it overlaps several outputs, report the one which was declared first
            let within = path.ancestors().filter_map(|ancestor| sorted.get(ancestor));
            let containing = sorted.range::<Path, _>((Bound::Excluded(path.as_path()), Bound::Unbounded))
                .take_while(|(other, _)| other.starts_with(path))
                .map(|(_, index)| index);
            if let Some(&index) = within.chain(containing).min() {
                let other = &owned[index];
                self.diagnostics.push(Diagnostic {
                    span: Some(output.span),
                    message: Message::OverlappingOutput {
                        owner: output.item,
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
            owned.push(output);
        }
    }
}

/// An output of a crate, custom command or generated file, which no other may overlap.
struct Output {
    path: PathBuf,
    item: Item,
    /// The name of the crate, with its target, or of the custom command or file.
    name: String,
    span: Span,
}

/// Returns paths as a list of references to them, which are not resolved a second time where they are used.
fn path_refs(paths: &[PathBuf]) -> Data {
    let mut items = Vec::new();
    for path in paths {
        items.push(Data::Ref(Ref { referent: Referent::Paths, paths: vec![path.clone()] }));
    }
    Data::List(items)
}

/// Whether a value can be the arguments of a command. `none` and a dict cannot, nor a list which holds either.
pub(super) fn is_argument(data: &Data) -> bool {
    match data {
        Data::None | Data::Dict(_) => false,
        Data::List(items) => items.iter().all(is_argument),
        _ => true,
    }
}

pub(super) fn leading_reference(data: &Data) -> Option<&Ref> {
    match data {
        Data::Ref(reference) => Some(reference),
        Data::List(items) => leading_reference(items.first()?),
        _ => None,
    }
}
