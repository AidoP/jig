//! The state of a build: what each edge was last run with, which tells whether it must run again.
//!
//! The state is a log in a directory, `log`, to which a record is appended as soon as an edge has run, so that an
//! interrupted build loses only the edges which were running. A later record of an edge replaces an earlier one. The
//! log is rewritten without the records which have been replaced once they are most of it. `lock` in the same
//! directory is locked while the state is open, so that two builds do not share it.
//!
//! The log is the magic bytes and the version, then each record, as its length as a `u32` and the 64-bit FNV-1a hash of
//! its contents, followed by its contents. A record which is cut short or does not match its hash, as when the build
//! was killed while writing it, ends the log, and is overwritten by the next record. A log of another version is
//! started again.
//!
//! A record is the time the edge started and how long it ran, in milliseconds, the hash of its action, then its
//! outputs, its declared inputs and the inputs it discovered, each as their number followed by each path and its stamp,
//! then the environment variables it read, as their number followed by each name, and `0` if it was not set, or `1`
//! followed by the hash of its value, as a `u64`.
//! A stamp is `0` if the file was missing, `1` followed by its modification time, as an `i64`, and its size, as a
//! `u64`, `2` followed by those and the BLAKE3 hash of its contents, or `3`, for an output left on a worker, followed
//! by its hash, and `1` if it is executable or `0` if not. Numbers are little-endian, and paths are their
//! length as a `u32` followed by their UTF-8 bytes.

use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use crate::Error;
use crate::bytes::{Reader, Writer};
use crate::stamp::Stamp;

const MAGIC: &[u8; 4] = b"RIGL";
const VERSION: u32 = 4;

/// The fewest records the log has before it is rewritten without those which have been replaced.
const COMPACT_MIN: usize = 1024;

/// What an edge was last run with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// When the edge started, in milliseconds since the Unix epoch.
    pub start: u64,
    /// How long the edge ran, in milliseconds.
    pub duration: u32,
    /// The hash of the edge's action, as [`crate::plan::action_hash`] finds it.
    pub action: u64,
    /// The outputs, once the edge had run. The first is the key of the record.
    pub outputs: Vec<(String, Stamp)>,
    /// The inputs and dependencies the edge declares, with those of aliases in their place, when it started.
    pub inputs: Vec<(String, Stamp)>,
    /// The inputs the edge reported in its depfile, once it had run.
    pub discovered: Vec<(String, Stamp)>,
    /// The environment variables the edge reported reading in its depfile, with the hash of each value it saw, or
    /// `None` if it was not set.
    pub env: Vec<(String, Option<u64>)>,
}
impl Record {
    /// The key of the record, its first output, as the edge is found by.
    #[must_use]
    pub fn key(&self) -> &str {
        self.outputs.first().map_or("", |(path, _)| path)
    }
}

/// The state of a build, which is locked while it is open.
pub struct State {
    dir: PathBuf,
    /// The log, which is open but while it is replaced as it is compacted, as Windows does not replace a file which
    /// is open.
    log: Option<std::fs::File>,
    /// The lock file, which is held while the state is open.
    _lock: std::fs::File,
    records: HashMap<String, Record>,
    /// The number of records in the log, including those which have been replaced.
    logged: usize,
}
impl State {
    /// Open the state in a directory, creating it if it does not exist.
    ///
    /// # Errors
    ///
    /// If the directory cannot be created, the log cannot be read or written, or the state is open elsewhere.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
        let lock_path = dir.join("lock");
        let lock = std::fs::File::options().create(true).truncate(false).write(true).open(&lock_path);
        let lock = lock.map_err(io(&lock_path))?;
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Locked(dir.into())),
            Err(std::fs::TryLockError::Error(error)) => return Err(Error::Io(lock_path, error)),
        }
        let path = dir.join("log");
        let log = std::fs::File::options().create(true).truncate(false).read(true).write(true).open(&path);
        let mut log = log.map_err(io(&path))?;
        let bytes = std::fs::read(&path).map_err(io(&path))?;
        let (records, logged, end) = read_log(&bytes);
        // a log of another version is started again, and one which is cut short is written again from where it is
        // valid
        let header = [MAGIC.as_slice(), &VERSION.to_le_bytes()].concat();
        let reset = if end == 0 {
            log.set_len(0).and_then(|()| log.write_all(&header))
        } else {
            log.set_len(end as u64)
        };
        reset.and_then(|()| log.seek(std::io::SeekFrom::End(0))).map_err(io(&path))?;
        let mut state = Self { dir: dir.into(), log: Some(log), _lock: lock, records, logged };
        if state.logged >= COMPACT_MIN && state.logged > state.records.len() * 2 {
            state.compact()?;
        }
        Ok(state)
    }
    /// Returns the record of an edge, by its first output.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Record> {
        self.records.get(key)
    }
    /// Returns the outputs left on workers, by path, with their stamps, as their edges' records have them.
    #[must_use]
    pub fn remote_outputs(&self) -> HashMap<String, Stamp> {
        let outputs = self.records.values().flat_map(|record| &record.outputs);
        outputs.filter(|(_, stamp)| stamp.is_remote()).map(|(path, stamp)| (path.clone(), *stamp)).collect()
    }
    /// Returns the number of edges with a record.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    /// Append a record, which replaces any of the same edge.
    ///
    /// # Errors
    ///
    /// If the log cannot be written.
    pub fn record(&mut self, record: Record) -> Result<(), Error> {
        let mut bytes = Vec::new();
        write_record(&mut bytes, &record);
        let closed = || std::io::Error::other("the log is closed");
        if let Err(error) = self.log.as_mut().ok_or_else(closed).and_then(|log| log.write_all(&bytes)) {
            return Err(Error::Io(self.dir.join("log"), error));
        }
        self.logged += 1;
        self.records.insert(record.key().into(), record);
        Ok(())
    }
    /// Rewrite the log with only the records which have not been replaced.
    ///
    /// # Errors
    ///
    /// If the log cannot be written.
    pub fn compact(&mut self) -> Result<(), Error> {
        let path = self.dir.join("log");
        let tmp = self.dir.join("log.tmp");
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        let mut records: Vec<_> = self.records.values().collect();
        // in the order they were run, so that the log is the same for the same records
        records.sort_by(|lhs, rhs| (lhs.start, lhs.key()).cmp(&(rhs.start, rhs.key())));
        for record in records {
            write_record(&mut bytes, record);
        }
        std::fs::write(&tmp, &bytes).map_err(|error| Error::Io(tmp.clone(), error))?;
        // the log is closed while it is replaced, and opened again, at its end, whether it was replaced or not
        self.log = None;
        let renamed = std::fs::rename(&tmp, &path);
        let log = std::fs::File::options().append(true).open(&path);
        self.log = Some(log.map_err(|error| Error::Io(path.clone(), error))?);
        renamed.map_err(|error| Error::Io(path, error))?;
        self.logged = self.records.len();
        Ok(())
    }
}

/// Returns the error of a file which could not be read or written.
fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |error| Error::Io(path.to_path_buf(), error)
}

/// The 64-bit FNV-1a hash.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

fn write_record(bytes: &mut Vec<u8>, record: &Record) {
    let mut w = Writer::new();
    w.u64(record.start);
    w.u32(record.duration);
    w.u64(record.action);
    for list in [&record.outputs, &record.inputs, &record.discovered] {
        w.len(list.len());
        for (path, stamp) in list {
            w.str(path);
            match stamp {
                Stamp::Missing => w.u8(0),
                Stamp::File { mtime, size } => {
                    w.u8(1);
                    w.i64(*mtime);
                    w.u64(*size);
                },
                Stamp::Content { mtime, size, hash } => {
                    w.u8(2);
                    w.i64(*mtime);
                    w.u64(*size);
                    w.bytes(hash);
                },
                Stamp::Remote { hash, executable } => {
                    w.u8(3);
                    w.bytes(hash);
                    w.u8(u8::from(*executable));
                },
            }
        }
    }
    w.len(record.env.len());
    for (name, hash) in &record.env {
        w.str(name);
        match hash {
            None => w.u8(0),
            Some(hash) => {
                w.u8(1);
                w.u64(*hash);
            },
        }
    }
    let contents = w.0;
    bytes.extend_from_slice(&(contents.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&fnv1a(&contents).to_le_bytes());
    bytes.extend_from_slice(&contents);
}

/// Read the records of a log, the number of them including those replaced, and the length of the log which is valid,
/// which is `0` if it is not a log of this version.
fn read_log(bytes: &[u8]) -> (HashMap<String, Record>, usize, usize) {
    let mut records = HashMap::new();
    let header = MAGIC.len() + 4;
    if bytes.len() < header || &bytes[..4] != MAGIC || bytes[4..8] != VERSION.to_le_bytes() {
        return (records, 0, 0);
    }
    let mut pos = header;
    let mut logged = 0;
    while let Some(head) = bytes.get(pos..pos + 12) {
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let hash = u64::from_le_bytes(head[4..12].try_into().unwrap_or_default());
        let Some(contents) = bytes.get(pos + 12..pos + 12 + len) else {
            break;
        };
        let Some(record) = (fnv1a(contents) == hash).then(|| read_record(contents)).flatten() else {
            break;
        };
        records.insert(record.key().to_string(), record);
        logged += 1;
        pos += 12 + len;
    }
    (records, logged, pos)
}

fn read_record(contents: &[u8]) -> Option<Record> {
    let mut r = Reader(contents);
    let (start, duration, action) = (r.u64()?, r.u32()?, r.u64()?);
    let mut entry = |r: &mut Reader| {
        let path = r.string()?;
        let stamp = match r.u8()? {
            0 => Stamp::Missing,
            1 => Stamp::File { mtime: r.i64()?, size: r.u64()? },
            2 => Stamp::Content { mtime: r.i64()?, size: r.u64()?, hash: r.hash()? },
            3 => Stamp::Remote { hash: r.hash()?, executable: r.u8()? == 1 },
            _ => return None,
        };
        Some((path, stamp))
    };
    let (outputs, inputs, discovered) = (r.list(&mut entry)?, r.list(&mut entry)?, r.list(&mut entry)?);
    let env = r.list(|r| {
        let name = r.string()?;
        let hash = match r.u8()? {
            0 => None,
            1 => Some(r.u64()?),
            _ => return None,
        };
        Some((name, hash))
    })?;
    let valid = r.is_empty() && !outputs.is_empty();
    valid.then_some(Record { start, duration, action, outputs, inputs, discovered, env })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rig-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Open the state in a directory, which the test's other threads can hold for a moment after this one closed it,
    /// as a process which one of them is starting has a copy of every descriptor until it runs its program, the lock's
    /// among them.
    fn open(dir: &Path) -> State {
        let mut tries = 0;
        loop {
            match State::open(dir) {
                Err(Error::Locked(_)) if tries < 100 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                },
                state => return state.unwrap(),
            }
        }
    }

    fn record(output: &str, start: u64) -> Record {
        Record {
            start,
            duration: 5,
            action: 42,
            outputs: vec![
                (output.into(), Stamp::File { mtime: -3, size: 7 }),
                (format!("{output}.left"), Stamp::Remote { hash: [9; 32], executable: true }),
            ],
            inputs: vec![("in".into(), Stamp::File { mtime: 1_700_000_000_000_000_000, size: 0 })],
            discovered: vec![("header.h".into(), Stamp::Missing), ("hashed".into(), Stamp::Content {
                mtime: 5,
                size: 6,
                hash: [7; 32],
            })],
            env: vec![("SET".into(), Some(7)), ("UNSET".into(), None)],
        }
    }

    #[test]
    fn keeps_records() {
        let dir = dir("keeps");
        let mut state = open(&dir);
        assert!(state.is_empty());
        state.record(record("a", 1)).unwrap();
        state.record(record("b", 2)).unwrap();
        // a later record of an edge replaces the earlier one
        state.record(record("a", 3)).unwrap();
        drop(state);
        let state = open(&dir);
        assert_eq!(state.len(), 2);
        assert_eq!(state.get("a"), Some(&record("a", 3)));
        assert_eq!(state.get("b"), Some(&record("b", 2)));
        assert_eq!(state.logged, 3);
        let left = Stamp::Remote { hash: [9; 32], executable: true };
        let remote = state.remote_outputs();
        assert_eq!(remote.len(), 2);
        assert_eq!(remote.get("a.left"), Some(&left));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_locked_while_open() {
        let dir = dir("locked");
        let state = open(&dir);
        assert!(matches!(State::open(&dir), Err(Error::Locked(_))));
        drop(state);
        let _ = open(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_a_torn_record() {
        let dir = dir("torn");
        let mut state = open(&dir);
        state.record(record("a", 1)).unwrap();
        state.record(record("b", 2)).unwrap();
        drop(state);
        // as if the build was killed while writing the last record
        let log = dir.join("log");
        let len = std::fs::metadata(&log).unwrap().len();
        std::fs::File::options().write(true).open(&log).unwrap().set_len(len - 3).unwrap();
        let mut state = open(&dir);
        assert_eq!(state.len(), 1);
        assert!(state.get("b").is_none());
        // which the next record replaces
        state.record(record("c", 3)).unwrap();
        drop(state);
        let state = open(&dir);
        assert_eq!((state.len(), state.get("c").is_some()), (2, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn starts_again_with_another_version() {
        let dir = dir("version");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), b"RIGL\x01\0\0\0garbage").unwrap();
        let mut state = open(&dir);
        assert!(state.is_empty());
        state.record(record("a", 1)).unwrap();
        drop(state);
        assert_eq!(open(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compacts() {
        let dir = dir("compacts");
        let mut state = open(&dir);
        for start in 0..COMPACT_MIN as u64 {
            state.record(record(if start % 2 == 0 { "a" } else { "b" }, start)).unwrap();
        }
        let before = std::fs::metadata(dir.join("log")).unwrap().len();
        drop(state);
        // the log is rewritten when it is opened, as most of its records have been replaced
        let mut state = open(&dir);
        assert_eq!(state.logged, 2);
        assert!(std::fs::metadata(dir.join("log")).unwrap().len() < before / 100);
        state.record(record("c", 9999)).unwrap();
        drop(state);
        let state = open(&dir);
        assert_eq!(state.len(), 3);
        assert_eq!(state.get("b").unwrap().start, COMPACT_MIN as u64 - 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
