//! Stamps, which tell whether a file has changed since it was recorded.
//!
//! The stamps a build needs are found together before it is planned, by [`stamps_of`], on several threads.

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
    /// A file stamped by its contents, as an edge which is hashed stamps its files, with the time and size it had, by
    /// which its hash is reused while they are the same.
    Content { mtime: i64, size: u64, hash: [u8; 32] },
    /// An output left on a worker, which wrote it, rather than fetched, by its contents and whether it is executable,
    /// which is fetched once something here needs it. Its file is missing until then, and its size is not known.
    Remote { hash: [u8; 32], executable: bool },
}
impl Stamp {
    /// The size of the file, or `0` if it is missing, or left on a worker.
    #[must_use]
    pub fn size(&self) -> u64 {
        match self {
            Self::Missing | Self::Remote { .. } => 0,
            Self::File { size, .. } | Self::Content { size, .. } => *size,
        }
    }
    /// Returns the stamp of a file as it is now, following symbolic links.
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
    /// Returns the hash of the contents of the file, if it is stamped by them, as one left on a worker is.
    #[must_use]
    pub fn hash(&self) -> Option<&[u8; 32]> {
        match self {
            Self::Content { hash, .. } | Self::Remote { hash, .. } => Some(hash),
            Self::Missing | Self::File { .. } => None,
        }
    }
    /// Whether the file was left on a worker.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }
    #[must_use]
    pub fn exists(self) -> bool {
        self != Self::Missing
    }
    /// Whether a file has not changed between two stamps: by its contents if both are of them, as a file left on a
    /// worker is, or otherwise by its time and size, while a stamp of one kind differs from a stamp of the other.
    #[must_use]
    pub fn same(self, other: Self) -> bool {
        match (self.hash(), other.hash()) {
            (Some(hash), Some(other)) => hash == other,
            _ => self == other,
        }
    }
}

/// The granularity of the modification time of a file, beyond which a change to a file after it was stamped changes
/// its time: a second if the time has no fraction of one, as on file systems with times in seconds, or otherwise the
/// kernel's clock, which is coarser than its nanoseconds.
fn tick(mtime: i64) -> i64 {
    if mtime % 1_000_000_000 == 0 { 1_000_000_000 } else { 10_000_000 }
}

/// Whether a file which was stamped at a time, in milliseconds since the Unix epoch, may have changed again within the
/// same tick of its time since then, so that its time and size alone do not show it, as git's racy files.
fn racy(mtime: i64, stamped: u64) -> bool {
    let stamped = i64::try_from(stamped).unwrap_or(i64::MAX / 1_000_000).saturating_mul(1_000_000);
    mtime >= stamped.saturating_sub(tick(mtime))
}

/// The stamps of files, each found once, of the nodes of a graph and of other paths, relative to a directory, and of
/// environment variables, as the commands of a build see them.
pub struct Stamps<'a> {
    dir: PathBuf,
    graph: &'a Graph,
    nodes: Vec<Option<Stamp>>,
    paths: HashMap<String, Stamp>,
    /// The variables the commands are given, beside those they inherit.
    env: Vec<(String, String)>,
    /// The stamps of the contents of files, by path, each found once.
    contents: HashMap<String, Stamp>,
    /// The outputs left on workers, by path, whose stamps they have while their files are missing.
    remote: HashMap<String, Stamp>,
    /// The stamps of the contents of files which were found elsewhere, as the thread which ran an edge hashed its
    /// outputs, by path, which stand while their files have the times and sizes they had.
    known: HashMap<String, Stamp>,
}
impl<'a> Stamps<'a> {
    /// The stamps of the files of a graph, whose paths are relative to `dir`.
    #[must_use]
    pub fn new(graph: &'a Graph, dir: &Path) -> Self {
        let nodes = vec![None; graph.nodes()];
        let (paths, contents, remote, known) = (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
        Self { dir: dir.into(), graph, nodes, paths, env: Vec::new(), contents, remote, known }
    }
    /// The contents of a file were stamped elsewhere, as by the thread which ran the edge which wrote it, which stands
    /// while it has the time and size it had, rather than it being read again.
    pub fn know(&mut self, path: &str, stamp: Stamp) {
        if matches!(stamp, Stamp::Content { .. }) {
            self.known.insert(path.to_string(), stamp);
        }
    }
    /// Stamp the outputs left on workers, by path, by these stamps while their files are missing, beside those which
    /// were.
    pub fn extend_remote(&mut self, remote: HashMap<String, Stamp>) {
        self.remote.extend(remote);
    }
    /// An output has been left on a worker, with this stamp, which it has while its file is missing, or with `None`,
    /// is no longer, as its edge has run again, or it was lost, so that it is stamped again.
    pub fn set_remote(&mut self, path: &str, stamp: Option<Stamp>) {
        match stamp {
            Some(stamp) => self.remote.insert(path.to_string(), stamp),
            None => self.remote.remove(path),
        };
        match self.graph.node(path) {
            Some(node) => self.forget(node),
            None => {
                self.paths.remove(path);
            },
        }
    }
    /// Returns the stamp of a file left on a worker, if it is one, and it is missing here.
    fn or_remote(&self, path: &str, stamp: Stamp) -> Stamp {
        match stamp {
            Stamp::Missing => self.remote.get(path).copied().unwrap_or(stamp),
            stamp => stamp,
        }
    }
    /// Stamp environment variables as they are for commands which are given `env` beside what they inherit.
    #[must_use]
    pub fn with_env(mut self, env: &[(String, String)]) -> Self {
        self.env = env.to_vec();
        self
    }
    /// Returns the stamp of an environment variable, a hash of its value, or `None` if it is not set, as a command
    /// sees it.
    ///
    /// A hash is kept rather than the value, which may be a secret, and is written to the state of the build.
    #[must_use]
    pub fn env(&self, name: &str) -> Option<u64> {
        self.env_value(name, crate::state::fnv1a)
    }
    /// Returns the BLAKE3 hash of an environment variable's value, or `None` if it is not set, as a command sees it,
    /// as the key of a cache is made of.
    #[must_use]
    pub fn env_hash(&self, name: &str) -> Option<[u8; 32]> {
        self.env_value(name, crate::blake3::hash)
    }
    fn env_value<T>(&self, name: &str, hash: impl Fn(&[u8]) -> T) -> Option<T> {
        if let Some((_, value)) = self.env.iter().rev().find(|(variable, _)| variable == name) {
            return Some(hash(value.as_bytes()));
        }
        // a value is hashed as the platform encodes it, which is its bytes on Unix
        std::env::var_os(name).map(|value| hash(value.as_encoded_bytes()))
    }
    /// Returns the stamp of a node, finding it if it has not been.
    pub fn node(&mut self, node: NodeId) -> Stamp {
        let index = node.0 as usize;
        if let Some(stamp) = self.nodes[index] {
            return stamp;
        }
        let path = self.graph.path(node);
        let stamp = self.or_remote(path, Stamp::of(&self.dir.join(path)));
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
        let stamp = self.or_remote(path, Stamp::of(&self.dir.join(path)));
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
            self.nodes[node.0 as usize] = Some(self.or_remote(self.graph.path(*node), stamp));
        }
        for (path, stamp) in paths.iter().zip(stamps) {
            let stamp = self.or_remote(path, stamp);
            self.paths.insert((*path).to_string(), stamp);
        }
    }
    /// Forget the stamp of a node, such as an output once its edge has run, so that it is found again.
    pub fn forget(&mut self, node: NodeId) {
        self.nodes[node.0 as usize] = None;
        self.contents.remove(self.graph.path(node));
    }
    /// Returns the stamp of a node as an edge compares it: its time and size, or with `hashed`, its contents too.
    ///
    /// The hash of a stamp which was `recorded`, with the time it was stamped in milliseconds since the Unix epoch, is
    /// reused while the file's time and size are those it had then, so that it is not read again. That is, unless the
    /// file is a source, which no edge produces, which changed within a tick of its time of being stamped, as a person
    /// may edit it again within that tick, which its time would not show.
    pub fn node_as(&mut self, node: NodeId, hashed: bool, recorded: Option<(&Stamp, u64)>) -> Stamp {
        let stamp = self.node(node);
        if !hashed {
            return stamp;
        }
        let source = self.graph.producer(node).is_none();
        self.content(self.graph.path(node).to_string(), stamp, source, recorded)
    }
    /// Returns the stamp of a path as an edge compares it, as [`Self::node_as`] does, which is a source unless it is a
    /// node which an edge produces.
    pub fn path_as(&mut self, path: &str, hashed: bool, recorded: Option<(&Stamp, u64)>) -> Stamp {
        if let Some(node) = self.graph.node(path) {
            return self.node_as(node, hashed, recorded);
        }
        let stamp = self.path(path);
        if !hashed {
            return stamp;
        }
        self.content(path.to_string(), stamp, true, recorded)
    }
    /// Returns the stamp of the contents of a file, whose stamp by time and size is `stamp`.
    fn content(&mut self, path: String, stamp: Stamp, source: bool, recorded: Option<(&Stamp, u64)>) -> Stamp {
        let Stamp::File { mtime, size } = stamp else {
            return stamp;
        };
        let unchanged =
            |stamp: &Stamp| matches!(*stamp, Stamp::Content { mtime: m, size: s, .. } if (m, s) == (mtime, size));
        let found = self.contents.get(&path).or_else(|| self.known.get(&path));
        if let Some(&stamp) = found.filter(|stamp| unchanged(stamp)) {
            return stamp;
        }
        let reused = recorded.and_then(|(stamp, stamped)| match *stamp {
            Stamp::Content { hash, .. } if unchanged(stamp) && !(source && racy(mtime, stamped)) => Some(hash),
            _ => None,
        });
        let hash = match reused {
            Some(hash) => hash,
            None => match crate::blake3::hash_file(&self.dir.join(&path)) {
                Ok(hash) => hash,
                // a file which cannot be read is taken as missing, as `Stamp::of` takes it
                Err(_) => return Stamp::Missing,
            },
        };
        let stamp = Stamp::Content { mtime, size, hash };
        self.contents.insert(path, stamp);
        stamp
    }
}

/// The fewest files stamped on threads of their own, below which they are stamped on the calling thread.
const PARALLEL_MIN: usize = 256;

/// Returns the stamps of files, in the same order, found together on several threads if there are enough of them.
#[must_use]
pub fn stamps_of(paths: &[&Path]) -> Vec<Stamp> {
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
    fn reuses_hashes_unless_a_source_is_racy() {
        let dir = std::env::temp_dir().join(format!("rig-stamps-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut graph = Graph::new();
        let source = graph.file("source");
        let path = dir.join("source");
        std::fs::write(&path, "one").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        let hashed = Stamps::new(&graph, &dir).node_as(source, true, None);
        let Stamp::Content { hash, .. } = hashed else { panic!("{hashed:?}") };
        assert_eq!(hash, crate::blake3::hash(b"one"));
        // changed with the same time and size, which only reading it would show, so the recorded hash is reused
        std::fs::write(&path, "two").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        let later = 2_000_000_000;
        assert_eq!(Stamps::new(&graph, &dir).node_as(source, true, Some((&hashed, later))), hashed);
        // unless it was stamped within a tick of its time, when it is read again
        let racy = 1_000_000_000;
        let again = Stamps::new(&graph, &dir).node_as(source, true, Some((&hashed, racy)));
        assert!(!again.same(hashed));
        // and without a recorded hash, as without hashing, it is stamped by its time and size
        assert!(matches!(Stamps::new(&graph, &dir).node_as(source, false, None), Stamp::File { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn takes_what_was_hashed_elsewhere() {
        let dir = std::env::temp_dir().join(format!("rig-stamps-known-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut graph = Graph::new();
        let out = graph.file("out");
        std::fs::write(dir.join("out"), "one").unwrap();
        let Stamp::File { mtime, size } = Stamp::of(&dir.join("out")) else { panic!() };
        // a hash found by the thread which ran its edge stands while the file has the time and size it had then
        let mut stamps = Stamps::new(&graph, &dir);
        let known = Stamp::Content { mtime, size, hash: [7; 32] };
        stamps.know("out", known);
        assert_eq!(stamps.node_as(out, true, None), known);
        // and is not taken once they change
        std::fs::write(dir.join("out"), "other").unwrap();
        stamps.forget(out);
        let Stamp::Content { hash, .. } = stamps.node_as(out, true, None) else { panic!() };
        assert_eq!(hash, crate::blake3::hash(b"other"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamps_what_was_left_on_a_worker() {
        let dir = std::env::temp_dir().join(format!("rig-stamps-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut graph = Graph::new();
        let left = graph.file("left");
        let hash = crate::blake3::hash(b"left");
        let remote = Stamp::Remote { hash, executable: false };
        // while it is missing, it is stamped as it was left
        let mut stamps = Stamps::new(&graph, &dir);
        stamps.extend_remote(HashMap::from([("left".to_string(), remote)]));
        assert_eq!(stamps.node(left), remote);
        // and once it is here, by its file, which is the same by its contents
        std::fs::write(dir.join("left"), "left").unwrap();
        stamps.forget(left);
        let here = stamps.node_as(left, true, None);
        assert!(here.same(remote) && remote.same(here));
        assert!(!stamps.node(left).same(remote));
        assert!(!Stamp::Remote { hash: [0; 32], executable: false }.same(remote));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
