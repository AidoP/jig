//! A cache of the results of edges, shared between builds. An edge which has already run with the same action and
//! inputs, in any build sharing the cache, has its outputs fetched instead of running again.
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
//! An edge is found in two steps, because the inputs it discovers are only known once it has run. ccache's manifests
//! work the same way.
//!
//! 1. The first key, from [`first_key`], is made from the edge's action and the contents of the inputs it declares. It
//!    leads to a manifest: the sets of inputs the edge discovered when it ran with that key before.
//! 2. The second key, from [`second_key`], is made from the first key and the contents, as they are now, of the inputs
//!    in one of those sets. It leads to a result.
//!
//! A hit sets the modification times of its result and manifest to now. [`Cache::gc`] uses those times to remove the
//! results used longest ago first, then the blobs no remaining result refers to, and the manifests not used since.
//!
//! A cache can have a [`Remote`] cache behind it. What is not found locally is looked for there, a blob which is not
//! held locally is fetched from there when it is needed, and what is added locally is pushed there, if the remote
//! takes pushes.
//!
//! Every file is written to `tmp` and renamed into place, so a build reading the cache never sees a file another
//! build has half written, and builds need not lock the cache. A blob is checked against its hash when it is read,
//! and a manifest or result against the hash which ends it. A corrupted file is removed and counts as a miss, so
//! corruption never leads to a wrong build.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::Arc;

use crate::Error;
use crate::blake3::{self, Hasher};
use crate::bytes::{Reader, Writer};
use crate::remote::Remote;

/// The hash of a blob, or a key.
pub type Hash = [u8; 32];

const MAGIC: &[u8; 4] = b"RIGC";
/// The version of the format of manifests and results.
///
/// It became 2 to discard results stored under the wrong toolchain's identity: before `RUSTUP_TOOLCHAIN` was passed
/// on, the hermetic environment could run a different toolchain from the one a result was identified by.
const VERSION: u32 = 2;
pub(crate) const MANIFEST: u8 = 1;
pub(crate) const RESULT: u8 = 2;

/// The most sets of discovered inputs a manifest keeps, latest first. The limit bounds the cost of a lookup for an
/// edge whose inputs have changed many times.
const MANIFEST_MAX: usize = 16;

/// How long collection leaves a blob which no result refers to, or a file in `tmp`. A build may still be writing the
/// file, or about to add the result which refers to the blob.
const GRACE: std::time::Duration = std::time::Duration::from_hours(1);

/// How long an edge which failed on a worker but succeeded here stays marked to run here.
const LOCAL_FOR: std::time::Duration = std::time::Duration::from_hours(24 * 30);

/// The file whose modification time records when [`Cache::gc_if_due`] last collected the cache.
const COLLECTED: &str = "collected";

/// Counts the files this process has written to `tmp`, to give each a different name.
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// The cache in a directory, and the remote cache behind it, if it has one.
#[derive(Clone, Debug)]
pub struct Cache {
    dir: PathBuf,
    remote: Option<Arc<Remote>>,
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

/// A blob being written in parts, as it is received, to a file in `tmp`. It is hashed as it is written, and the file
/// is removed if the blob is never finished.
pub struct BlobWriter {
    cache: Cache,
    tmp: PathBuf,
    file: Option<std::io::BufWriter<std::fs::File>>,
    hasher: Hasher,
    /// Whether a part could not be written, so that the blob is not added.
    failed: bool,
}
impl BlobWriter {
    /// Write the next part of the blob.
    pub fn write(&mut self, part: &[u8]) {
        use std::io::Write;
        self.hasher.update(part);
        let written = self.file.as_mut().map(|file| file.write_all(part));
        self.failed |= !matches!(written, Some(Ok(())));
    }
    /// Add the blob to the cache, if every part was written and it has its hash, returning whether it was added.
    #[must_use]
    pub fn finish(mut self, hash: &Hash) -> bool {
        use std::io::Write;
        let flushed = self.file.take().is_some_and(|mut file| file.flush().is_ok());
        if self.failed || !flushed || self.hasher.finalize() != *hash {
            return false;
        }
        if self.cache.has_blob(hash) {
            return true;
        }
        Cache::place(&self.tmp, &self.cache.blob_path(hash)).is_ok()
    }
}
impl Drop for BlobWriter {
    fn drop(&mut self) {
        // once it is placed, the file is not there
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// What a cache holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub results: usize,
    pub manifests: usize,
    pub blobs: usize,
    /// The size of its files, in bytes.
    pub bytes: u64,
}

/// What [`Cache::gc`] removed, and what it kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub removed: Stats,
    pub kept: Stats,
}

/// A file of one of the stores, with its size and when it was last modified, or used.
struct Stored {
    path: PathBuf,
    size: u64,
    modified: std::time::SystemTime,
}

/// Returns the files in a directory, which is empty if it cannot be read.
fn stored(dir: &Path) -> Vec<Stored> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let files = entries.filter_map(|entry| {
        let entry = entry.ok()?;
        let metadata = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        Some(Stored { path: entry.path(), size: metadata.len(), modified })
    });
    files.collect()
}

/// Mark a file as used just now, by setting its modification time.
fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
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
        Ok(Self { dir: dir.into(), remote: None })
    }
    /// Put the cache in front of a remote cache.
    #[must_use]
    pub fn with_remote(mut self, remote: Arc<Remote>) -> Self {
        self.remote = Some(remote);
        self
    }
    #[must_use]
    pub fn remote(&self) -> Option<&Remote> {
        self.remote.as_deref()
    }
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn blob_path(&self, hash: &Hash) -> PathBuf {
        let hex = hex(hash);
        self.dir.join("cas").join(&hex[..2]).join(&hex[2..])
    }
    fn keyed(&self, store: &str, key: &Hash) -> PathBuf {
        self.dir.join(store).join(hex(key))
    }
    /// Returns a path in `tmp` which no other file being written has.
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
    /// Whether the cache has a blob. If it does, the blob is marked as used just now, which keeps it from collection.
    pub(crate) fn use_blob(&self, hash: &Hash) -> bool {
        let path = self.blob_path(hash);
        let has = path.is_file();
        if has {
            touch(&path);
        }
        has
    }
    /// Add a file to the cache as a blob, unless the cache already has it. `hash` is the hash of the file's contents.
    ///
    /// # Errors
    ///
    /// If the file cannot be copied into the cache, or its contents no longer have the hash, as it changed after it
    /// was hashed.
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
    /// If the cache does not have the blob, or the blob is corrupted, in which case it is removed.
    pub fn read_blob(&self, hash: &Hash) -> std::io::Result<Vec<u8>> {
        self.fetch_blob(hash);
        let path = self.blob_path(hash);
        let bytes = std::fs::read(&path)?;
        if blake3::hash(&bytes) != *hash {
            let _ = std::fs::remove_file(&path);
            return Err(corrupted(&path));
        }
        Ok(bytes)
    }
    /// Write a blob to a file, replacing the file. The blob is copied, by a reflink where the file system supports
    /// one, and the copy is checked against the hash before it replaces the file.
    ///
    /// # Errors
    ///
    /// If the cache does not have the blob, the file cannot be written, or the blob is corrupted, in which case it is
    /// removed.
    pub fn restore(&self, hash: &Hash, path: &Path, executable: bool) -> std::io::Result<()> {
        self.fetch_blob(hash);
        let blob = self.blob_path(hash);
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".rig-tmp");
        let tmp = PathBuf::from(tmp);
        let restored = copy(&blob, &tmp).and_then(|()| {
            if blake3::hash_file(&tmp)? != *hash {
                let _ = std::fs::remove_file(&blob);
                return Err(corrupted(&blob));
            }
            crate::sys::set_executable(&tmp, executable)?;
            std::fs::rename(&tmp, path)
        });
        if restored.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        restored
    }
    /// Fetch a blob from the remote cache if this one does not have it, checking it against its hash. Returns whether
    /// this cache has the blob afterwards.
    fn fetch_blob(&self, hash: &Hash) -> bool {
        self.fetch_blobs(std::slice::from_ref(hash));
        self.has_blob(hash)
    }
    /// Fetch the blobs this cache does not have from the remote cache, in one request, checking each against its
    /// hash.
    pub fn fetch_blobs(&self, blobs: &[Hash]) {
        let Some(remote) = self.remote() else {
            return;
        };
        let missing: Vec<Hash> = blobs.iter().filter(|blob| !self.has_blob(blob)).copied().collect();
        if missing.is_empty() {
            return;
        }
        remote.fetch_blobs(self, &missing);
    }
    /// Add a blob which was sent to this cache, if its contents have the hash. Returns whether it was added.
    pub(crate) fn accept_blob(&self, hash: &Hash, bytes: &[u8]) -> bool {
        blake3::hash(bytes) == *hash && self.put_bytes(bytes).is_ok()
    }
    /// Returns a writer for a blob which is received in parts. When it is finished, the blob is added if its contents
    /// have the hash.
    ///
    /// # Errors
    ///
    /// If its file in `tmp` cannot be created.
    pub fn blob_writer(&self) -> std::io::Result<BlobWriter> {
        let tmp = self.tmp();
        let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        Ok(BlobWriter { cache: self.without_remote(), tmp, file: Some(file), hasher: Hasher::new(), failed: false })
    }
    /// Returns a blob's file, open to be read in parts, with its size, if the cache has it.
    ///
    /// # Errors
    ///
    /// If the cache does not have it, or it cannot be opened.
    pub fn open_blob(&self, hash: &Hash) -> std::io::Result<(std::fs::File, u64)> {
        let file = std::fs::File::open(self.blob_path(hash))?;
        let size = file.metadata()?.len();
        Ok((file, size))
    }
    /// Remove a blob, such as one found to be corrupted.
    pub(crate) fn remove_blob(&self, hash: &Hash) {
        let _ = std::fs::remove_file(self.blob_path(hash));
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
    ///
    /// The sets from the remote cache follow those from this one, as the edge may have run with other inputs
    /// elsewhere.
    #[must_use]
    pub fn manifest(&self, key: &Hash) -> Vec<Discovery> {
        let mut manifest = self.local_manifest(key);
        let remote = self.remote().and_then(|remote| remote.manifest(key));
        let remote = remote.and_then(|bytes| read_manifest(valid(MANIFEST, &bytes)?)).unwrap_or_default();
        for discovery in remote {
            if !manifest.contains(&discovery) {
                manifest.push(discovery);
            }
        }
        manifest
    }
    fn local_manifest(&self, key: &Hash) -> Vec<Discovery> {
        read(MANIFEST, &self.keyed("mf", key)).and_then(|bytes| read_manifest(&bytes)).unwrap_or_default()
    }
    /// Add a set of inputs which an edge discovered when it ran with a first key to its manifest, as the latest.
    ///
    /// Two builds adding to the same manifest at the same time may overwrite each other, losing one of the sets. That
    /// only causes a later miss, never a wrong build.
    ///
    /// # Errors
    ///
    /// If the manifest cannot be written.
    pub fn add_to_manifest(&self, key: &Hash, discovery: Discovery) -> std::io::Result<()> {
        let manifest = self.local_manifest(key);
        if manifest.first() == Some(&discovery) {
            return Ok(());
        }
        let bytes = manifest_with(manifest, discovery);
        let tmp = self.tmp();
        std::fs::write(&tmp, &bytes)?;
        Self::place(&tmp, &self.keyed("mf", key))
    }
    /// Returns the result of an edge by its second key. It is taken from this cache if it has the result and every
    /// blob the result needs, and otherwise from the remote cache. A result from the remote cache is added to this
    /// one, and its blobs are fetched later, when they are restored.
    #[must_use]
    pub fn result(&self, key: &Hash) -> Option<Entry> {
        if let Some(entry) = self.local_result(key) {
            return Some(entry);
        }
        let (_, bytes) = self.remote()?.first_result(std::slice::from_ref(key))?;
        self.accept_fetched(key, &bytes)
    }
    /// Returns the result of an edge by its second key, if this cache has it and every blob it needs.
    fn local_result(&self, key: &Hash) -> Option<Entry> {
        let entry = read(RESULT, &self.keyed("ac", key)).and_then(|bytes| read_result(&bytes))?;
        let complete = entry_blobs(&entry).all(|blob| self.has_blob(blob));
        complete.then_some(entry)
    }
    /// Add a result fetched from the remote cache, as it is stored, by its second key, if it is valid.
    fn accept_fetched(&self, key: &Hash, bytes: &[u8]) -> Option<Entry> {
        let entry = read_result(valid(RESULT, bytes)?)?;
        self.accept_raw(&self.keyed("ac", key), bytes).ok()?;
        Some(entry)
    }
    /// Finds the result of an edge from its first key. Returns the second key it was found under, and the result.
    ///
    /// `second` computes the second key for a set of inputs the edge discovered. Each set in this cache's manifest is
    /// tried first, latest first. If none leads to a result here, every set in this cache's and the remote's
    /// manifests is tried against the remote cache, in one request. A result found in the remote cache is added to
    /// this one, with its set of inputs, and its blobs are fetched later, when they are restored. The result found is
    /// marked as used just now.
    #[must_use]
    pub fn find(&self, first: &Hash, second: impl Fn(&Discovery) -> Hash) -> Option<(Hash, Entry)> {
        let mut discoveries = self.local_manifest(first);
        let mut keys = Vec::new();
        for discovery in &discoveries {
            let key = second(discovery);
            if let Some(entry) = self.local_result(&key) {
                self.used(first, &key);
                return Some((key, entry));
            }
            keys.push(key);
        }
        let remote = self.remote()?;
        let manifest = remote.manifest(first).and_then(|bytes| read_manifest(valid(MANIFEST, &bytes)?));
        for discovery in manifest.unwrap_or_default() {
            if !discoveries.contains(&discovery) {
                keys.push(second(&discovery));
                discoveries.push(discovery);
            }
        }
        let (index, bytes) = remote.first_result(&keys)?;
        let entry = self.accept_fetched(&keys[index], &bytes)?;
        let _ = self.add_to_manifest(first, discoveries[index].clone());
        Some((keys[index], entry))
    }
    /// Returns the stored bytes of a result, by its second key, if it is valid and the cache has every blob it needs.
    /// A remote cache serves these.
    pub(crate) fn raw_result(&self, key: &Hash) -> Option<Vec<u8>> {
        let path = self.keyed("ac", key);
        let bytes = std::fs::read(&path).ok()?;
        let entry = read_result(valid(RESULT, &bytes)?)?;
        entry_blobs(&entry).all(|blob| self.has_blob(blob)).then(|| {
            touch(&path);
            bytes
        })
    }
    /// Returns the stored bytes of a manifest, by its first key, if it is valid. A remote cache serves these.
    pub(crate) fn raw_manifest(&self, key: &Hash) -> Option<Vec<u8>> {
        let path = self.keyed("mf", key);
        let bytes = std::fs::read(&path).ok()?;
        valid(MANIFEST, &bytes)?;
        touch(&path);
        Some(bytes)
    }
    /// Add a result which was sent as its stored bytes, under its second key, if it is valid. Returns whether it was
    /// added.
    pub(crate) fn accept_result(&self, key: &Hash, bytes: &[u8]) -> bool {
        let valid = valid(RESULT, bytes).and_then(read_result).is_some();
        valid && self.accept_raw(&self.keyed("ac", key), bytes).is_ok()
    }
    fn accept_raw(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = self.tmp();
        std::fs::write(&tmp, bytes)?;
        Self::place(&tmp, path)
    }
    /// Mark an edge, by its first key, to run here rather than on a worker, as it failed there and succeeded here.
    pub fn mark_local(&self, first: &Hash) {
        let _ = self.write_bytes(&self.keyed("local", first), &[]);
    }
    /// Whether an edge, by its first key, is marked to run here rather than on a worker.
    #[must_use]
    pub fn is_local(&self, first: &Hash) -> bool {
        self.keyed("local", first).is_file()
    }
    /// Push the result of an edge, with its keys and the inputs it discovered, to the remote cache, if that takes
    /// pushes. The push happens in the background, and [`Self::wait_for_pushes`] waits for it. Returns whether a push
    /// was queued.
    #[must_use]
    pub fn push(&self, first: &Hash, second: &Hash, discovery: &Discovery) -> bool {
        let Some(remote) = self.remote() else {
            return false;
        };
        remote.queue_push(self.without_remote(), first, second, discovery)
    }
    /// Add the result of an edge by its second key, once its blobs have been added.
    ///
    /// # Errors
    ///
    /// If the result cannot be written.
    pub fn put_result(&self, key: &Hash, entry: &Entry) -> std::io::Result<()> {
        self.write_bytes(&self.keyed("ac", key), &encode_result(entry))
    }
    /// Wait until everything queued for the remote cache has been pushed. Does nothing without a remote cache.
    pub fn wait_for_pushes(&self) {
        if let Some(remote) = self.remote() {
            remote.wait_for_pushes();
        }
    }
    /// Returns the cache without the remote cache it is in front of.
    #[must_use]
    pub fn without_remote(&self) -> Self {
        Self { dir: self.dir.clone(), remote: None }
    }
    fn write_bytes(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let tmp = self.tmp();
        std::fs::write(&tmp, bytes)?;
        Self::place(&tmp, path)
    }
    /// Write a blob to a file which a command will read as an input: by a hard link where possible, and otherwise as
    /// [`Self::restore`] does.
    ///
    /// A hard link shares the blob's file. So this is only for a file which is removed once it has been read, and
    /// which is not executable, as the blob's mode must not be changed.
    ///
    /// # Errors
    ///
    /// If the cache does not have the blob, or the file cannot be written.
    pub fn link(&self, hash: &Hash, path: &Path, executable: bool) -> std::io::Result<()> {
        if !executable && std::fs::hard_link(self.blob_path(hash), path).is_ok() {
            return Ok(());
        }
        self.restore(hash, path, executable)
    }
}

/// Returns the stored bytes of a manifest with `discovery` put first among its sets of discovered inputs. The sets
/// beyond the most a manifest keeps are dropped.
fn manifest_with(mut manifest: Vec<Discovery>, discovery: Discovery) -> Vec<u8> {
    manifest.retain(|other| *other != discovery);
    manifest.insert(0, discovery);
    manifest.truncate(MANIFEST_MAX);
    let mut w = header(MANIFEST);
    w.len(manifest.len());
    for discovery in &manifest {
        w.strs(&discovery.inputs);
        w.strs(&discovery.env);
    }
    sealed(w)
}

/// Returns what was written followed by its hash, which is how each manifest and result is stored.
fn sealed(w: Writer) -> Vec<u8> {
    let mut bytes = w.0;
    let hash = blake3::hash(&bytes);
    bytes.extend_from_slice(&hash);
    bytes
}

/// Returns the stored bytes of a result.
pub(crate) fn encode_result(entry: &Entry) -> Vec<u8> {
    let mut w = header(RESULT);
    w.bytes(&entry.printed);
    w.len(entry.outputs.len());
    for output in &entry.outputs {
        w.str(&output.path);
        w.bytes(&output.blob);
        w.u8(u8::from(output.executable));
    }
    sealed(w)
}

/// Returns a writer for a manifest or a result, already holding the header which says which it is.
fn header(kind: u8) -> Writer {
    let mut w = Writer::new();
    w.bytes(MAGIC);
    w.u32(VERSION);
    w.u8(kind);
    w
}

/// Parses the stored bytes of a result, if they are valid.
pub(crate) fn decode_result(bytes: &[u8]) -> Option<Entry> {
    read_result(valid(RESULT, bytes)?)
}

/// Returns the stored bytes of a manifest with another set of discovered inputs added as the latest. `stored` is the
/// manifest as it was, which is ignored if it is not valid.
pub(crate) fn merged_manifest(stored: Option<&[u8]>, discovery: Discovery) -> Vec<u8> {
    let manifest = stored.and_then(|bytes| read_manifest(valid(MANIFEST, bytes)?)).unwrap_or_default();
    manifest_with(manifest, discovery)
}

/// Returns the blobs which the stored bytes of a result refer to, if they are valid.
pub(crate) fn result_blobs(result: &[u8]) -> Option<Vec<Hash>> {
    let entry = read_result(valid(RESULT, result)?)?;
    Some(entry_blobs(&entry).copied().collect())
}

/// Returns the blobs a result refers to.
pub(crate) fn entry_blobs(entry: &Entry) -> impl Iterator<Item = &Hash> {
    std::iter::once(&entry.printed).chain(entry.outputs.iter().map(|output| &output.blob))
}

/// Returns the body of the stored bytes of a manifest or result, if they are of this version and not corrupted.
pub(crate) fn valid(kind: u8, bytes: &[u8]) -> Option<&[u8]> {
    let header = MAGIC.len() + 5;
    let ok = bytes.len() >= header + 32 && bytes.starts_with(MAGIC) && bytes[4..8] == VERSION.to_le_bytes();
    let (body, hash) = bytes.split_at(bytes.len().saturating_sub(32));
    (ok && bytes[8] == kind && blake3::hash(body) == hash).then(|| &body[header..])
}

/// Reads the body of a manifest or result from its file, removing the file if it is corrupted.
fn read(kind: u8, path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let body = valid(kind, &bytes).map(<[u8]>::to_vec);
    if body.is_none() {
        let _ = std::fs::remove_file(path);
    }
    body
}

impl Cache {
    /// Mark the result under a second key, and the manifest under its first key, as used just now, so that they are
    /// the last to be collected.
    pub fn used(&self, first: &Hash, second: &Hash) {
        touch(&self.keyed("mf", first));
        touch(&self.keyed("ac", second));
    }
    /// Returns the blobs of the cache, by their hash in hexadecimal.
    fn blobs(&self) -> Vec<(String, Stored)> {
        let shards = std::fs::read_dir(self.dir.join("cas")).into_iter().flatten().filter_map(Result::ok);
        let mut blobs = Vec::new();
        for shard in shards {
            let prefix = shard.file_name().to_string_lossy().into_owned();
            for blob in stored(&shard.path()) {
                let name = blob.path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                blobs.push((format!("{prefix}{name}"), blob));
            }
        }
        blobs
    }
    /// Returns what the cache holds.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let (results, manifests, blobs) = (stored(&self.dir.join("ac")), stored(&self.dir.join("mf")), self.blobs());
        let sizes = results.iter().chain(&manifests).chain(blobs.iter().map(|(_, blob)| blob)).map(|file| file.size);
        Stats { results: results.len(), manifests: manifests.len(), blobs: blobs.len(), bytes: sizes.sum() }
    }
    /// Remove what was used longest ago until the cache is no larger than `max` bytes.
    ///
    /// In order:
    ///
    /// 1. The results used longest ago are removed, each with the blobs no remaining result refers to.
    /// 2. The manifests not used since the latest result which was removed are removed.
    /// 3. If the cache is still too large, which needs every result to be gone, the manifests used longest ago are
    ///    removed.
    ///
    /// A blob which no result refers to, and a file in `tmp`, is only removed once it is an hour old, as a build may
    /// be writing the file, or about to add the result which refers to the blob. A corrupted result is removed. A
    /// file which cannot be removed is skipped, as another build may have removed it, or the cache may be read-only.
    #[must_use]
    pub fn gc(&self, max: u64) -> Collected {
        let now = std::time::SystemTime::now();
        let old = |file: &Stored| now.duration_since(file.modified).is_ok_and(|age| age >= GRACE);
        let mut collected = Collected::default();
        let mut remove = |file: &Stored, count: &mut dyn FnMut(&mut Stats)| {
            if std::fs::remove_file(&file.path).is_ok() {
                count(&mut collected.removed);
                collected.removed.bytes += file.size;
                true
            } else {
                false
            }
        };
        // the results, oldest first, with the blobs each refers to
        let mut results: Vec<(Stored, Vec<String>)> = Vec::new();
        let mut corrupted = 0;
        for result in stored(&self.dir.join("ac")) {
            match read(RESULT, &result.path).and_then(|bytes| read_result(&bytes)) {
                Some(entry) => {
                    let blobs = std::iter::once(&entry.printed).chain(entry.outputs.iter().map(|output| &output.blob));
                    results.push((result, blobs.map(hex).collect()));
                },
                // `read` removed it
                None => corrupted += 1,
            }
        }
        results.sort_by_key(|(result, _)| result.modified);
        let mut references: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for blob in results.iter().flat_map(|(_, blobs)| blobs) {
            *references.entry(blob.clone()).or_default() += 1;
        }
        let mut blobs: std::collections::HashMap<String, Stored> = std::collections::HashMap::new();
        for (hash, blob) in self.blobs() {
            if !references.contains_key(&hash) && old(&blob) {
                remove(&blob, &mut |stats| stats.blobs += 1);
            } else {
                blobs.insert(hash, blob);
            }
        }
        let manifests = stored(&self.dir.join("mf"));
        let sizes = results.iter().map(|(result, _)| result.size);
        let mut size: u64 = sizes.chain(manifests.iter().map(|manifest| manifest.size)).sum();
        size += blobs.values().map(|blob| blob.size).sum::<u64>();
        // the latest use of a result which was removed
        let mut cutoff = None;
        let mut kept = results.len();
        for (result, refers) in &results {
            if size <= max {
                break;
            }
            if !remove(result, &mut |stats| stats.results += 1) {
                continue;
            }
            kept -= 1;
            size -= result.size;
            cutoff = Some(result.modified);
            for hash in refers {
                let count = references.get_mut(hash).map_or(0, |count| {
                    *count -= 1;
                    *count
                });
                if count == 0 && let Some(blob) = blobs.remove(hash) {
                    size -= blob.size;
                    remove(&blob, &mut |stats| stats.blobs += 1);
                }
            }
        }
        let mut manifests = manifests;
        manifests.sort_by_key(|manifest| manifest.modified);
        let mut kept_manifests = 0;
        for manifest in &manifests {
            let unused = cutoff.is_some_and(|cutoff| manifest.modified <= cutoff);
            if (unused || size > max) && remove(manifest, &mut |stats| stats.manifests += 1) {
                size -= manifest.size;
            } else {
                kept_manifests += 1;
            }
        }
        for file in stored(&self.dir.join("tmp")).iter().filter(|file| old(file)) {
            let _ = std::fs::remove_file(&file.path);
        }
        // a mark to run an edge here expires, after which the edge is tried on a worker again
        let marked_long_ago = |file: &Stored| now.duration_since(file.modified).is_ok_and(|age| age >= LOCAL_FOR);
        for file in stored(&self.dir.join("local")).iter().filter(|file| marked_long_ago(file)) {
            let _ = std::fs::remove_file(&file.path);
        }
        collected.removed.results += corrupted;
        collected.kept = Stats { results: kept, manifests: kept_manifests, blobs: blobs.len(), bytes: size };
        collected
    }
    /// Collect the cache as [`Self::gc`] does, if it was last collected at least `every` ago. Returns what was
    /// removed and kept, or `None` if it was not collected. A cache with no record of being collected is new, so it
    /// is only marked as collected now.
    #[must_use]
    pub fn gc_if_due(&self, max: u64, every: std::time::Duration) -> Option<Collected> {
        let path = self.dir.join(COLLECTED);
        let modified = std::fs::metadata(&path).and_then(|metadata| metadata.modified());
        let due = modified.as_ref().is_ok_and(|modified| modified.elapsed().is_ok_and(|elapsed| elapsed >= every));
        if modified.is_err() || due {
            // Mark it first, so that builds finishing together do not all collect it. The time is set explicitly,
            // as writing nothing to the file does not change its time on Windows.
            let _ = std::fs::File::create(&path).and_then(|file| file.set_modified(std::time::SystemTime::now()));
        }
        due.then(|| self.gc(max))
    }
    /// Remove everything the cache holds.
    ///
    /// # Errors
    ///
    /// If its stores cannot be removed, or created again.
    pub fn clear(&self) -> Result<(), Error> {
        for store in ["cas", "mf", "ac", "tmp"] {
            let path = self.dir.join(store);
            match std::fs::remove_dir_all(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(Error::Io(path, error)),
            }
        }
        let _ = std::fs::remove_file(self.dir.join(COLLECTED));
        Self::open(&self.dir).map(|_| ())
    }
}

fn corrupted(path: &Path) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{path:?} is corrupted"))
}

/// Copy a file, by a reflink where the file system supports one: the copy shares the file's contents until either is
/// written to.
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

/// A key being built from parts. Each part is hashed with its length before it, so that `["ab", "c"]` and
/// `["a", "bc"]` give different keys.
pub struct Key(Hasher);
impl Key {
    /// Starts a key of a kind. Keys of different kinds are never equal.
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
    /// Add a hash, or `None` for where there is none, as for a missing file.
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

/// Returns the first key of an edge, which is made from:
///
/// - its cache identity;
/// - its action, as [`crate::plan::action_bytes`] gives it;
/// - the variables every command is given;
/// - the variables it passes on, each with the hash of its value, or that it is unset;
/// - its outputs and its depfile;
/// - the inputs it declares, each with its path and the hash of its contents, or that it is missing.
#[must_use]
pub fn first_key(
    identity: &str,
    action: &[u8],
    env: &[(String, String)],
    passed: &[(&str, Option<&Hash>)],
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
    key.part(&(passed.len() as u64).to_le_bytes());
    for (name, hash) in passed {
        key.part(name.as_bytes()).hash(*hash);
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

/// Returns the second key of an edge, which is made from its first key, the inputs it discovered, each with its path
/// and the hash of its contents, or that it is missing, and the environment variables it discovered, each with the
/// hash of its value, or that it is unset.
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

fn read_manifest(bytes: &[u8]) -> Option<Vec<Discovery>> {
    let mut r = Reader(bytes);
    let manifest = r.list(|r| Some(Discovery { inputs: r.strings()?, env: r.strings()? }))?;
    r.is_empty().then_some(manifest)
}

fn read_result(bytes: &[u8]) -> Option<Entry> {
    let mut r = Reader(bytes);
    let printed = r.hash()?;
    let outputs = r.list(|r| {
        Some(Output { path: r.string()?, blob: r.hash()?, executable: r.bool()? })
    })?;
    r.is_empty().then_some(Entry { outputs, printed })
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&restored).unwrap().permissions().mode() & 0o777, 0o755);
        }
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
        assert_eq!(cache.manifest(&key), Vec::<Discovery>::new());
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

    /// Make a file as old as it would be `seconds` ago.
    fn age(path: &Path, seconds: u64) {
        let time = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    /// Add a result by a key, with its manifest, of an output with contents, returning its second key.
    fn add(cache: &Cache, key: &str, contents: &str) -> Hash {
        let (first, second) = (blake3::hash(format!("{key} first").as_bytes()), blake3::hash(key.as_bytes()));
        let blob = cache.put_bytes(contents.as_bytes()).unwrap();
        let printed = cache.put_bytes(b"").unwrap();
        let entry = Entry { outputs: vec![Output { path: "out".into(), blob, executable: false }], printed };
        cache.put_result(&second, &entry).unwrap();
        cache.add_to_manifest(&first, Discovery::default()).unwrap();
        second
    }

    #[test]
    fn collects_what_was_used_longest_ago() {
        let dir = dir("gc");
        let cache = Cache::open(&dir).unwrap();
        let (old, used, new) = (add(&cache, "old", "old output"), add(&cache, "used", "used output"), add(&cache, "new", "x"));
        for (key, seconds) in [("old", 300), ("used", 200), ("new", 100)] {
            let first = blake3::hash(format!("{key} first").as_bytes());
            // the manifest first, so that it is not newer than its result
            age(&cache.keyed("mf", &first), seconds);
            age(&cache.keyed("ac", &blake3::hash(key.as_bytes())), seconds);
        }
        // the oldest is used, so the next oldest is removed first
        cache.used(&blake3::hash(b"old first"), &old);
        let stats = cache.stats();
        assert_eq!((stats.results, stats.manifests, stats.blobs), (3, 3, 4));
        // nothing is removed while it fits
        assert_eq!(cache.gc(stats.bytes).removed, Stats::default());
        let collected = cache.gc(stats.bytes - 1);
        assert_eq!((collected.removed.results, collected.removed.manifests, collected.removed.blobs), (1, 1, 1));
        assert!(cache.result(&used).is_none());
        assert!(cache.result(&old).is_some() && cache.result(&new).is_some());
        assert_eq!(collected.kept, cache.stats());
        // the blob of what was printed is shared, so it is kept while a result refers to it
        let collected = cache.gc(0);
        assert_eq!((collected.kept.results, collected.kept.blobs, collected.kept.bytes), (0, 0, 0));
        assert_eq!(cache.stats(), Stats::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_manifests_newer_than_any_result_to_fit() {
        // as a manifest is written just after its result, it is newer than the latest result removed
        let dir = dir("gc-manifests");
        let cache = Cache::open(&dir).unwrap();
        add(&cache, "a", "a");
        let manifest = cache.keyed("mf", &blake3::hash(b"a first"));
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::File::options().write(true).open(&manifest).unwrap().set_modified(later).unwrap();
        let collected = cache.gc(0);
        assert_eq!((collected.removed.results, collected.removed.manifests), (1, 1));
        assert_eq!(cache.stats(), Stats::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_new_blobs_which_nothing_refers_to() {
        let dir = dir("gc-grace");
        let cache = Cache::open(&dir).unwrap();
        // as a build may be adding the result which refers to it
        let new = cache.put_bytes(b"new").unwrap();
        let old = cache.put_bytes(b"old").unwrap();
        age(&cache.blob_path(&old), 2 * 60 * 60);
        std::fs::write(cache.dir.join("tmp/stale"), "").unwrap();
        age(&cache.dir.join("tmp/stale"), 2 * 60 * 60);
        let collected = cache.gc(u64::MAX);
        assert_eq!(collected.removed.blobs, 1);
        assert!(cache.has_blob(&new) && !cache.has_blob(&old));
        assert!(!cache.dir.join("tmp/stale").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_when_due_and_clears() {
        let dir = dir("gc-due");
        let cache = Cache::open(&dir).unwrap();
        add(&cache, "a", "a");
        let day = std::time::Duration::from_hours(24);
        // a new cache is only marked, then collected once a day has passed
        assert!(cache.gc_if_due(0, day).is_none());
        assert!(cache.gc_if_due(0, day).is_none());
        age(&cache.dir.join(COLLECTED), day.as_secs() + 1);
        assert_eq!(cache.gc_if_due(0, day).map(|collected| collected.removed.results), Some(1));
        assert!(cache.gc_if_due(0, day).is_none());
        add(&cache, "b", "b");
        cache.clear().unwrap();
        assert_eq!(cache.stats(), Stats::default());
        assert!(cache.dir.join("cas").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_differ_by_each_part() {
        let hash = blake3::hash(b"a");
        let env = [("A".to_string(), "1".to_string())];
        let key = |identity, action: &[u8], env: &[(String, String)], inputs: &[(&str, Option<&Hash>)]| {
            first_key(identity, action, env, &[], &["out"], Some("out.d"), inputs)
        };
        let base = key("id", b"cc", &env, &[("a", Some(&hash))]);
        assert_eq!(base, key("id", b"cc", &env, &[("a", Some(&hash))]));
        assert_ne!(base, key("other", b"cc", &env, &[("a", Some(&hash))]));
        assert_ne!(base, key("id", b"c", &env, &[("a", Some(&hash))]));
        assert_ne!(base, key("id", b"cc", &[], &[("a", Some(&hash))]));
        assert_ne!(base, key("id", b"cc", &env, &[("a", None)]));
        assert_ne!(base, key("id", b"cc", &env, &[("b", Some(&hash))]));
        assert_ne!(base, first_key("id", b"cc", &env, &[], &["out"], None, &[("a", Some(&hash))]));
        assert_ne!(base, first_key("id", b"cc", &env, &[("A", None)], &["out"], Some("out.d"), &[("a", Some(&hash))]));
        // parts which run together differ
        assert_ne!(key("idc", b"c", &env, &[]), key("id", b"cc", &env, &[]));
        let second = second_key(&base, &[("a.h", Some(&hash))], &[]);
        assert_ne!(second, second_key(&base, &[], &[("a.h", Some(&hash))]));
        assert_ne!(second, second_key(&base, &[("a.h", None)], &[]));
        assert_eq!(hex(&[0xab; 32]).len(), 64);
    }
}
