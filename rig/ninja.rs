//! Building a [`Graph`] from ninja's rules, edges and variables, as a ninja file describes them.
//!
//! A [`Manifest`] takes the statements of a ninja file and evaluates them as ninja does: the variables of an edge are
//! evaluated where it is defined, then the bindings of its rule are evaluated for it, looking up a variable in the
//! edge, then the rule, then the variables defined before the edge. `$in` and `$out` are its explicit inputs and
//! outputs, quoted for the shell as ninja quotes them, except in the depfile. Each edge then has its command,
//! description and depfile.
//!
//! [`read`] reads a ninja file into a [`Manifest`]. Only what rig supports is read: `include`, `subninja`, `pool`
//! statements, `deps = msvc`, response files and dyndep files are errors. Paths are taken as they are written, rather
//! than with `.` and `..` resolved as ninja does.

use std::collections::HashMap;

use crate::graph::{Action, Edge, EdgeKind, Graph, NodeId, Pool};
use crate::Error;

/// The bindings a rule can have, as ninja allows.
const RULE_BINDINGS: &[&str] = &[
    "command",
    "description",
    "depfile",
    "deps",
    "pool",
    "generator",
    "restat",
    "rspfile",
    "rspfile_content",
    "msvc_deps_prefix",
    "dyndep",
];

/// How deeply the bindings of a rule may refer to each other, beyond which they are taken to refer to themselves.
const MAX_DEPTH: usize = 64;

/// A value of a ninja file, as text and the variables it refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Text(String),
    Variable(String),
}

/// Parse a value, which is a single line, where `$` escapes the next character or refers to a variable.
fn parse(value: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut chars = value.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            text.push(c);
            continue;
        }
        let variable = match chars.next() {
            Some((_, c @ ('$' | ' ' | ':'))) => {
                text.push(c);
                continue;
            },
            Some((start, '{')) => {
                let Some(end) = value[start..].find('}') else {
                    return Err(format!("unterminated `${{` in {value:?}"));
                };
                let name = &value[start + 1..start + end];
                if name.is_empty() || !name.chars().all(|c| is_simple_char(c) || c == '.') {
                    return Err(format!("invalid variable name {name:?} in {value:?}"));
                }
                while chars.next_if(|&(i, _)| i <= start + end).is_some() {}
                name.to_string()
            },
            Some((start, c)) if is_simple_char(c) => {
                let mut end = start + c.len_utf8();
                while let Some((i, c)) = chars.next_if(|&(_, c)| is_simple_char(c)) {
                    end = i + c.len_utf8();
                }
                value[start..end].to_string()
            },
            _ => return Err(format!("bad `$`-escape in {value:?}")),
        };
        if !text.is_empty() {
            tokens.push(Token::Text(std::mem::take(&mut text)));
        }
        tokens.push(Token::Variable(variable));
    }
    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }
    Ok(tokens)
}

/// Whether a character can be part of a variable's name which is not in `${}`.
fn is_simple_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Evaluate a parsed value, looking up each variable it refers to.
fn evaluate(tokens: &[Token], lookup: &mut dyn FnMut(&str) -> Result<String, String>) -> Result<String, String> {
    let mut value = String::new();
    for token in tokens {
        match token {
            Token::Text(text) => value.push_str(text),
            Token::Variable(name) => value.push_str(&lookup(name)?),
        }
    }
    Ok(value)
}

/// Quote a path for the shell, as ninja does in `$in` and `$out`, unless every character is known to be safe.
fn shell_escape(path: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | '/');
    if path.chars().all(safe) {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', "'\\''"))
}

/// The statements of a ninja file, which build a [`Graph`].
#[derive(Default)]
pub struct Manifest {
    graph: Graph,
    /// The variables defined so far at the top level, evaluated.
    variables: HashMap<String, String>,
    /// The bindings of each rule, parsed but not evaluated.
    rules: HashMap<String, HashMap<String, Vec<Token>>>,
}

/// A `build` statement, whose paths are evaluated, and whose bindings are as they are written.
#[derive(Default)]
pub struct Build<'a> {
    pub rule: &'a str,
    pub outputs: &'a [&'a str],
    pub implicit_outputs: &'a [&'a str],
    pub inputs: &'a [&'a str],
    pub implicit: &'a [&'a str],
    pub order_only: &'a [&'a str],
    pub bindings: &'a [(&'a str, &'a str)],
}

impl Manifest {
    #[must_use]
    pub fn new() -> Self {
        <Self as Default>::default()
    }
    /// Define a variable at the top level, whose value is as it is written, which is evaluated now.
    ///
    /// # Errors
    ///
    /// If the value is not valid.
    pub fn variable(&mut self, name: &str, value: &str) -> Result<(), Error> {
        let tokens = parse(value).map_err(message)?;
        let value = evaluate(&tokens, &mut |name| Ok(self.variables.get(name).cloned().unwrap_or_default()))
            .map_err(message)?;
        self.variables.insert(name.into(), value);
        Ok(())
    }
    /// Define a rule, whose bindings are as they are written, which are evaluated for each edge.
    ///
    /// # Errors
    ///
    /// If the rule is defined again, has no command or a binding which a rule cannot have, or a value is not valid.
    pub fn rule(&mut self, name: &str, bindings: &[(&str, &str)]) -> Result<(), Error> {
        if name == "phony" || self.rules.contains_key(name) {
            return Err(message(format!("duplicate rule {name:?}")));
        }
        let mut parsed = HashMap::new();
        for &(binding, value) in bindings {
            if !RULE_BINDINGS.contains(&binding) {
                return Err(message(format!("unexpected variable {binding:?} in rule {name:?}")));
            }
            parsed.insert(binding.to_string(), parse(value).map_err(message)?);
        }
        if !parsed.contains_key("command") {
            return Err(message(format!("rule {name:?} has no command")));
        }
        self.rules.insert(name.into(), parsed);
        Ok(())
    }
    /// Add an edge, whose bindings are evaluated with the variables defined so far.
    ///
    /// # Errors
    ///
    /// If its rule is not defined, an output is produced by another edge, a value is not valid, or it uses what rig
    /// does not support.
    pub fn build(&mut self, build: &Build) -> Result<(), Error> {
        let mut edge_variables = HashMap::new();
        for &(name, value) in build.bindings {
            let tokens = parse(value).map_err(message)?;
            let value = evaluate(&tokens, &mut |name| Ok(self.variables.get(name).cloned().unwrap_or_default()))
                .map_err(message)?;
            edge_variables.insert(name.to_string(), value);
        }
        let mut nodes = |paths: &[&str]| paths.iter().map(|path| self.graph.file(path)).collect::<Vec<NodeId>>();
        let mut edge = Edge {
            outputs: nodes(build.outputs),
            implicit_outputs: nodes(build.implicit_outputs),
            inputs: nodes(build.inputs),
            implicit: nodes(build.implicit),
            order_only: nodes(build.order_only),
            ..Edge::default()
        };
        if build.rule != "phony" {
            let Some(rule) = self.rules.get(build.rule) else {
                return Err(message(format!("unknown rule {:?}", build.rule)));
            };
            let mut scope = Scope { build, edge: &edge_variables, rule, variables: &self.variables, escape: true };
            let command = scope.lookup("command", 0).map_err(message)?;
            edge.description = scope.lookup("description", 0).map_err(message)?;
            // the depfile is a path, rather than part of a command
            scope.escape = false;
            let depfile = scope.lookup("depfile", 0).map_err(message)?;
            scope.escape = true;
            edge.depfile = (!depfile.is_empty()).then_some(depfile);
            match scope.lookup("deps", 0).map_err(message)?.as_str() {
                "" | "gcc" => (),
                deps => return Err(message(format!("`deps = {deps}` is not supported"))),
            }
            edge.pool = match scope.lookup("pool", 0).map_err(message)?.as_str() {
                "" => Pool::Default,
                "console" => Pool::Console,
                pool => return Err(message(format!("pool {pool:?} is not supported"))),
            };
            for unsupported in ["rspfile", "dyndep"] {
                if !scope.lookup(unsupported, 0).map_err(message)?.is_empty() {
                    return Err(message(format!("`{unsupported}` is not supported")));
                }
            }
            edge.generator = !scope.lookup("generator", 0).map_err(message)?.is_empty();
            edge.restat = !scope.lookup("restat", 0).map_err(message)?.is_empty();
            edge.kind = EdgeKind::Command(Action::Shell(command));
        }
        self.graph.add_edge(edge)?;
        Ok(())
    }
    /// Add to the targets which are built when none are requested.
    pub fn default(&mut self, paths: &[&str]) {
        let nodes: Vec<_> = paths.iter().map(|path| self.graph.file(path)).collect();
        self.graph.add_default(&nodes);
    }
    /// Returns the graph the statements build.
    #[must_use]
    pub fn finish(self) -> Graph {
        self.graph
    }
}

/// What a binding of a rule is evaluated in, for an edge.
struct Scope<'a> {
    build: &'a Build<'a>,
    /// The variables of the edge, evaluated.
    edge: &'a HashMap<String, String>,
    rule: &'a HashMap<String, Vec<Token>>,
    variables: &'a HashMap<String, String>,
    /// Whether `$in` and `$out` are quoted for the shell.
    escape: bool,
}
impl Scope<'_> {
    fn lookup(&self, name: &str, depth: usize) -> Result<String, String> {
        let paths = |paths: &[&str], separator| {
            let paths = paths.iter().map(|&path| if self.escape { shell_escape(path) } else { path.to_string() });
            paths.collect::<Vec<_>>().join(separator)
        };
        match name {
            "in" => return Ok(paths(self.build.inputs, " ")),
            "in_newline" => return Ok(paths(self.build.inputs, "\n")),
            "out" => return Ok(paths(self.build.outputs, " ")),
            _ => (),
        }
        if let Some(value) = self.edge.get(name) {
            return Ok(value.clone());
        }
        if let Some(tokens) = self.rule.get(name) {
            if depth >= MAX_DEPTH {
                return Err(format!("the binding {name:?} refers to itself"));
            }
            return evaluate(tokens, &mut |name| self.lookup(name, depth + 1));
        }
        Ok(self.variables.get(name).cloned().unwrap_or_default())
    }
}

fn message(message: String) -> Error {
    Error::Ninja { line: 0, message }
}

/// Read a ninja file into a graph.
///
/// # Errors
///
/// If the file is not valid, or uses what rig does not support, with the line of the statement.
pub fn read(text: &str) -> Result<Graph, Error> {
    let mut manifest = Manifest::new();
    let lines = logical_lines(text);
    let mut lines = lines.iter().peekable();
    while let Some(&(number, ref line)) = lines.next() {
        let at = |error: Error| match error {
            Error::Ninja { line: 0, message } => Error::Ninja { line: number, message },
            error => error,
        };
        let fail = |message: String| Error::Ninja { line: number, message };
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(fail("unexpected indentation".into()));
        }
        // the indented bindings which follow a statement
        let mut bindings = Vec::new();
        let indented = |(_, line): &&(usize, String)| line.starts_with([' ', '\t']) || line.trim().is_empty();
        while let Some((number, binding)) = lines.next_if(indented) {
            let binding = binding.trim_start();
            if binding.is_empty() || binding.starts_with('#') {
                continue;
            }
            let Some(binding) = split_binding(binding) else {
                let message = format!("expected a binding, not {binding:?}");
                return Err(Error::Ninja { line: *number, message });
            };
            bindings.push(binding);
        }
        let (keyword, rest) = line.split_once([' ', '\t']).unwrap_or((line.as_str(), ""));
        match keyword {
            "rule" => manifest.rule(rest.trim(), &bindings).map_err(at)?,
            "build" => read_build(&mut manifest, rest, &bindings).map_err(at)?,
            "default" => {
                let mut paths = Vec::new();
                for token in path_tokens(rest) {
                    match token {
                        PathToken::Path(raw) => {
                            paths.push(evaluate_path(&raw, &HashMap::new(), &manifest).map_err(fail)?);
                        },
                        _ => return Err(fail("unexpected `:` or `|` in `default`".into())),
                    }
                }
                manifest.default(&paths.iter().map(String::as_str).collect::<Vec<_>>());
            },
            "pool" | "include" | "subninja" => return Err(fail(format!("`{keyword}` is not supported"))),
            _ => {
                let Some((name, value)) = split_binding(line) else {
                    return Err(fail(format!("unexpected {line:?}")));
                };
                if !bindings.is_empty() {
                    return Err(fail(format!("unexpected indentation after the variable {name:?}")));
                }
                manifest.variable(name, value).map_err(at)?;
            },
        }
    }
    Ok(manifest.finish())
}

/// Returns the lines of a ninja file with their numbers, where a `$` before a line break continues the line, and the
/// whitespace which begins the next line is left out.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut start = 1;
    let mut number = 1;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' => match chars.peek() {
                Some('\n') => {
                    chars.next();
                    number += 1;
                    while chars.next_if(|&c| c == ' ').is_some() {}
                },
                Some('\r') => {
                    chars.next();
                    chars.next_if_eq(&'\n');
                    number += 1;
                    while chars.next_if(|&c| c == ' ').is_some() {}
                },
                Some(&next) => {
                    chars.next();
                    line.push('$');
                    line.push(next);
                },
                None => line.push('$'),
            },
            '\n' => {
                let line = std::mem::take(&mut line);
                lines.push((start, line.strip_suffix('\r').map(String::from).unwrap_or(line)));
                number += 1;
                start = number;
            },
            c => line.push(c),
        }
    }
    if !line.is_empty() {
        lines.push((start, line));
    }
    lines
}

/// Split `name = value`, where the value begins after the whitespace which follows `=`.
fn split_binding(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.split_once('=')?;
    let name = name.trim_end();
    if name.is_empty() || !name.chars().all(|c| is_simple_char(c) || c == '.') {
        return None;
    }
    Some((name, value.trim_start_matches([' ', '\t'])))
}

enum PathToken {
    Path(String),
    Colon,
    Pipe,
    PipePipe,
}

/// Split the paths of a statement, as they are written, at unescaped spaces, `:` and `|`.
fn path_tokens(text: &str) -> Vec<PathToken> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => (),
            ':' => tokens.push(PathToken::Colon),
            '|' if chars.next_if_eq(&'|').is_some() => tokens.push(PathToken::PipePipe),
            '|' => tokens.push(PathToken::Pipe),
            c => {
                let mut path = String::from(c);
                if c == '$' {
                    path.extend(chars.next());
                }
                while let Some(c) = chars.next_if(|&c| !matches!(c, ' ' | '\t' | ':' | '|')) {
                    path.push(c);
                    if c == '$' {
                        path.extend(chars.next());
                    }
                }
                tokens.push(PathToken::Path(path));
            },
        }
    }
    tokens
}

/// Evaluate a path as it is written, looking up a variable in the bindings of its edge, then at the top level.
fn evaluate_path(raw: &str, edge: &HashMap<&str, &str>, manifest: &Manifest) -> Result<String, String> {
    let tokens = parse(raw)?;
    evaluate(&tokens, &mut |name| {
        if let Some(value) = edge.get(name) {
            let tokens = parse(value)?;
            return evaluate(&tokens, &mut |name| Ok(manifest.variables.get(name).cloned().unwrap_or_default()));
        }
        Ok(manifest.variables.get(name).cloned().unwrap_or_default())
    })
}

/// Read the rest of a `build` statement, after `build`.
fn read_build(manifest: &mut Manifest, rest: &str, bindings: &[(&str, &str)]) -> Result<(), Error> {
    let edge: HashMap<&str, &str> = bindings.iter().copied().collect();
    let mut lists: [Vec<String>; 5] = Default::default();
    let mut rule = None;
    // outputs, then implicit outputs, then the rule, then inputs, implicit inputs and order-only inputs
    let mut list = 0;
    for token in path_tokens(rest) {
        match token {
            PathToken::Path(raw) if list == 2 && rule.is_none() => rule = Some(raw),
            PathToken::Path(raw) => lists[list].push(evaluate_path(&raw, &edge, manifest).map_err(message)?),
            PathToken::Pipe if list == 0 => list = 1,
            PathToken::Colon if list < 2 => list = 2,
            PathToken::Pipe if list == 2 && rule.is_some() => list = 3,
            PathToken::PipePipe if (2..4).contains(&list) && rule.is_some() => list = 4,
            _ => return Err(message("unexpected `:` or `|` in `build`".into())),
        }
    }
    let Some(rule) = rule else {
        return Err(message("expected a rule in `build`".into()));
    };
    if lists[0].is_empty() {
        return Err(message("expected an output in `build`".into()));
    }
    let lists: Vec<Vec<&str>> = lists.iter().map(|list| list.iter().map(String::as_str).collect()).collect();
    manifest.build(&Build {
        rule: &rule,
        outputs: &lists[0],
        implicit_outputs: &lists[1],
        inputs: &lists[2],
        implicit: &lists[3],
        order_only: &lists[4],
        bindings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(command: &str) -> EdgeKind {
        EdgeKind::Command(Action::Shell(command.into()))
    }

    #[test]
    fn parses_values() {
        let text = |s: &str| Token::Text(s.into());
        let variable = |s: &str| Token::Variable(s.into());
        assert_eq!(parse("a $$b$ c$:d").unwrap(), [text("a $b c:d")]);
        assert_eq!(parse("$out.d").unwrap(), [variable("out"), text(".d")]);
        assert_eq!(parse("${a.b}c $crate-env").unwrap(), [variable("a.b"), text("c "), variable("crate-env")]);
        assert!(parse("$").is_err());
        assert!(parse("$%").is_err());
        assert!(parse("${a").is_err());
    }

    #[test]
    fn escapes_paths_for_the_shell() {
        assert_eq!(shell_escape("a/b-c_d+e.f"), "a/b-c_d+e.f");
        assert_eq!(shell_escape("a b"), "'a b'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn evaluates_as_ninja_does() {
        let graph = read(
            "flags = -O\n\
             rule cc\n  command = cc $flags $extra -o $out $in\n  description = CC $out\n  depfile = $out.d\n  deps = gcc\n\
             flags = -O2\n\
             build a$ b.o | a.d: cc a.c b$:c.c | h.h || gen\n  extra = $$x $flags\n  flags = -g\n",
        )
        .unwrap();
        let edge = graph.edge(graph.producer(graph.node("a b.o").unwrap()).unwrap());
        // the edge's variables are evaluated where it is defined, and replace the top level's
        assert_eq!(edge.kind(), &shell("cc -g $x -O2 -o 'a b.o' a.c 'b:c.c'"));
        assert_eq!(edge.description(), "CC 'a b.o'");
        // as ninja does, the depfile is not quoted
        assert_eq!(edge.depfile(), Some("a b.o.d"));
        let paths = |nodes: &[NodeId]| nodes.iter().map(|&node| graph.path(node)).collect::<Vec<_>>();
        assert_eq!(paths(edge.implicit_outputs()), ["a.d"]);
        assert_eq!(paths(edge.inputs()), ["a.c", "b:c.c"]);
        assert_eq!(paths(edge.implicit()), ["h.h"]);
        assert_eq!(paths(edge.order_only()), ["gen"]);
    }

    #[test]
    fn reads_rules_and_aliases() {
        let graph = read(
            "# a comment\n\
             rule gen\n  command = gen $\n      $in\n  pool = console\n  generator = 1\n  restat = 1\n\n\
             build build.ninja: gen Jig\n\
             build all: phony build.ninja\n\
             default all\n",
        )
        .unwrap();
        let edge = graph.edge(graph.producer(graph.node("build.ninja").unwrap()).unwrap());
        assert_eq!(edge.kind(), &shell("gen Jig"));
        assert_eq!(edge.pool(), Pool::Console);
        assert!(edge.generator() && edge.restat());
        let alias = graph.edge(graph.producer(graph.node("all").unwrap()).unwrap());
        assert_eq!(alias.kind(), &EdgeKind::Alias);
        assert_eq!(graph.defaults(), &[graph.node("all").unwrap()]);
    }

    #[test]
    fn rule_bindings_refer_to_each_other() {
        let graph = read("rule r\n  command = run $description\n  description = $out\nbuild o: r\n").unwrap();
        assert_eq!(graph.edge(graph.producer(graph.node("o").unwrap()).unwrap()).kind(), &shell("run o"));
        let error = read("rule r\n  command = $command\nbuild o: r\n").unwrap_err();
        assert!(error.to_string().contains("refers to itself"), "{error}");
    }

    #[test]
    fn reports_errors_with_their_lines() {
        let error = |text: &str| read(text).unwrap_err().to_string();
        assert_eq!(error("rule r\n  command = c\n\nbuild o: missing\n"), "line 4: unknown rule \"missing\"");
        assert_eq!(error("rule r\n  depfile = d\n"), "line 1: rule \"r\" has no command");
        assert_eq!(error("rule r\n  command = c\n  wat = 1\n"), "line 1: unexpected variable \"wat\" in rule \"r\"");
        assert_eq!(error("include other.ninja\n"), "line 1: `include` is not supported");
        assert_eq!(error("rule r\n  command = c\n  pool = link\nbuild o: r\n"), "line 4: pool \"link\" is not supported");
        assert_eq!(error("rule r\n  command = c\nbuild o: r\nbuild o: r\n"), "\"o\" is produced by more than one edge");
        assert_eq!(error("  x = 1\n"), "line 1: unexpected indentation");
        assert_eq!(error("build o\n"), "line 1: expected a rule in `build`");
    }
}
