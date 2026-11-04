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
//! A stamp is `0` if the file was missing, or `1` followed by its modification time, as an `i64`, and its size, as a
//! `u64`. Numbers are little-endian, and paths are their length as a `u32` followed by their UTF-8 bytes.

use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use crate::Error;
use crate::stamp::Stamp;

const MAGIC: &[u8; 4] = b"RIGL";
const VERSION: u32 = 2;

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
    /// The outputs, explicit then implicit, once the edge had run. The first is the key of the record.
    pub outputs: Vec<(String, Stamp)>,
    /// The inputs the edge declares, explicit and implicit, with the inputs of aliases in their place, when it started.
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
    log: std::fs::File,
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
        let mut state = Self { dir: dir.into(), log, _lock: lock, records, logged };
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
        if let Err(error) = self.log.write_all(&bytes) {
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
        let written = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &path));
        written.map_err(|error| Error::Io(path.clone(), error))?;
        let log = std::fs::File::options().append(true).open(&path);
        self.log = log.map_err(|error| Error::Io(path, error))?;
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
    let mut contents = Vec::new();
    contents.extend_from_slice(&record.start.to_le_bytes());
    contents.extend_from_slice(&record.duration.to_le_bytes());
    contents.extend_from_slice(&record.action.to_le_bytes());
    for list in [&record.outputs, &record.inputs, &record.discovered] {
        contents.extend_from_slice(&(list.len() as u32).to_le_bytes());
        for (path, stamp) in list {
            contents.extend_from_slice(&(path.len() as u32).to_le_bytes());
            contents.extend_from_slice(path.as_bytes());
            match stamp {
                Stamp::Missing => contents.push(0),
                Stamp::File { mtime, size } => {
                    contents.push(1);
                    contents.extend_from_slice(&mtime.to_le_bytes());
                    contents.extend_from_slice(&size.to_le_bytes());
                },
            }
        }
    }
    contents.extend_from_slice(&(record.env.len() as u32).to_le_bytes());
    for (name, hash) in &record.env {
        contents.extend_from_slice(&(name.len() as u32).to_le_bytes());
        contents.extend_from_slice(name.as_bytes());
        match hash {
            None => contents.push(0),
            Some(hash) => {
                contents.push(1);
                contents.extend_from_slice(&hash.to_le_bytes());
            },
        }
    }
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
    let mut r = Reader { bytes: contents, pos: 0 };
    let start = u64::from_le_bytes(r.take(8)?.try_into().ok()?);
    let duration = u32::from_le_bytes(r.take(4)?.try_into().ok()?);
    let action = u64::from_le_bytes(r.take(8)?.try_into().ok()?);
    let mut lists = [Vec::new(), Vec::new(), Vec::new()];
    for list in &mut lists {
        for _ in 0..r.u32()? {
            let len = r.u32()? as usize;
            let path = std::str::from_utf8(r.take(len)?).ok()?.to_string();
            let stamp = match r.take(1)?[0] {
                0 => Stamp::Missing,
                1 => Stamp::File {
                    mtime: i64::from_le_bytes(r.take(8)?.try_into().ok()?),
                    size: u64::from_le_bytes(r.take(8)?.try_into().ok()?),
                },
                _ => return None,
            };
            list.push((path, stamp));
        }
    }
    let [outputs, inputs, discovered] = lists;
    let mut env = Vec::new();
    for _ in 0..r.u32()? {
        let len = r.u32()? as usize;
        let name = std::str::from_utf8(r.take(len)?).ok()?.to_string();
        let hash = match r.take(1)?[0] {
            0 => None,
            1 => Some(u64::from_le_bytes(r.take(8)?.try_into().ok()?)),
            _ => return None,
        };
        env.push((name, hash));
    }
    let valid = r.pos == contents.len() && !outputs.is_empty();
    valid.then_some(Record { start, duration, action, outputs, inputs, discovered, env })
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let bytes = self.bytes.get(self.pos..self.pos.checked_add(len)?)?;
        self.pos += len;
        Some(bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rig-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(output: &str, start: u64) -> Record {
        Record {
            start,
            duration: 5,
            action: 42,
            outputs: vec![(output.into(), Stamp::File { mtime: -3, size: 7 })],
            inputs: vec![("in".into(), Stamp::File { mtime: 1_700_000_000_000_000_000, size: 0 })],
            discovered: vec![("header.h".into(), Stamp::Missing)],
            env: vec![("SET".into(), Some(7)), ("UNSET".into(), None)],
        }
    }

    #[test]
    fn keeps_records() {
        let dir = dir("keeps");
        let mut state = State::open(&dir).unwrap();
        assert!(state.is_empty());
        state.record(record("a", 1)).unwrap();
        state.record(record("b", 2)).unwrap();
        // a later record of an edge replaces the earlier one
        state.record(record("a", 3)).unwrap();
        drop(state);
        let state = State::open(&dir).unwrap();
        assert_eq!(state.len(), 2);
        assert_eq!(state.get("a"), Some(&record("a", 3)));
        assert_eq!(state.get("b"), Some(&record("b", 2)));
        assert_eq!(state.logged, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_locked_while_open() {
        let dir = dir("locked");
        let state = State::open(&dir).unwrap();
        assert!(matches!(State::open(&dir), Err(Error::Locked(_))));
        drop(state);
        assert!(State::open(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_a_torn_record() {
        let dir = dir("torn");
        let mut state = State::open(&dir).unwrap();
        state.record(record("a", 1)).unwrap();
        state.record(record("b", 2)).unwrap();
        drop(state);
        // as if the build was killed while writing the last record
        let log = dir.join("log");
        let len = std::fs::metadata(&log).unwrap().len();
        std::fs::File::options().write(true).open(&log).unwrap().set_len(len - 3).unwrap();
        let mut state = State::open(&dir).unwrap();
        assert_eq!(state.len(), 1);
        assert!(state.get("b").is_none());
        // which the next record replaces
        state.record(record("c", 3)).unwrap();
        drop(state);
        let state = State::open(&dir).unwrap();
        assert_eq!((state.len(), state.get("c").is_some()), (2, true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn starts_again_with_another_version() {
        let dir = dir("version");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log"), b"RIGL\x01\0\0\0garbage").unwrap();
        let mut state = State::open(&dir).unwrap();
        assert!(state.is_empty());
        state.record(record("a", 1)).unwrap();
        drop(state);
        assert_eq!(State::open(&dir).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compacts() {
        let dir = dir("compacts");
        let mut state = State::open(&dir).unwrap();
        for start in 0..COMPACT_MIN as u64 {
            state.record(record(if start % 2 == 0 { "a" } else { "b" }, start)).unwrap();
        }
        let before = std::fs::metadata(dir.join("log")).unwrap().len();
        drop(state);
        // the log is rewritten when it is opened, as most of its records have been replaced
        let mut state = State::open(&dir).unwrap();
        assert_eq!(state.logged, 2);
        assert!(std::fs::metadata(dir.join("log")).unwrap().len() < before / 100);
        state.record(record("c", 9999)).unwrap();
        drop(state);
        let state = State::open(&dir).unwrap();
        assert_eq!(state.len(), 3);
        assert_eq!(state.get("b").unwrap().start, COMPACT_MIN as u64 - 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
