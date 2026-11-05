//! Building a [`Graph`] from rules, edges and variables, as ninja evaluates them.
//!
//! A [`Manifest`] takes rules, edges and variables, whose values are written as ninja's are, and evaluates them as
//! ninja does: the variables of an edge are evaluated where it is defined, then the bindings of its rule are evaluated
//! for it, looking up a variable in the edge, then the rule, then the variables defined before the edge. `$in` and
//! `$out` are its explicit inputs and outputs, quoted for the shell as ninja quotes them, except in the depfile. Each
//! edge then has its command, description and depfile.
//!
//! Only what rig supports can be given: `deps = msvc`, pools other than `console`, response files and dyndep files are
//! errors.

use std::collections::HashMap;

use crate::graph::{Action, Edge, EdgeKind, Graph, NodeId, Pool};
use crate::Error;

/// The bindings a rule can have, as ninja allows, and `discover`, which only rig reads, as a command which writes the
/// depfile of an edge which has not run, without running it. ninja does not allow it, so a manifest for both does not
/// have it, but gives rig its edges' actions which discover, as jig does.
const RULE_BINDINGS: &[&str] = &[
    "command",
    "discover",
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
    /// What the edge runs, in place of the command of its rule, whose other bindings still apply.
    pub action: Option<Action>,
    /// Whether the edge's files are stamped by their contents, which ninja does not do.
    pub hashed: bool,
    /// What identifies the edge in a cache, beside its action and inputs, if its results are cached.
    pub cache: Option<String>,
    /// The environment variables the edge's action reads from those it inherits, whose values are its inputs.
    pub pass_env: Vec<String>,
    /// Which workers the edge may run on, if it is cached.
    pub placement: crate::Placement,
    /// What finds the inputs the edge discovers without running it, in place of the `discover` binding of its rule,
    /// which rig takes as a command which writes its depfile, while ninja does not know it.
    pub discover: Option<Action>,
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
            edge.kind = EdgeKind::Command(build.action.clone().unwrap_or(Action::Shell(command)));
            edge.hashed = build.hashed;
            edge.cache.clone_from(&build.cache);
            edge.pass_env.clone_from(&build.pass_env);
            edge.placement.clone_from(&build.placement);
            let discover = scope.lookup("discover", 0).map_err(message)?;
            let bound = (!discover.is_empty()).then_some(Action::Shell(discover));
            edge.discover = build.discover.clone().or(bound);
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
    Error::Manifest(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::graph;

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
        let graph = graph()
            .variable("flags", "-O")
            .rule("cc", &[
                ("command", "cc $flags $extra -o $out $in"),
                ("description", "CC $out"),
                ("depfile", "$out.d"),
                ("deps", "gcc"),
            ])
            .variable("flags", "-O2")
            .edge_with("cc", &["a b.o", "|", "a.d"], &["a.c", "b:c.c", "|", "h.h", "||", "gen"], &[
                ("extra", "$$x $flags"),
                ("flags", "-g"),
            ])
            .finish();
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
    fn makes_rules_and_aliases() {
        let graph = graph()
            .rule("gen", &[("command", "gen $in"), ("pool", "console"), ("generator", "1"), ("restat", "1")])
            .edge("gen", &["build.ninja"], &["Jig"])
            .edge("phony", &["all"], &["build.ninja"])
            .default(&["all"])
            .finish();
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
        let graph = graph().rule("r", &[("command", "run $description"), ("description", "$out")]);
        let graph = graph.edge("r", &["o"], &[]).finish();
        assert_eq!(graph.edge(graph.producer(graph.node("o").unwrap()).unwrap()).kind(), &shell("run o"));
        let mut graph = crate::testing::graph().rule("r", &[("command", "$command")]);
        let error = graph.try_edge("r", &["o"], &[], &[]).unwrap_err();
        assert!(error.to_string().contains("refers to itself"), "{error}");
    }

    #[test]
    fn refuses_what_it_does_not_support() {
        let rule = |bindings: &[(&str, &str)]| Manifest::new().rule("r", bindings).unwrap_err().to_string();
        assert_eq!(rule(&[("depfile", "d")]), "rule \"r\" has no command");
        assert_eq!(rule(&[("command", "c"), ("wat", "1")]), "unexpected variable \"wat\" in rule \"r\"");
        let edge = |bindings: &[(&str, &str)], rule: &str| {
            let mut graph = graph().rule("r", bindings);
            graph.try_edge(rule, &["o"], &[], &[]).unwrap_err().to_string()
        };
        assert_eq!(edge(&[("command", "c")], "missing"), "unknown rule \"missing\"");
        assert_eq!(edge(&[("command", "c"), ("pool", "link")], "r"), "pool \"link\" is not supported");
        assert_eq!(edge(&[("command", "c"), ("deps", "msvc")], "r"), "`deps = msvc` is not supported");
        let mut graph = graph().rule("r", &[("command", "c")]).edge("r", &["o"], &[]);
        let error = graph.try_edge("r", &["o"], &[], &[]).unwrap_err();
        assert_eq!(error.to_string(), "\"o\" is produced by more than one edge");
        assert!(Manifest::new().variable("v", "$").is_err());
    }
}
