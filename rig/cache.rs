//! A cache of the results of edges, shared between builds, so that an edge which has already run with the same action
//! and inputs, in any build which shares the cache, has its outputs fetched rather than running again.
//!
//! The cache is a directory of three stores:
//!
//! - `cas`, the blobs, which are files named by the BLAKE3 hash of their contents, in a directory named by the first
//!   two hexadecimal digits of it: the outputs of the edges, and what their commands printed.
//! - `mf`, the manifests, by the first key of an edge, each of which lists the sets of inputs and environment
//!   variables the edge discovered, as its depfile lists them, when it ran with that key.
//! - `ac`, the results, by the second key of an edge, each of which lists the edge's outputs, with the blob of each,
//!   and the blob of what its command printed.
//!
//! An edge is found by two keys, as the inputs it discovers are known only once it has run, as ccache's manifests find
//! a compilation: the first, by [`first_key`], is of its action and the contents of the inputs it declares, and leads
//! to the sets of inputs it discovered when it ran with them before. The second, by [`second_key`], is of the first and
//! the contents of the inputs of one of those sets, as they are now, and leads to a result.
//!
//! A file is written to `tmp` and renamed into place, so that a build which reads the cache while another writes to
//! it never sees a file half written, and builds need not lock it. A blob is checked against its hash when it is read,
//! and a manifest or result against the hash which ends it, so that a cache which is corrupted is a miss, and the file
//! is removed, rather than a build which is wrong.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Error;
use crate::blake3::{self, Hasher};

/// The hash of a blob, or a key.
pub type Hash = [u8; 32];

const MAGIC: &[u8; 4] = b"RIGC";
const VERSION: u32 = 1;
const MANIFEST: u8 = 1;
const RESULT: u8 = 2;

/// The most sets of discovered inputs a manifest keeps, the latest first, so that one which is looked up is not read
/// in full after many changes.
const MANIFEST_MAX: usize = 16;

/// The files written to `tmp`, so that each is named differently from others the process writes at once.
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// The cache in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cache {
    dir: PathBuf,
}

/// The inputs an edge discovered when it ran, as its depfile listed them: files, and environment variables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub inputs: Vec<String>,
    pub env: Vec<String>,
}

/// What an edge produced when it ran, in the blobs of the cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub outputs: Vec<Output>,
    /// The blob of what the edge's command printed, stdout and stderr together.
    pub printed: Hash,
}

/// An output of an edge, relative to the build's directory, with the blob of its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub path: String,
    pub blob: Hash,
    pub executable: bool,
}

impl Cache {
    /// Open the cache in a directory, creating it if it does not exist.
    ///
    /// # Errors
    ///
    /// If its directories cannot be created.
    pub fn open(dir: &Path) -> Result<Self, Error> {
        for store in ["cas", "mf", "ac", "tmp"] {
            let path = dir.join(store);
            std::fs::create_dir_all(&path).map_err(|error| Error::Io(path, error))?;
        }
        Ok(Self { dir: dir.into() })
    }
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hex(hash);
        self.dir.join("cas").join(&hex[..2]).join(&hex[2..])
    }
    fn keyed(&self, store: &str, key: &Hash) -> PathBuf {
        self.dir.join(store).join(hex(key))
    }
    /// Returns a path in `tmp` which no other file which is written at once has.
    fn tmp(&self) -> PathBuf {
        let count = WRITTEN.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        self.dir.join("tmp").join(format!("{}-{count}-{nanos}", std::process::id()))
    }
    /// Move a file written to `tmp` into place, creating its directory if it does not exist.
    fn place(tmp: &Path, path: &Path) -> std::io::Result<()> {
        let renamed = std::fs::rename(tmp, path).or_else(|error| match path.parent() {
            Some(parent) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(tmp, path))
            },
            _ => Err(error),
        });
        if renamed.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        renamed
    }
    /// Whether the cache has a blob.
    #[must_use]
    pub fn has_blob(&self, hash: &Hash) -> bool {
        self.blob_path(hash).is_file()
    }
    /// Add a file to the cache as a blob, whose contents have the hash, unless the cache has it.
    ///
    /// # Errors
    ///
    /// If the file cannot be copied into the cache, or its contents do not have the hash, as it changed since it was
    /// hashed.
    pub fn put_file(&self, path: &Path, hash: &Hash) -> std::io::Result<()> {
        if self.has_blob(hash) {
            return Ok(());
        }
        let tmp = self.tmp();
        copy(path, &tmp)?;
        if blake3::hash_file(&tmp)? != *hash {
            let _ = std::fs::remove_file(&tmp);
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{path:?} changed")));
        }
        Self::place(&tmp, &self.blob_path(hash))
    }
    /// Add bytes to the cache as a blob, returning its hash.
    ///
    /// # Errors
    ///
    /// If the blob cannot be written.
    pub fn put_bytes(&self, bytes: &[u8]) -> std::io::Result<Hash> {
        let hash = blake3::hash(bytes);
        if !self.has_blob(&hash) {
            let tmp = self.tmp();
            std::fs::write(&tmp, bytes)?;
            Self::place(&tmp, &self.blob_path(&hash))?;
        }
        Ok(hash)
    }
    /// Returns the contents of a blob, checked against its hash.
    ///
    /// # Errors
    ///
    /// If the cache does not have the blob, or it is corrupted, when it is removed.
    pub fn read_blob(&self, hash: &Hash) -> std::io::Result<Vec<u8>> {
        let path = self.blob_path(hash);
        let bytes = std::fs::read(&path)?;
        if blake3::hash(&bytes) != *hash {
            let _ = std::fs::remove_file(&path);
            return Err(corrupted(&path));
        }
        Ok(bytes)
    }
    /// Write a blob to a file, replacing it, once the copy is checked against its hash, by a reflink where the file
    /// system can, and otherwise by copying it.
    ///
    /// # Errors
    ///
    /// If the cache does not have the blob, it is corrupted, when it is removed, or the file cannot be written.
    pub fn restore(&self, hash: &Hash, path: &Path, executable: bool) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let blob = self.blob_path(hash);
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".rig-tmp");
        let tmp = PathBuf::from(tmp);
        let restored = copy(&blob, &tmp).and_then(|()| {
            if blake3::hash_file(&tmp)? != *hash {
                let _ = std::fs::remove_file(&blob);
                return Err(corrupted(&blob));
            }
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
            std::fs::rename(&tmp, path)
        });
        if restored.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        restored
    }
    /// Check every blob of a result against its hash, removing those which are corrupted.
    pub fn check(&self, entry: &Entry) {
        for blob in std::iter::once(&entry.printed).chain(entry.outputs.iter().map(|output| &output.blob)) {
            let path = self.blob_path(blob);
            if blake3::hash_file(&path).is_ok_and(|hash| hash != *blob) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    /// Returns the sets of inputs an edge discovered when it ran with a first key, the latest first.
    #[must_use]
    pub fn manifest(&self, key: &Hash) -> Vec<Discovery> {
        read(MANIFEST, &self.keyed("mf", key)).and_then(|bytes| read_manifest(&bytes)).unwrap_or_default()
    }
    /// Add a set of inputs which an edge discovered when it ran with a first key to its manifest, as the latest.
    ///
    /// A build which adds to the same manifest at once may replace this one, which loses one set or the other, and
    /// so makes a miss rather than a build which is wrong.
    ///
    /// # Errors
    ///
    /// If the manifest cannot be written.
    pub fn add_to_manifest(&self, key: &Hash, discovery: Discovery) -> std::io::Result<()> {
        let mut manifest = self.manifest(key);
        if manifest.first() == Some(&discovery) {
            return Ok(());
        }
        manifest.retain(|other| *other != discovery);
        manifest.insert(0, discovery);
        manifest.truncate(MANIFEST_MAX);
        let mut w = Writer::new(MANIFEST);
        w.len(manifest.len());
        for discovery in &manifest {
            w.strs(&discovery.inputs);
            w.strs(&discovery.env);
        }
        self.write(&self.keyed("mf", key), w)
    }
    /// Returns the result of an edge by its second key, if the cache has it and every blob it needs.
    #[must_use]
    pub fn result(&self, key: &Hash) -> Option<Entry> {
        let entry = read_result(&read(RESULT, &self.keyed("ac", key))?)?;
        let blobs = std::iter::once(&entry.printed).chain(entry.outputs.iter().map(|output| &output.blob));
        blobs.into_iter().all(|blob| self.has_blob(blob)).then_some(entry)
    }
    /// Add the result of an edge by its second key, once its blobs have been added.
    ///
    /// # Errors
    ///
    /// If the result cannot be written.
    pub fn put_result(&self, key: &Hash, entry: &Entry) -> std::io::Result<()> {
        let mut w = Writer::new(RESULT);
        w.0.extend_from_slice(&entry.printed);
        w.len(entry.outputs.len());
        for output in &entry.outputs {
            w.str(&output.path);
            w.0.extend_from_slice(&output.blob);
            w.0.push(u8::from(output.executable));
        }
        self.write(&self.keyed("ac", key), w)
    }
    fn write(&self, path: &Path, w: Writer) -> std::io::Result<()> {
        let mut bytes = w.0;
        let hash = blake3::hash(&bytes);
        bytes.extend_from_slice(&hash);
        let tmp = self.tmp();
        std::fs::write(&tmp, &bytes)?;
        Self::place(&tmp, path)
    }
}

/// Returns the body of a manifest or result, which is removed if it is corrupted.
fn read(kind: u8, path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let header = MAGIC.len() + 5;
    let valid = bytes.len() >= header + 32 && bytes.starts_with(MAGIC) && bytes[4..8] == VERSION.to_le_bytes();
    let (body, hash) = bytes.split_at(bytes.len().saturating_sub(32));
    if !valid || bytes[8] != kind || blake3::hash(body) != hash {
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(body[header..].to_vec())
}

fn corrupted(path: &Path) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{path:?} is corrupted"))
}

/// Copy a file, by a reflink where the file system can, which shares its contents until one of them is written.
fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = std::fs::File::open(from)?;
    let target = std::fs::File::create(to)?;
    if crate::sys::clone_file(&source, &target).is_ok() {
        return Ok(());
    }
    drop(target);
    std::fs::copy(from, to).map(|_| ())
}

/// Returns a hash in hexadecimal.
#[must_use]
pub fn hex(hash: &Hash) -> String {
    use std::fmt::Write;
    hash.iter().fold(String::with_capacity(64), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// A key, from parts which are each hashed with their length before them, so that parts which run together hash
/// differently.
pub struct Key(Hasher);
impl Key {
    /// A key of a kind, which keys of other kinds never equal.
    #[must_use]
    pub fn new(kind: &str) -> Self {
        let mut key = Self(Hasher::new());
        key.part(kind.as_bytes());
        key
    }
    pub fn part(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }
    /// Add a hash, or that there is none, as of a file which is missing.
    pub fn hash(&mut self, hash: Option<&Hash>) -> &mut Self {
        match hash {
            Some(hash) => self.part(hash),
            None => self.part(b"\0missing"),
        }
    }
    #[must_use]
    pub fn finish(&self) -> Hash {
        self.0.finalize()
    }
}

/// Returns the first key of an edge: of what identifies it in the cache, its action, as
/// [`crate::plan::action_bytes`] gives it, the variables every command is given, its outputs and depfile, and the
/// contents of the inputs it declares, by path, or that they are missing.
#[must_use]
pub fn first_key(
    identity: &str,
    action: &[u8],
    env: &[(String, String)],
    outputs: &[&str],
    depfile: Option<&str>,
    inputs: &[(&str, Option<&Hash>)],
) -> Hash {
    let mut key = Key::new("rig first key");
    key.part(identity.as_bytes()).part(action);
    key.part(&(env.len() as u64).to_le_bytes());
    for (name, value) in env {
        key.part(name.as_bytes()).part(value.as_bytes());
    }
    key.part(&(outputs.len() as u64).to_le_bytes());
    for output in outputs {
        key.part(output.as_bytes());
    }
    key.part(depfile.unwrap_or("\0").as_bytes());
    key.part(&(inputs.len() as u64).to_le_bytes());
    for (path, hash) in inputs {
        key.part(path.as_bytes()).hash(*hash);
    }
    key.finish()
}

/// Returns the second key of an edge: of its first key, and the contents of the inputs it discovered, by path, and the
/// hashes of the values of the environment variables it discovered, or that each is missing or unset.
#[must_use]
pub fn second_key(first: &Hash, inputs: &[(&str, Option<&Hash>)], env: &[(&str, Option<&Hash>)]) -> Hash {
    let mut key = Key::new("rig second key");
    key.part(first);
    for list in [inputs, env] {
        key.part(&(list.len() as u64).to_le_bytes());
        for (name, hash) in list {
            key.part(name.as_bytes()).hash(*hash);
        }
    }
    key.finish()
}

struct Writer(Vec<u8>);
impl Writer {
    fn new(kind: u8) -> Self {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.push(kind);
        Self(bytes)
    }
    fn len(&mut self, len: usize) {
        self.0.extend_from_slice(&(len as u32).to_le_bytes());
    }
    fn str(&mut self, string: &str) {
        self.len(string.len());
        self.0.extend_from_slice(string.as_bytes());
    }
    fn strs(&mut self, strings: &[String]) {
        self.len(strings.len());
        for string in strings {
            self.str(string);
        }
    }
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
    fn len(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }
    fn str(&mut self) -> Option<String> {
        let len = self.len()?;
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_string())
    }
    fn strs(&mut self) -> Option<Vec<String>> {
        (0..self.len()?).map(|_| self.str()).collect()
    }
    fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
    fn end(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

fn read_manifest(bytes: &[u8]) -> Option<Vec<Discovery>> {
    let mut r = Reader { bytes, pos: 0 };
    let manifest = (0..r.len()?).map(|_| Some(Discovery { inputs: r.strs()?, env: r.strs()? }));
    let manifest = manifest.collect::<Option<Vec<_>>>()?;
    r.end().then_some(manifest)
}

fn read_result(bytes: &[u8]) -> Option<Entry> {
    let mut r = Reader { bytes, pos: 0 };
    let printed = r.hash()?;
    let outputs = (0..r.len()?).map(|_| {
        let (path, blob) = (r.str()?, r.hash()?);
        let executable = match r.take(1)?[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Output { path, blob, executable })
    });
    let outputs = outputs.collect::<Option<Vec<_>>>()?;
    r.end().then_some(Entry { outputs, printed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rig-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn keeps_blobs_and_checks_them() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir("blobs");
        let cache = Cache::open(&dir.join("cache")).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, "contents").unwrap();
        let hash = blake3::hash(b"contents");
        cache.put_file(&file, &hash).unwrap();
        assert!(cache.has_blob(&hash));
        // a file which does not have the hash it was given is not added
        let wrong = blake3::hash(b"other");
        assert!(cache.put_file(&file, &wrong).is_err());
        assert!(!cache.has_blob(&wrong));
        // restored by replacing the file, executable or not
        let restored = dir.join("restored");
        std::fs::write(&restored, "old").unwrap();
        cache.restore(&hash, &restored, true).unwrap();
        assert_eq!(std::fs::read_to_string(&restored).unwrap(), "contents");
        assert_eq!(std::fs::metadata(&restored).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(cache.read_blob(&cache.put_bytes(b"printed").unwrap()).unwrap(), b"printed");
        // a blob which is corrupted is not restored, and is removed, leaving the file as it was
        std::fs::write(cache.blob_path(&hash), "corrupted").unwrap();
        assert!(cache.restore(&hash, &restored, false).is_err());
        assert!(!cache.has_blob(&hash));
        assert_eq!(std::fs::read_to_string(&restored).unwrap(), "contents");
        assert!(!dir.join("restored.rig-tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_manifests_and_results() {
        let dir = dir("entries");
        let cache = Cache::open(&dir).unwrap();
        let key = blake3::hash(b"key");
        assert!(cache.manifest(&key).is_empty());
        let discovery = |input: &str| Discovery { inputs: vec![input.into()], env: vec!["VARIABLE".into()] };
        cache.add_to_manifest(&key, discovery("a.h")).unwrap();
        cache.add_to_manifest(&key, discovery("b.h")).unwrap();
        // one which is added again is the latest
        cache.add_to_manifest(&key, discovery("a.h")).unwrap();
        assert_eq!(cache.manifest(&key), [discovery("a.h"), discovery("b.h")]);
        for i in 0..MANIFEST_MAX * 2 {
            cache.add_to_manifest(&key, discovery(&i.to_string())).unwrap();
        }
        assert_eq!(cache.manifest(&key).len(), MANIFEST_MAX);
        // a result is found only once the cache has its blobs
        let printed = cache.put_bytes(b"").unwrap();
        let blob = blake3::hash(b"output");
        let entry = Entry { outputs: vec![Output { path: "a.o".into(), blob, executable: false }], printed };
        cache.put_result(&key, &entry).unwrap();
        assert_eq!(cache.result(&key), None);
        assert_eq!(cache.put_bytes(b"output").unwrap(), blob);
        assert_eq!(cache.result(&key), Some(entry));
        // and one which is corrupted is a miss, and is removed
        let path = cache.keyed("ac", &key);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[12] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(cache.result(&key), None);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_differ_by_each_part() {
        let hash = blake3::hash(b"a");
        let env = [("A".to_string(), "1".to_string())];
        let key = |identity, action: &[u8], env: &[(String, String)], inputs: &[(&str, Option<&Hash>)]| {
            first_key(identity, action, env, &["out"], Some("out.d"), inputs)
        };
        let base = key("id", b"cc", &env, &[("a", Some(&hash))]);
        assert_eq!(base, key("id", b"cc", &env, &[("a", Some(&hash))]));
        assert_ne!(base, key("other", b"cc", &env, &[("a", Some(&hash))]));
        assert_ne!(base, key("id", b"c", &env, &[("a", Some(&hash))]));
        assert_ne!(base, key("id", b"cc", &[], &[("a", Some(&hash))]));
        assert_ne!(base, key("id", b"cc", &env, &[("a", None)]));
        assert_ne!(base, key("id", b"cc", &env, &[("b", Some(&hash))]));
        assert_ne!(base, first_key("id", b"cc", &env, &["out"], None, &[("a", Some(&hash))]));
        // parts which run together differ
        assert_ne!(key("idc", b"c", &env, &[]), key("id", b"cc", &env, &[]));
        let second = second_key(&base, &[("a.h", Some(&hash))], &[]);
        assert_ne!(second, second_key(&base, &[], &[("a.h", Some(&hash))]));
        assert_ne!(second, second_key(&base, &[("a.h", None)], &[]));
        assert_eq!(hex(&[0xab; 32]).len(), 64);
    }
}
