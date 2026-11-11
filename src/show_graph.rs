//! `jig graph`, which shows rig's graph of a project, as it is generated: each edge, with its outputs, inputs and
//! command, as text, or with `--json`, as JSON, for tools.
//!
//! What it shows is a stable interface: a later jig may add keys, but doesn't change or remove those it has.
//!
//! The text has a statement for each edge, as ninja's `build` statements are written, in the order the graph has them,
//! followed by the default targets:
//!
//! ```text
//! build <output> ... [| <implicit output> ...]: command|phony [<input> ...] [| <implicit> ...] [|| <order-only> ...]
//!     <key> = <value>
//! default <target> ...
//! ```
//!
//! Its paths are relative to the build directory, with `/` between their components on every platform, and a path
//! with a space, `|`, `:` or `$` has it escaped with `$`, as ninja escapes it. The lines under a statement are those of
//! what the edge has, in this order:
//!
//! - `description`, as a build shows it, unless it has none, as an alias.
//! - `command`, the command line, as `-v` shows it, with each word quoted for `sh` where it needs to be: its working
//!   directory, relative to the build directory, with `cd`, its variables, the program and its arguments, and the
//!   file stderr is written to, if it has one, with `2>`.
//! - `shell`, for a shell line, which only runs on Unix, rather than a program.
//! - `depfile`, `discover`, the command which writes the depfile without running the edge, `early`, the outputs it
//!   reports as it writes them, `pass-env`, the variables it passes on, a word each.
//! - `pool = console`, `cache`, the identity it is cached by, `target`, `tags` and `any-host`, which workers run it.
//! - `generator`, `restat` and `hashed`, each `= true`, if it is.
//!
//! The JSON is an object of `edges`, an array of an object for each, and of `defaults`, an array of paths. An edge has
//! `outputs`, `implicit-outputs`, `kind`, `command` or `phony`, `inputs`, `implicit` and `order-only`, then the keys of
//! the text, in its order, but for `command`, which it has in parts: `program`, `args`, an array, `env`, an array of
//! `[name, value]` pairs, `dir` and `stderr`. Those which are lists in the text are arrays, a flag is `true`, and what
//! an edge does not have is left out.

use std::fmt::Write as _;

use crate::Error;
use crate::configured::{Configured, Engine, Output};
use crate::rust_analyzer::string;

/// Show rig's graph of the project which contains the working directory, once it is up to date, or only the edges
/// which produce the outputs given, as text, or with `--json`, as JSON.
pub fn show(args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), Error> {
    let mut json = false;
    let mut outputs = Vec::new();
    for arg in args {
        match arg.to_string_lossy().as_ref() {
            "--json" => json = true,
            option if option.starts_with('-') => return Err(Error::RigArgument(option.into())),
            // as the graph has them, with `/`, which a path on Windows may be written without
            output => outputs.push(crate::graph::path_string(std::path::Path::new(output))),
        }
    }
    let project = Configured::find(Output::Stderr)?;
    if project.engine != Engine::Rig {
        return Err(Error::GraphOfNinja);
    }
    let graph = rig::Graph::load(&project.build_dir.join(crate::rig_graph::FILE)).map_err(Error::Rig)?;
    let mut ids = Vec::new();
    for output in &outputs {
        let edge = graph.node(output).and_then(|node| graph.producer(node));
        let edge = edge.ok_or_else(|| Error::Rig(rig::Error::UnknownTarget(output.clone())))?;
        if !ids.contains(&edge) {
            ids.push(edge);
        }
    }
    let edges: Vec<rig::EdgeRef<'_>> = if outputs.is_empty() {
        graph.edges().collect()
    } else {
        ids.into_iter().map(|id| graph.edge(id)).collect()
    };
    let all = outputs.is_empty();
    let shown = if json { to_json(&graph, &edges, all) } else { to_text(&graph, &edges, all) };
    print!("{shown}");
    Ok(())
}

/// What is shown of an edge, beside its paths, as the keys and values of its lines, each value as text, or as a list
/// of words.
enum Value {
    Text(String),
    Words(Vec<String>),
    Pairs(Vec<(String, String)>),
    Flag,
}

/// Returns what is shown of an edge, beside its paths, in order.
fn fields(graph: &rig::Graph, edge: rig::EdgeRef<'_>, text: bool) -> Vec<(&'static str, Value)> {
    let mut fields = Vec::new();
    if !edge.description().is_empty() {
        fields.push(("description", Value::Text(edge.description().into())));
    }
    match edge.kind() {
        rig::EdgeKind::Alias => {},
        rig::EdgeKind::Command(rig::Action::Shell(line)) => fields.push(("shell", Value::Text(line.clone()))),
        rig::EdgeKind::Command(action @ rig::Action::Exec(exec)) => {
            // the text has the command line, which they make up, and the JSON each part of it
            if text {
                fields.push(("command", Value::Text(action.command_line())));
                return finish(graph, edge, fields);
            }
            fields.push(("program", Value::Text(exec.program.clone())));
            if !exec.args.is_empty() {
                fields.push(("args", Value::Words(exec.args.clone())));
            }
            if !exec.env.is_empty() {
                fields.push(("env", Value::Pairs(exec.env.clone())));
            }
            for (key, value) in [("dir", &exec.dir), ("stderr", &exec.stderr)] {
                if let Some(value) = value {
                    fields.push((key, Value::Text(value.clone())));
                }
            }
        },
    }
    finish(graph, edge, fields)
}

/// Returns what is shown of an edge, with what follows its command.
fn finish(
    graph: &rig::Graph,
    edge: rig::EdgeRef<'_>,
    mut fields: Vec<(&'static str, Value)>,
) -> Vec<(&'static str, Value)> {
    let paths = |nodes: &[rig::NodeId]| nodes.iter().map(|&node| graph.path(node).to_string()).collect::<Vec<_>>();
    if let Some(depfile) = edge.depfile() {
        fields.push(("depfile", Value::Text(depfile.into())));
    }
    if let Some(discover) = edge.discover() {
        fields.push(("discover", Value::Text(discover.command_line())));
    }
    if !edge.early().is_empty() {
        fields.push(("early", Value::Words(paths(edge.early()))));
    }
    if !edge.pass_env().is_empty() {
        fields.push(("pass-env", Value::Words(edge.pass_env().to_vec())));
    }
    if edge.pool() == rig::Pool::Console {
        fields.push(("pool", Value::Text("console".into())));
    }
    if let Some(cache) = edge.cache() {
        fields.push(("cache", Value::Text(cache.into())));
    }
    let placement = edge.placement();
    if let Some(target) = &placement.target {
        fields.push(("target", Value::Text(target.clone())));
    }
    if !placement.tags.is_empty() {
        fields.push(("tags", Value::Words(placement.tags.clone())));
    }
    let flags = [
        ("any-host", placement.any_host),
        ("generator", edge.generator()),
        ("restat", edge.restat()),
        ("hashed", edge.hashed()),
    ];
    fields.extend(flags.into_iter().filter(|&(_, set)| set).map(|(key, _)| (key, Value::Flag)));
    fields
}

/// Returns a path as the text has it, with what would end it, or a list of them, escaped with `$`, as ninja does.
fn escape(path: &str) -> String {
    let mut escaped = String::new();
    for c in path.chars() {
        if matches!(c, ' ' | '|' | ':' | '$') {
            escaped.push('$');
        }
        escaped.push(c);
    }
    escaped
}

/// Returns the edges as text.
fn to_text(graph: &rig::Graph, edges: &[rig::EdgeRef<'_>], defaults: bool) -> String {
    let mut text = String::new();
    let paths = |nodes: &[rig::NodeId]| -> String {
        nodes.iter().fold(String::new(), |mut paths, &node| {
            paths.push(' ');
            paths.push_str(&escape(graph.path(node)));
            paths
        })
    };
    for &edge in edges {
        let kind = if *edge.kind() == rig::EdgeKind::Alias { "phony" } else { "command" };
        let _ = write!(text, "build{}", paths(edge.outputs()));
        if !edge.implicit_outputs().is_empty() {
            let _ = write!(text, " |{}", paths(edge.implicit_outputs()));
        }
        let _ = write!(text, ": {kind}{}", paths(edge.inputs()));
        if !edge.implicit().is_empty() {
            let _ = write!(text, " |{}", paths(edge.implicit()));
        }
        if !edge.order_only().is_empty() {
            let _ = write!(text, " ||{}", paths(edge.order_only()));
        }
        text.push('\n');
        for (key, value) in fields(graph, edge, true) {
            match value {
                Value::Text(value) => _ = writeln!(text, "    {key} = {value}"),
                Value::Words(words) => {
                    let words: Vec<String> = words.iter().map(|word| quote(word)).collect();
                    let _ = writeln!(text, "    {key} = {}", words.join(" "));
                },
                Value::Pairs(pairs) => {
                    for (name, value) in pairs {
                        let _ = writeln!(text, "    {key} = {name}={}", quote(&value));
                    }
                },
                Value::Flag => _ = writeln!(text, "    {key} = true"),
            }
        }
        text.push('\n');
    }
    if defaults && !graph.defaults().is_empty() {
        let _ = writeln!(text, "default{}", paths(graph.defaults()));
    }
    text
}

/// Quote a word for `sh`, as a command line shows it, unless every character of it is known to be safe.
fn quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_+-./=:,@%".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Returns the edges as JSON.
fn to_json(graph: &rig::Graph, edges: &[rig::EdgeRef<'_>], defaults: bool) -> String {
    let list = |items: &mut dyn Iterator<Item = String>| format!("[{}]", items.collect::<Vec<_>>().join(", "));
    let paths = |nodes: &[rig::NodeId]| list(&mut nodes.iter().map(|&node| string(graph.path(node))));
    let mut entries = Vec::new();
    for &edge in edges {
        let kind = if *edge.kind() == rig::EdgeKind::Alias { "phony" } else { "command" };
        let mut entry = vec![
            format!("\"outputs\": {}", paths(edge.outputs())),
            format!("\"implicit-outputs\": {}", paths(edge.implicit_outputs())),
            format!("\"kind\": {}", string(kind)),
            format!("\"inputs\": {}", paths(edge.inputs())),
            format!("\"implicit\": {}", paths(edge.implicit())),
            format!("\"order-only\": {}", paths(edge.order_only())),
        ];
        for (key, value) in fields(graph, edge, false) {
            let value = match value {
                Value::Text(value) => string(&value),
                Value::Words(words) => list(&mut words.iter().map(|word| string(word))),
                Value::Pairs(pairs) => {
                    list(&mut pairs.iter().map(|(name, value)| format!("[{}, {}]", string(name), string(value))))
                },
                Value::Flag => "true".into(),
            };
            entry.push(format!("{}: {value}", string(key)));
        }
        entries.push(format!("    {{{}}}", entry.join(", ")));
    }
    let defaults = if defaults { paths(graph.defaults()) } else { "[]".into() };
    format!("{{\"edges\": [\n{}\n], \"defaults\": {defaults}}}\n", entries.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_paths_as_ninja_does() {
        assert_eq!(escape("a b|c:d$e"), "a$ b$|c$:d$$e");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("--crate-type=lib"), "--crate-type=lib");
    }
}
