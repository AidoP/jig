//! Stamps, which tell whether a file has changed since it was recorded.
//!
//! The stamps a build needs are found together before it is planned, by [`stamps_of`], on several threads, or with
//! `io_uring`, which submits them in batches, if `RIG_IO_URING` is `1` and the system supports it.

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
    /// Find the stamps of nodes and paths which have not been found, together, so that they are not found one by one.
    pub fn prefetch(&mut self, nodes: &[NodeId], paths: &[&str]) {
        let nodes: Vec<NodeId> = nodes.iter().copied().filter(|node| self.nodes[node.0 as usize].is_none()).collect();
        let new = |path: &&str| self.graph.node(path).is_none() && !self.paths.contains_key(*path);
        let paths: Vec<&str> = paths.iter().copied().filter(new).collect();
        let all = nodes.iter().map(|&node| self.graph.path(node)).chain(paths.iter().copied());
        let full: Vec<PathBuf> = all.map(|path| self.dir.join(path)).collect();
        let full: Vec<&Path> = full.iter().map(PathBuf::as_path).collect();
        let mut stamps = stamps_of(&full).into_iter();
        for (node, stamp) in nodes.iter().zip(stamps.by_ref()) {
            self.nodes[node.0 as usize] = Some(stamp);
        }
        for (path, stamp) in paths.iter().zip(stamps) {
            self.paths.insert((*path).to_string(), stamp);
        }
    }
    /// Forget the stamp of a node, such as an output once its edge has run, so that it is found again.
    pub fn forget(&mut self, node: NodeId) {
        self.nodes[node.0 as usize] = None;
    }
}

/// The fewest files stamped on threads of their own, below which they are stamped on the calling thread.
const PARALLEL_MIN: usize = 256;

/// Returns the stamps of files, in the same order, found together: with `io_uring` if `RIG_IO_URING` is `1` and the
/// system supports it, or otherwise on several threads if there are enough of them.
#[must_use]
pub fn stamps_of(paths: &[&Path]) -> Vec<Stamp> {
    #[cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]
    if paths.len() > 1
        && std::env::var_os("RIG_IO_URING").is_some_and(|value| value == "1")
        && let Ok(mut ring) = crate::uring::Ring::new()
    {
        return ring.stamps(paths);
    }
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    if paths.len() < PARALLEL_MIN || threads == 1 {
        return paths.iter().map(|path| Stamp::of(path)).collect();
    }
    let chunk = paths.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let stamp = |chunk: &[&Path]| chunk.iter().map(|path| Stamp::of(path)).collect::<Vec<_>>();
        let handles: Vec<_> = paths.chunks(chunk).map(|chunk| scope.spawn(move || stamp(chunk))).collect();
        handles.into_iter().flat_map(|handle| handle.join().unwrap_or_default()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_together_as_one_by_one() {
        let dir = std::env::temp_dir().join(format!("rig-stamps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = (0..PARALLEL_MIN * 2).map(|i| dir.join(format!("f{i}"))).collect();
        for (i, path) in paths.iter().enumerate().filter(|(i, _)| i % 3 != 0) {
            std::fs::write(path, "x".repeat(i)).unwrap();
        }
        let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let expected: Vec<Stamp> = refs.iter().map(|path| Stamp::of(path)).collect();
        assert_eq!(stamps_of(&refs), expected);
        assert_eq!(stamps_of(&refs[..3]), expected[..3]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
