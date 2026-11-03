//! Stamps, which tell whether a file has changed since it was recorded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::graph::{Graph, NodeId};

/// What a file was when it was recorded: its modification time and size, which come from the same `stat` call, or
/// that it was missing.
///
/// A file has changed if its stamp differs from the one recorded, in either direction, so a file whose modification
/// time goes back, as when an older version is checked out, has changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stamp {
    Missing,
    File {
        /// Nanoseconds since the Unix epoch, negative before it.
        mtime: i64,
        size: u64,
    },
}
impl Stamp {
    /// Returns the stamp of a file as it is now, following symbolic links, as ninja does.
    ///
    /// A file which cannot be read, other than one which is missing, is taken as missing, so that what depends on it
    /// is built, which reports the error.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        let Ok(metadata) = std::fs::metadata(path) else {
            return Self::Missing;
        };
        let mtime = match metadata.modified() {
            Ok(time) => match time.duration_since(std::time::UNIX_EPOCH) {
                Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
                Err(before) => i64::try_from(before.duration().as_nanos()).map_or(i64::MIN, |nanos| -nanos),
            },
            // a platform without modification times is compared by size alone
            Err(_) => 0,
        };
        Self::File { mtime, size: metadata.len() }
    }
    #[must_use]
    pub fn exists(self) -> bool {
        self != Self::Missing
    }
}

/// The stamps of files, each found once, of the nodes of a graph and of other paths, relative to a directory.
pub struct Stamps<'a> {
    dir: PathBuf,
    graph: &'a Graph,
    nodes: Vec<Option<Stamp>>,
    paths: HashMap<String, Stamp>,
}
impl<'a> Stamps<'a> {
    /// The stamps of the files of a graph, whose paths are relative to `dir`.
    #[must_use]
    pub fn new(graph: &'a Graph, dir: &Path) -> Self {
        Self { dir: dir.into(), graph, nodes: vec![None; graph.nodes()], paths: HashMap::new() }
    }
    /// Returns the stamp of a node, finding it if it has not been.
    pub fn node(&mut self, node: NodeId) -> Stamp {
        let index = node.0 as usize;
        if let Some(stamp) = self.nodes[index] {
            return stamp;
        }
        let stamp = Stamp::of(&self.dir.join(self.graph.path(node)));
        self.nodes[index] = Some(stamp);
        stamp
    }
    /// Returns the stamp of a path, which is a node's if it is one, finding it if it has not been.
    pub fn path(&mut self, path: &str) -> Stamp {
        if let Some(node) = self.graph.node(path) {
            return self.node(node);
        }
        if let Some(&stamp) = self.paths.get(path) {
            return stamp;
        }
        let stamp = Stamp::of(&self.dir.join(path));
        self.paths.insert(path.into(), stamp);
        stamp
    }
    /// Forget the stamp of a node, such as an output once its edge has run, so that it is found again.
    pub fn forget(&mut self, node: NodeId) {
        self.nodes[node.0 as usize] = None;
    }
}
