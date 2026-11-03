//! rig, a build engine.
//!
//! A build is described by a [`Graph`] of files, which are its nodes, and the [`Edge`]s which produce them from others
//! by running an [`Action`]. A graph is built through its API, or read from a ninja file with [`ninja::read`], and can
//! be saved and loaded again in a compact binary format, so that what describes the build need not run again.
//!
//! What each edge last ran with is kept in a [`state::State`], from which [`plan::plan`] finds the edges which must run
//! to bring targets up to date, and why, by comparing the [`Stamp`]s of their files with those recorded.
//!
//! rig has no dependencies other than `std`.

mod format;
mod graph;
pub mod ninja;
pub mod plan;
mod stamp;
pub mod state;

pub use graph::{Action, Edge, EdgeId, EdgeKind, EdgeRef, Graph, NodeId, Pool};
pub use stamp::{Stamp, Stamps};

/// An error from building, saving or loading a graph, or reading a ninja file.
#[derive(Debug)]
pub enum Error {
    /// Two edges produce the same file.
    DuplicateOutput(String),
    /// The graph has a cycle, given as the paths along it, starting and ending with the same path.
    Cycle(Vec<String>),
    /// A saved graph which is not valid, such as one of another version, or corrupted.
    Format(&'static str),
    /// A file which could not be read or written.
    Io(std::path::PathBuf, std::io::Error),
    /// A ninja file, or a value or binding of one, which is not valid, or uses what rig does not support.
    Ninja { line: usize, message: String },
    /// A target which is not in the graph.
    UnknownTarget(String),
    /// An input which no edge produces, which is missing, with the first output of the edge which needs it.
    MissingInput { input: String, needed_by: Option<String> },
    /// The state of a build in a directory which another build has open.
    Locked(std::path::PathBuf),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::DuplicateOutput(path) => write!(f, "{path:?} is produced by more than one edge"),
            Self::Cycle(paths) => write!(f, "dependency cycle: {}", paths.join(" -> ")),
            Self::Format(message) => write!(f, "invalid graph: {message}"),
            Self::Io(path, error) => write!(f, "{path:?}: {error}"),
            Self::Ninja { line: 0, message } => write!(f, "{message}"),
            Self::Ninja { line, message } => write!(f, "line {line}: {message}"),
            Self::UnknownTarget(path) => write!(f, "unknown target {path:?}"),
            Self::MissingInput { input, needed_by: Some(output) } => {
                write!(f, "{input:?}, needed by {output:?}, is missing and no edge produces it")
            },
            Self::MissingInput { input, needed_by: None } => write!(f, "{input:?} is missing and no edge produces it"),
            Self::Locked(dir) => write!(f, "the build state in {dir:?} is in use by another build"),
        }
    }
}
impl std::error::Error for Error {}
