//! `jig graph`, which shows rig's graph of a project, as it is generated: each edge, with its outputs, inputs and
//! command, as text, or with `--json`, as JSON, for tools.
//!
//! What it shows is a stable interface: a later jig may add keys, but doesn't change or remove those it has.
//!
//! The text has a block for each edge, in the order the graph has them, headed by its outputs, followed by an indented
//! line for each of what it has, as `<key> = <value>`, and a blank line, then the default targets:
//!
//! ```text
//! <output> ...
//!     <key> = <value>
//!
//! default <target> ...
//! ```
//!
//! Its paths are relative to the build directory, with `/` between their components on every platform, and each path
//! and word is quoted for `sh` where it needs to be, as a command line has it. The lines of an edge are those of what
//! it has, in this order:
//!
//! - `description`, as a build shows it, unless it has none, as an alias.
//! - `inputs`, the files it reads, which must be there, `dependencies`, those it depends on, which may be missing, as a
//!   crate it uses, and `after`, those which are built before it, but which it neither reads nor depends on.
//! - `alias`, for an alias, the targets it names, in place of a description and a command.
//! - `command`, the command line, as `-v` shows it: its working directory, relative to the build directory, with `cd`,
//!   its variables, the program and its arguments, and the file stderr is written to, if it has one, with `2>`.
//! - `depfile`, `discover`, the command which writes the depfile without running the edge, `early`, the outputs it
//!   reports as it writes them, `pass-env`, the variables it passes on, a word each.
//! - `pool = console`, `cache`, the identity it is cached by, `target`, `tags` and `any-host`, which workers run it.
//! - `generator`, `restat` and `hashed`, each `= true`, if it is.
//!
//! The JSON is an object of `edges`, an array of an object for each, and of `defaults`, an array of paths. An edge has
//! `outputs`, then the keys of the text, in its order, but for `command`, which it has in parts: `program`, `args`,
//! `env`, an array of `[name, value]` pairs, `dir` and `stderr`. Those which are lists in the text are arrays, a flag
//! is `true`, and what an edge does not have is left out.

use std::fmt::Write as _;

use crate::Error;
use crate::configured::{Configured, Output};
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
            output => outputs.push(crate::rig_graph::path_string(std::path::Path::new(output))),
        }
    }
    let project = Configured::find(Output::Stderr)?;
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
    let paths = |nodes: &[rig::NodeId]| nodes.iter().map(|&node| graph.path(node).to_string()).collect::<Vec<_>>();
    let mut fields = Vec::new();
    if !edge.description().is_empty() {
        fields.push(("description", Value::Text(edge.description().into())));
    }
    if *edge.kind() == rig::EdgeKind::Alias {
        fields.push(("alias", Value::Words(paths(edge.inputs()))));
        return finish(graph, edge, fields);
    }
    let lists = [("inputs", edge.inputs()), ("dependencies", edge.dependencies()), ("after", edge.after())];
    for (key, nodes) in lists {
        if !nodes.is_empty() {
            fields.push((key, Value::Words(paths(nodes))));
        }
    }
    match edge.kind() {
        rig::EdgeKind::Alias => {},
        rig::EdgeKind::Command(exec) => {
            // the text has the command line, which they make up, and the JSON each part of it
            if text {
                fields.push(("command", Value::Text(exec.command_line())));
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

/// Returns the edges as text.
fn to_text(graph: &rig::Graph, edges: &[rig::EdgeRef<'_>], defaults: bool) -> String {
    let mut text = String::new();
    let paths = |nodes: &[rig::NodeId]| nodes.iter().map(|&node| quote(graph.path(node))).collect::<Vec<_>>().join(" ");
    for &edge in edges {
        let _ = writeln!(text, "{}", paths(edge.outputs()));
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
        let _ = writeln!(text, "default {}", paths(graph.defaults()));
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
        let mut entry = vec![format!("\"outputs\": {}", paths(edge.outputs()))];
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
    fn quotes_words_for_sh() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("--crate-type=lib"), "--crate-type=lib");
    }
}
