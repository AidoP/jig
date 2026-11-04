//! The format a [`Graph`] is saved in.
//!
//! It is the magic bytes and the version, then the paths of the nodes, then the edges, then the defaults, followed by a
//! checksum of all that precedes it, the 64-bit FNV-1a hash. Numbers are little-endian `u32`s, strings are their length
//! followed by their UTF-8 bytes, and a list of nodes is its length followed by each node's index.
//!
//! An edge is its kind, `0` for an alias, `1` for a shell command followed by the command, or `2` for a program
//! followed by the program, its arguments as a list of strings, its environment variables as their number followed by
//! each name and value, then its working directory and the file of its stderr, each as `0`, or `1` followed by the
//! path, then its description, its depfile as `0`, or `1` followed by the path, its pool, `0` or `1` for the console,
//! its flags, `1` if it is a generator, `2` if it is restat and `4` if it is hashed, then its lists of nodes: outputs,
//! implicit outputs, inputs, implicit inputs and order-only inputs.

use std::path::Path;

use crate::graph::{Action, Edge, EdgeKind, Exec, Graph, NodeId, Pool};
use crate::Error;

const MAGIC: &[u8; 4] = b"RIGG";
/// The version of the format, which jig's database changes with, so that jig configures a project again, rather than
/// failing to load its graph.
const VERSION: u32 = 3;

const GENERATOR: u32 = 1;
const RESTAT: u32 = 2;
const HASHED: u32 = 4;

impl Graph {
    /// Returns the graph in the format it is saved in.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.u32(VERSION);
        w.len(self.nodes());
        for node in 0..self.nodes() {
            w.str(self.path(NodeId(node as u32)));
        }
        w.len(self.edges().len());
        for edge in self.edges() {
            match edge.kind() {
                EdgeKind::Alias => w.u32(0),
                EdgeKind::Command(Action::Shell(command)) => {
                    w.u32(1);
                    w.str(command);
                },
                EdgeKind::Command(Action::Exec(exec)) => {
                    w.u32(2);
                    w.exec(exec);
                },
            }
            w.str(edge.description());
            match edge.depfile() {
                None => w.u32(0),
                Some(depfile) => {
                    w.u32(1);
                    w.str(depfile);
                },
            }
            w.u32(match edge.pool() {
                Pool::Default => 0,
                Pool::Console => 1,
            });
            let flags = [(edge.generator(), GENERATOR), (edge.restat(), RESTAT), (edge.hashed(), HASHED)];
            w.u32(flags.into_iter().filter(|&(set, _)| set).fold(0, |flags, (_, flag)| flags | flag));
            for list in [edge.outputs(), edge.implicit_outputs(), edge.inputs(), edge.implicit(), edge.order_only()] {
                w.nodes(list);
            }
        }
        w.nodes(self.defaults());
        let checksum = fnv1a(&w.0);
        w.0.extend_from_slice(&checksum.to_le_bytes());
        w.0
    }
    /// Read a graph from the format it is saved in.
    ///
    /// # Errors
    ///
    /// If the bytes are not a graph of this version, or are corrupted.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let Some((body, checksum)) = bytes.split_last_chunk::<8>() else {
            return Err(Error::Format("truncated"));
        };
        if !body.starts_with(MAGIC) {
            return Err(Error::Format("not a graph"));
        }
        if u64::from_le_bytes(*checksum) != fnv1a(body) {
            return Err(Error::Format("checksum mismatch"));
        }
        let mut r = Reader { bytes: body, pos: MAGIC.len() };
        if r.u32()? != VERSION {
            return Err(Error::Format("another version"));
        }
        let mut graph = Graph::new();
        for _ in 0..r.u32()? {
            let path = r.str()?;
            if graph.node(path).is_some() {
                return Err(Error::Format("duplicate path"));
            }
            graph.file(path);
        }
        let nodes = graph.nodes();
        for _ in 0..r.u32()? {
            let kind = match r.u32()? {
                0 => EdgeKind::Alias,
                1 => EdgeKind::Command(Action::Shell(r.str()?.into())),
                2 => EdgeKind::Command(Action::Exec(r.exec()?)),
                _ => return Err(Error::Format("unknown kind of edge")),
            };
            let description = r.str()?.into();
            let depfile = match r.u32()? {
                0 => None,
                1 => Some(r.str()?.into()),
                _ => return Err(Error::Format("invalid depfile")),
            };
            let pool = match r.u32()? {
                0 => Pool::Default,
                1 => Pool::Console,
                _ => return Err(Error::Format("unknown pool")),
            };
            let flags = r.u32()?;
            if flags & !(GENERATOR | RESTAT | HASHED) != 0 {
                return Err(Error::Format("unknown flags"));
            }
            let edge = Edge {
                kind,
                description,
                outputs: r.nodes(nodes)?,
                implicit_outputs: r.nodes(nodes)?,
                inputs: r.nodes(nodes)?,
                implicit: r.nodes(nodes)?,
                order_only: r.nodes(nodes)?,
                depfile,
                pool,
                generator: flags & GENERATOR != 0,
                restat: flags & RESTAT != 0,
                hashed: flags & HASHED != 0,
            };
            graph.add_edge(edge).map_err(|_| Error::Format("an output is produced by more than one edge"))?;
        }
        let defaults = r.nodes(nodes)?;
        graph.add_default(&defaults);
        if r.pos != body.len() {
            return Err(Error::Format("trailing bytes"));
        }
        Ok(graph)
    }
    /// Save the graph to a file, which is replaced once it is written, so that it is never seen half written.
    ///
    /// # Errors
    ///
    /// If the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let tmp = path.with_added_extension("tmp");
        let written = std::fs::write(&tmp, self.to_bytes()).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Io(path.into(), error));
        }
        Ok(())
    }
    /// Load a graph saved by [`Graph::save`].
    ///
    /// # Errors
    ///
    /// If the file cannot be read, or is not a graph of this version.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(|error| Error::Io(path.into(), error))?;
        Self::from_bytes(&bytes)
    }
}

/// The 64-bit FNV-1a hash, which detects a file which is truncated or corrupted.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

struct Writer(Vec<u8>);
impl Writer {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    /// Write a length, which the graph keeps within a `u32`.
    fn len(&mut self, len: usize) {
        self.u32(len as u32);
    }
    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.0.extend_from_slice(s.as_bytes());
    }
    fn optional(&mut self, s: Option<&str>) {
        match s {
            None => self.u32(0),
            Some(s) => {
                self.u32(1);
                self.str(s);
            },
        }
    }
    fn exec(&mut self, exec: &Exec) {
        self.str(&exec.program);
        self.len(exec.args.len());
        for arg in &exec.args {
            self.str(arg);
        }
        self.len(exec.env.len());
        for (name, value) in &exec.env {
            self.str(name);
            self.str(value);
        }
        self.optional(exec.dir.as_deref());
        self.optional(exec.stderr.as_deref());
    }
    fn nodes(&mut self, nodes: &[NodeId]) {
        self.len(nodes.len());
        for node in nodes {
            self.u32(node.0);
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let bytes = self.bytes.get(self.pos..self.pos.saturating_add(len)).ok_or(Error::Format("truncated"))?;
        self.pos += len;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    fn str(&mut self) -> Result<&'a str, Error> {
        let len = self.u32()? as usize;
        std::str::from_utf8(self.take(len)?).map_err(|_| Error::Format("invalid UTF-8"))
    }
    fn optional(&mut self) -> Result<Option<String>, Error> {
        match self.u32()? {
            0 => Ok(None),
            1 => Ok(Some(self.str()?.into())),
            _ => Err(Error::Format("invalid optional string")),
        }
    }
    fn exec(&mut self) -> Result<Exec, Error> {
        let program = self.str()?.into();
        let args = (0..self.u32()?).map(|_| self.str().map(String::from)).collect::<Result<_, _>>()?;
        let env = (0..self.u32()?).map(|_| Ok((self.str()?.into(), self.str()?.into()))).collect::<Result<_, Error>>()?;
        Ok(Exec { program, args, env, dir: self.optional()?, stderr: self.optional()? })
    }
    /// Read a list of nodes, each of which must be one of the `nodes` of the graph.
    fn nodes(&mut self, nodes: usize) -> Result<Vec<NodeId>, Error> {
        let len = self.u32()? as usize;
        // each node takes 4 bytes, so a corrupted length does not allocate more than the file holds
        let mut list = Vec::with_capacity(len.min(self.bytes.len() / 4));
        for _ in 0..len {
            let node = self.u32()?;
            if node as usize >= nodes {
                return Err(Error::Format("node out of range"));
            }
            list.push(NodeId(node));
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Graph {
        let mut graph = Graph::new();
        let (a, b, c, d) = (graph.file("a"), graph.file("b c"), graph.file("c.d"), graph.file("d"));
        graph
            .add_edge(Edge {
                kind: EdgeKind::Command(Action::Shell("cc -o a b\\ c".into())),
                description: "CC a".into(),
                outputs: vec![a],
                implicit_outputs: vec![c],
                inputs: vec![b],
                order_only: vec![d],
                depfile: Some("a.d".into()),
                pool: Pool::Console,
                generator: true,
                restat: true,
                hashed: true,
                ..Edge::default()
            })
            .unwrap();
        graph.add_alias("all", &[a]).unwrap();
        let exec = Exec {
            program: "cc".into(),
            args: vec!["-o".into(), "e f".into()],
            env: vec![("A".into(), "b".into())],
            dir: Some("..".into()),
            stderr: None,
        };
        let program = graph.file("e f");
        let kind = EdgeKind::Command(Action::Exec(exec));
        graph.add_edge(Edge { kind, outputs: vec![program], ..Edge::default() }).unwrap();
        graph.add_default(&[a, d]);
        graph
    }

    #[test]
    fn round_trips() {
        let graph = graph();
        let read = Graph::from_bytes(&graph.to_bytes()).unwrap();
        assert_eq!(read, graph);
    }

    #[test]
    fn refuses_invalid_graphs() {
        let bytes = graph().to_bytes();
        let error = |bytes: &[u8]| match Graph::from_bytes(bytes) {
            Err(Error::Format(message)) => message,
            result => panic!("{result:?}"),
        };
        assert_eq!(error(&bytes[..4]), "truncated");
        assert_eq!(error(&bytes[..bytes.len() - 1]), "checksum mismatch");
        let mut corrupted = bytes.clone();
        corrupted[10] ^= 1;
        assert_eq!(error(&corrupted), "checksum mismatch");
        let mut other = bytes[..bytes.len() - 8].to_vec();
        other[4] = 9;
        let checksum = fnv1a(&other);
        other.extend_from_slice(&checksum.to_le_bytes());
        assert_eq!(error(&other), "another version");
        assert_eq!(error(b"NOPE\x01\0\0\0\0\0\0\0\0\0\0\0"), "not a graph");
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("rig-format-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("graph");
        let graph = graph();
        graph.save(&path).unwrap();
        assert_eq!(Graph::load(&path).unwrap(), graph);
        assert!(!path.with_added_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
