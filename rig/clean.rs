//! Removing what a build wrote, by the outputs of the graph's edges, so that files in the build's directory which no
//! edge writes are left alone.

use std::collections::HashSet;
use std::path::Path;

use crate::graph::{EdgeId, EdgeKind, Graph, NodeId};
use crate::state::State;
use crate::{Error, plan};

/// What [`clean`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleaned {
    /// The files removed.
    pub files: usize,
    /// The edges whose records were forgotten, as they had run.
    pub edges: usize,
}

/// Remove the files the edges of a graph write, in the build's directory `dir`, and forget their records, so that
/// they run on the next build. With targets, those are the edges which build the targets and what they are built
/// from, through their inputs, dependencies, what they are built after and the inputs they discovered; without, they
/// are every edge.
///
/// A generator's outputs are left, as they are what describes the build, such as the graph itself, and so is its
/// record. What an edge writes is its outputs, its depfile and the file its stderr is written to. A directory left
/// empty by the files removed is removed too, up to `dir`.
///
/// # Errors
///
/// If a target is not in the graph, a file cannot be removed, or the state cannot be written.
pub fn clean(graph: &Graph, state: &mut State, dir: &Path, targets: &[&str]) -> Result<Cleaned, Error> {
    let edges: Vec<EdgeId> = if targets.is_empty() {
        (0..graph.edges().len()).map(|index| EdgeId(u32::try_from(index).unwrap_or(u32::MAX))).collect()
    } else {
        built_from(graph, state, &plan::target_nodes(graph, targets)?)
    };
    let mut cleaned = Cleaned::default();
    let mut forgotten = Vec::new();
    let mut parents = HashSet::new();
    for id in edges {
        let edge = graph.edge(id);
        let EdgeKind::Command(action) = edge.kind() else {
            continue;
        };
        if edge.generator() {
            continue;
        }
        let outputs = edge.outputs().iter().map(|&node| graph.path(node));
        for path in outputs.chain(edge.depfile()).chain(action.stderr.as_deref()) {
            let full = dir.join(path);
            match std::fs::remove_file(&full) {
                Ok(()) => cleaned.files += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(Error::Io(full, error)),
            }
            parents.extend(Path::new(path).parent().filter(|parent| !parent.as_os_str().is_empty()));
        }
        let key = plan::first_output(graph, id);
        if state.get(key).is_some() {
            forgotten.push(key.to_string());
        }
    }
    cleaned.edges = forgotten.len();
    state.forget(&forgotten)?;
    // the deepest first, so that a directory whose subdirectories are removed is then empty
    let mut parents: Vec<&Path> = parents.into_iter().flat_map(Path::ancestors).collect();
    parents.retain(|parent| !parent.as_os_str().is_empty());
    parents.sort_unstable_by_key(|parent| std::cmp::Reverse(parent.components().count()));
    parents.dedup();
    for parent in parents {
        // a directory which is not empty, or is already gone, is left
        let _ = std::fs::remove_dir(dir.join(parent));
    }
    Ok(cleaned)
}

/// Returns the edges which build nodes, and those which build what they are built from, each once.
fn built_from(graph: &Graph, state: &State, nodes: &[NodeId]) -> Vec<EdgeId> {
    let mut seen = HashSet::new();
    let mut edges = Vec::new();
    let mut pending: Vec<NodeId> = nodes.to_vec();
    while let Some(node) = pending.pop() {
        let Some(id) = graph.producer(node) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        edges.push(id);
        let edge = graph.edge(id);
        pending.extend(edge.inputs().iter().chain(edge.dependencies()).chain(edge.after()));
        pending.extend(plan::discovered_nodes(graph, state, id));
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::graph;

    #[test]
    fn removes_what_edges_wrote() {
        let dir = std::env::temp_dir().join(format!("rig-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let graph = graph()
            .rule("cc", &[("command", "cat $in > $out"), ("depfile", "$out.d")])
            .rule("ld", &[("command", "cat $in > $out")])
            .rule("gen", &[("command", "touch $out"), ("generator", "")])
            .edge("cc", &["obj/a.o"], &["a.c"])
            .edge("cc", &["obj/deep/b.o"], &["b.c"])
            .edge("ld", &["app"], &["obj/a.o"])
            .edge("gen", &["graph"], &[])
            .edge("alias", &["all"], &["app"])
            .finish();
        let write = |path: &str| {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, path).unwrap();
        };
        let written = ["obj/a.o", "obj/a.o.d", "obj/deep/b.o", "obj/deep/b.o.d", "app", "graph", "a.c", "b.c"];
        let record = |state: &mut State, output: &str| {
            let outputs = vec![(output.to_string(), crate::Stamp::Missing)];
            let record = crate::state::Record {
                start: 0,
                duration: 0,
                action: 0,
                outputs,
                inputs: Vec::new(),
                discovered: Vec::new(),
                env: Vec::new(),
            };
            state.record(record).unwrap();
        };
        let reset = |state: &mut State| {
            written.into_iter().chain(["obj/notes"]).for_each(write);
            for output in ["obj/a.o", "obj/deep/b.o", "app", "graph"] {
                record(state, output);
            }
        };
        let mut state = State::open(&dir.join(".rig")).unwrap();
        // a target, and what it is built from, through the alias, but not what it is not
        reset(&mut state);
        let cleaned = clean(&graph, &mut state, &dir, &["all"]).unwrap();
        assert_eq!(cleaned, Cleaned { files: 3, edges: 2 });
        assert!(!dir.join("app").exists() && !dir.join("obj/a.o.d").exists());
        assert!(dir.join("obj/deep/b.o").exists() && state.get("obj/deep/b.o").is_some());
        assert!(state.get("app").is_none() && state.get("obj/a.o").is_none());
        // everything, but for the generator's outputs, the sources, and what no edge writes, and the directory left
        // empty
        reset(&mut state);
        let cleaned = clean(&graph, &mut state, &dir, &[]).unwrap();
        assert_eq!(cleaned, Cleaned { files: 5, edges: 3 });
        for path in ["graph", "a.c", "b.c", "obj/notes"] {
            assert!(dir.join(path).exists(), "{path}");
        }
        assert!(!dir.join("obj/deep").exists());
        assert!(state.get("graph").is_some());
        // which the state remembers once it is opened again, which may wait for a process another test is starting,
        // which holds a copy of the lock until it runs its program
        drop(state);
        let reopen = || State::open(&dir.join(".rig"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut state = reopen();
        while matches!(state, Err(Error::Locked(_))) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            state = reopen();
        }
        let state = state.unwrap();
        assert_eq!(state.len(), 1);
        assert!(matches!(clean(&graph, &mut { state }, &dir, &["nothing"]), Err(Error::UnknownTarget(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
