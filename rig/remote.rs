//! A remote cache: a [`Cache`] on another machine, or in another process, spoken to over a stream of bytes, such as
//! the stdin and stdout of `ssh host jig serve --stdio`, which [`serve`] answers.
//!
//! The protocol is a request then its reply, in turn, each a frame: its kind as a byte, the length of its payload as a
//! little-endian `u64`, then the payload. Hashes and keys are 32 bytes, numbers are little-endian `u32`s and strings
//! are their length followed by their UTF-8 bytes.
//!
//! - The client sends `HELLO`, with the magic bytes and the version, and the server replies `HELLO`, with the version
//!   and whether it takes pushes, `1` or `0`, or `ERROR` with why it does not speak the version.
//! - `GET_MANIFEST`, `GET_RESULT` and `GET_BLOB`, with a key or hash, are replied to with `FOUND` and the manifest or
//!   result as it is stored, or the blob, or `NOT_FOUND`. A result is only found if the server has every blob it
//!   refers to.
//! - `MISSING`, with a number of hashes, is replied to with `MISSING` and those of them the server does not have.
//! - `PUT_BLOB`, with a hash and the blob, `PUT_RESULT`, with a key and the result as it is stored, and
//!   `ADD_TO_MANIFEST`, with a key and the inputs and environment variables an edge discovered, as two lists of
//!   strings, are replied to with `OK`, or `ERROR` with why, as when the server does not take pushes, or what was sent
//!   is not valid.
//!
//! What is fetched is checked as what is read from a cache on disk is: a blob against its hash, and a manifest or
//! result against the hash which ends it, so that a server which sends something corrupted is a miss.
//!
//! A [`Remote`] which fails, as its connection is lost or the server replies with an error, is not used again, so that
//! the build goes on with the local cache, and [`Remote::error`] says why.

use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::cache::{Cache, Discovery, Hash};

const MAGIC: &[u8; 4] = b"RIGR";
const VERSION: u32 = 1;

const HELLO: u8 = 1;
const GET_MANIFEST: u8 = 2;
const GET_RESULT: u8 = 3;
const GET_BLOB: u8 = 4;
const MISSING: u8 = 5;
const PUT_BLOB: u8 = 6;
const PUT_RESULT: u8 = 7;
const ADD_TO_MANIFEST: u8 = 8;
const FOUND: u8 = 64;
const NOT_FOUND: u8 = 65;
const OK: u8 = 66;
const ERROR: u8 = 67;

/// The largest payload of a frame which is read, beyond which the stream is taken as corrupted.
const MAX_FRAME: u64 = 1 << 36;

fn write_frame(w: &mut dyn Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&[kind])?;
    w.write_all(&(payload.len() as u64).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Returns the next frame, or `None` at the end of the stream, before a frame.
fn read_frame(r: &mut dyn Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut kind = [0];
    match r.read_exact(&mut kind) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut len = [0; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("a frame of {len} bytes")));
    }
    let mut payload = vec![0; usize::try_from(len).map_err(std::io::Error::other)?];
    r.read_exact(&mut payload)?;
    Ok(Some((kind[0], payload)))
}

fn push_str(bytes: &mut Vec<u8>, string: &str) {
    bytes.extend_from_slice(&(string.len() as u32).to_le_bytes());
    bytes.extend_from_slice(string.as_bytes());
}

fn push_strs(bytes: &mut Vec<u8>, strings: &[String]) {
    bytes.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    for string in strings {
        push_str(bytes, string);
    }
}

/// Reads the parts of a payload.
struct Payload<'a>(&'a [u8]);
impl<'a> Payload<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(taken)
    }
    fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_string())
    }
    fn strs(&mut self) -> Option<Vec<String>> {
        (0..self.u32()?).map(|_| self.str()).collect()
    }
    fn hashes(&mut self) -> Option<Vec<Hash>> {
        (0..self.u32()?).map(|_| self.hash()).collect()
    }
}

fn hashes(hashes: &[Hash]) -> Vec<u8> {
    let mut bytes = (hashes.len() as u32).to_le_bytes().to_vec();
    for hash in hashes {
        bytes.extend_from_slice(hash);
    }
    bytes
}

/// A connection to a server: what is read from it and written to it, and the process it is, if it is one.
struct Connection {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<Child>,
    /// What the process wrote to stderr, which is kept to tell why it failed, and the thread which reads it.
    stderr: Option<Arc<Mutex<Vec<u8>>>>,
    reading: Option<std::thread::JoinHandle<()>>,
    /// Whether the server takes pushes.
    pushes: bool,
}
impl Connection {
    fn request(&mut self, kind: u8, payload: &[u8]) -> std::io::Result<(u8, Vec<u8>)> {
        write_frame(&mut self.writer, kind, payload)?;
        let reply = read_frame(&mut self.reader)?;
        reply.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the server closed the connection"))
    }
    /// Returns why a request failed, with the last line the process wrote to stderr, if it wrote one, once it has
    /// ended, as it does once its stdin is closed, if it has not already.
    fn failure(&mut self, error: impl std::fmt::Display) -> String {
        self.writer = Box::new(std::io::sink());
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
        if let Some(reading) = self.reading.take() {
            let _ = reading.join();
        }
        match self.stderr() {
            Some(stderr) => format!("{error}: {stderr}"),
            None => error.to_string(),
        }
    }
    /// Returns the last line the process wrote to stderr, if it wrote one.
    fn stderr(&self) -> Option<String> {
        let stderr = self.stderr.as_ref()?.lock().ok()?;
        let text = String::from_utf8_lossy(&stderr);
        text.lines().rev().find(|line| !line.trim().is_empty()).map(|line| line.trim().to_string())
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        // the server ends once its stdin does
        self.writer = Box::new(std::io::sink());
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
    }
}

enum State {
    /// Not connected yet, with the streams to connect over if they were given rather than a command.
    Unconnected(Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)>),
    Connected(Connection),
    /// It failed, and is not used again, for this reason.
    Failed(String),
}

/// A remote cache, which is connected to when it is first used.
pub struct Remote {
    /// The program and arguments which run the server, speaking the protocol on its stdin and stdout.
    command: Vec<String>,
    /// Whether results are pushed to it, if it takes them.
    push: bool,
    state: Mutex<State>,
}
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Remote").field("command", &self.command).field("push", &self.push).finish_non_exhaustive()
    }
}

impl Remote {
    /// A remote cache which a command serves on its stdin and stdout, such as `ssh host jig serve --stdio`, which
    /// results are pushed to with `push`, if it takes them.
    #[must_use]
    pub fn command(command: Vec<String>, push: bool) -> Self {
        Self { command, push, state: Mutex::new(State::Unconnected(None)) }
    }
    /// A remote cache which is served over streams, as by [`serve`] on another thread.
    #[must_use]
    pub fn streams(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, push: bool) -> Self {
        let streams: (Box<dyn Read + Send>, Box<dyn Write + Send>) = (Box::new(reader), Box::new(writer));
        Self { command: Vec::new(), push, state: Mutex::new(State::Unconnected(Some(streams))) }
    }
    /// Returns why the remote cache failed, if it did, after which it is not used.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        match &*self.state.lock().ok()? {
            State::Failed(error) => Some(error.clone()),
            State::Unconnected(_) | State::Connected(_) => None,
        }
    }
    /// Whether results are pushed to the remote cache, as it is asked to and it takes them, connecting to it to find
    /// out.
    #[must_use]
    pub fn pushes(&self) -> bool {
        self.push && self.with(|connection| Ok(connection.pushes)).unwrap_or(false)
    }
    fn connect(&self, streams: Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)>) -> Result<Connection, String> {
        let mut connection = if let Some((reader, writer)) = streams {
            Connection { reader, writer, child: None, stderr: None, reading: None, pushes: false }
        } else {
            let (program, args) = self.command.split_first().ok_or("no command for the remote cache")?;
            let mut command = Command::new(program);
            command.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = command.spawn().map_err(|error| format!("failed to run `{program}`: {error}"))?;
            let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
            let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
                return Err(format!("`{program}` has no stdin or stdout"));
            };
            // stderr is read as it is written, so that the process does not block on it, and kept to tell why it
            // failed
            let stderr = Arc::new(Mutex::new(Vec::new()));
            let reading = child.stderr.take().map(|mut pipe| {
                let kept = Arc::clone(&stderr);
                std::thread::spawn(move || {
                    let mut buffer = [0; 4096];
                    while let Ok(read) = pipe.read(&mut buffer) {
                        if read == 0 {
                            break;
                        }
                        if let Ok(mut kept) = kept.lock() {
                            kept.extend_from_slice(&buffer[..read]);
                        }
                    }
                })
            });
            Connection {
                reader: Box::new(BufReader::new(stdout)),
                writer: Box::new(BufWriter::new(stdin)),
                child: Some(child),
                stderr: Some(stderr),
                reading,
                pushes: false,
            }
        };
        let mut hello = MAGIC.to_vec();
        hello.extend_from_slice(&VERSION.to_le_bytes());
        let reply = connection.request(HELLO, &hello);
        match reply {
            Ok((HELLO, payload)) if payload.len() == 5 && payload[..4] == VERSION.to_le_bytes() => {
                connection.pushes = payload[4] == 1;
                Ok(connection)
            },
            Ok((ERROR, message)) => Err(String::from_utf8_lossy(&message).into_owned()),
            Ok(_) => Err("the server does not speak rig's protocol".into()),
            Err(error) => Err(connection.failure(error)),
        }
    }
    /// Run a request on the connection, connecting first if it is not, returning `None` if the remote cache has failed,
    /// or fails now, when it is not used again.
    fn with<T>(&self, request: impl FnOnce(&mut Connection) -> std::io::Result<T>) -> Option<T> {
        let mut state = self.state.lock().ok()?;
        if let State::Unconnected(streams) = &mut *state {
            *state = match self.connect(streams.take()) {
                Ok(connection) => State::Connected(connection),
                Err(error) => State::Failed(error),
            };
        }
        let State::Connected(connection) = &mut *state else {
            return None;
        };
        match request(connection) {
            Ok(value) => Some(value),
            Err(error) => {
                *state = State::Failed(connection.failure(error));
                None
            },
        }
    }
    /// Run a request whose reply is `FOUND` with what was found, or `NOT_FOUND`.
    fn get(&self, kind: u8, key: &Hash) -> Option<Vec<u8>> {
        self.with(|connection| match connection.request(kind, key)? {
            (FOUND, payload) => Ok(Some(payload)),
            (NOT_FOUND, _) => Ok(None),
            reply => Err(unexpected(reply)),
        })
        .flatten()
    }
    /// Run a request whose reply is `OK`, returning whether it was.
    fn put(&self, kind: u8, payload: &[u8]) -> bool {
        self.with(|connection| match connection.request(kind, payload)? {
            (OK, _) => Ok(()),
            reply => Err(unexpected(reply)),
        })
        .is_some()
    }
    /// Returns a manifest as it is stored, if the remote cache has it.
    #[must_use]
    pub fn manifest(&self, key: &Hash) -> Option<Vec<u8>> {
        self.get(GET_MANIFEST, key)
    }
    /// Returns a result as it is stored, if the remote cache has it and every blob it refers to.
    #[must_use]
    pub fn result(&self, key: &Hash) -> Option<Vec<u8>> {
        self.get(GET_RESULT, key)
    }
    /// Returns a blob, if the remote cache has it.
    #[must_use]
    pub fn blob(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.get(GET_BLOB, hash)
    }
    /// Returns those of the blobs the remote cache does not have, or `None` if it failed.
    #[must_use]
    pub fn missing(&self, blobs: &[Hash]) -> Option<Vec<Hash>> {
        self.with(|connection| match connection.request(MISSING, &hashes(blobs))? {
            (MISSING, payload) => Payload(&payload).hashes().ok_or_else(|| invalid("an invalid list of blobs")),
            reply => Err(unexpected(reply)),
        })
    }
    /// Push a blob, returning whether it was taken.
    #[must_use]
    pub fn put_blob(&self, hash: &Hash, contents: &[u8]) -> bool {
        self.put(PUT_BLOB, &[hash.as_slice(), contents].concat())
    }
    /// Push a result as it is stored, by its second key, returning whether it was taken.
    #[must_use]
    pub fn put_result(&self, key: &Hash, result: &[u8]) -> bool {
        self.put(PUT_RESULT, &[key.as_slice(), result].concat())
    }
    /// Push the inputs an edge discovered, by its first key, to add to its manifest, returning whether they were taken.
    #[must_use]
    pub fn add_to_manifest(&self, key: &Hash, discovery: &Discovery) -> bool {
        let mut payload = key.to_vec();
        push_strs(&mut payload, &discovery.inputs);
        push_strs(&mut payload, &discovery.env);
        self.put(ADD_TO_MANIFEST, &payload)
    }
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Returns the error of a reply which was not expected, which is an error the server sent if it is one.
fn unexpected((kind, payload): (u8, Vec<u8>)) -> std::io::Error {
    if kind == ERROR {
        return std::io::Error::other(format!("the server failed: {}", String::from_utf8_lossy(&payload)));
    }
    invalid("an unexpected reply from the server")
}

/// Serve a cache over streams, as `jig serve --stdio` does on its stdin and stdout, until the client ends the stream,
/// taking pushes if `push`.
///
/// # Errors
///
/// If the streams cannot be read or written, or the client does not speak the protocol.
pub fn serve(cache: &Cache, reader: impl Read, writer: impl Write, push: bool) -> std::io::Result<()> {
    let (mut reader, mut writer) = (BufReader::new(reader), BufWriter::new(writer));
    let Ok(Some((HELLO, hello))) = read_frame(&mut reader) else {
        return Err(invalid("the client does not speak rig's protocol"));
    };
    let mut expected = MAGIC.to_vec();
    expected.extend_from_slice(&VERSION.to_le_bytes());
    if hello != expected {
        let message = format!("the server speaks version {VERSION} of rig's protocol, which the client does not");
        write_frame(&mut writer, ERROR, message.as_bytes())?;
        return Err(invalid(&message));
    }
    let mut reply = VERSION.to_le_bytes().to_vec();
    reply.push(u8::from(push));
    write_frame(&mut writer, HELLO, &reply)?;
    while let Some((kind, payload)) = read_frame(&mut reader)? {
        let (kind, reply) = answer(cache, push, kind, &payload);
        write_frame(&mut writer, kind, &reply)?;
    }
    Ok(())
}

/// Returns the reply to a request.
fn answer(cache: &Cache, push: bool, kind: u8, payload: &[u8]) -> (u8, Vec<u8>) {
    let error = |message: &str| (ERROR, message.as_bytes().to_vec());
    let found = |found: Option<Vec<u8>>| found.map_or((NOT_FOUND, Vec::new()), |found| (FOUND, found));
    let mut p = Payload(payload);
    if matches!(kind, PUT_BLOB | PUT_RESULT | ADD_TO_MANIFEST) && !push {
        return error("the server does not take pushes");
    }
    if kind == MISSING {
        return match p.hashes() {
            Some(blobs) => (MISSING, hashes(&blobs.into_iter().filter(|blob| !cache.has_blob(blob)).collect::<Vec<_>>())),
            None => error("an invalid list of blobs"),
        };
    }
    let Some(key) = p.hash() else {
        return error("a request without a key");
    };
    match kind {
        GET_MANIFEST => found(cache.raw_manifest(&key)),
        GET_RESULT => found(cache.raw_result(&key)),
        GET_BLOB => found(cache.read_blob(&key).ok()),
        PUT_BLOB if cache.accept_blob(&key, p.0) => (OK, Vec::new()),
        PUT_BLOB => error("a blob which does not have its hash"),
        PUT_RESULT if cache.accept_result(&key, p.0) => (OK, Vec::new()),
        PUT_RESULT => error("an invalid result"),
        ADD_TO_MANIFEST => {
            let discovery = p.strs().zip(p.strs()).map(|(inputs, env)| Discovery { inputs, env });
            match discovery.map(|discovery| cache.add_to_manifest(&key, discovery)) {
                Some(Ok(())) => (OK, Vec::new()),
                _ => error("an invalid manifest"),
            }
        },
        _ => error("an unknown request"),
    }
}

/// Returns a remote cache which `cache` is served as, by [`serve`] on another thread, over pipes.
#[cfg(test)]
pub(crate) fn served(cache: Cache, push: bool, pushes: bool) -> Remote {
    let (requests, client) = std::io::pipe().unwrap();
    let (server, replies) = std::io::pipe().unwrap();
    std::thread::spawn(move || serve(&cache, requests, replies, pushes));
    Remote::streams(server, client, push)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Entry, Output};

    fn cache(name: &str) -> Cache {
        let dir = std::env::temp_dir().join(format!("rig-remote-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Cache::open(&dir).unwrap()
    }

    /// Add a result to a cache by its keys, of an output with contents, with what it discovered.
    fn add(cache: &Cache, first: &Hash, second: &Hash, contents: &str, discovery: &Discovery) {
        let blob = cache.put_bytes(contents.as_bytes()).unwrap();
        let printed = cache.put_bytes(b"printed").unwrap();
        let entry = Entry { outputs: vec![Output { path: "out".into(), blob, executable: false }], printed };
        cache.put_result(second, &entry).unwrap();
        cache.add_to_manifest(first, discovery.clone()).unwrap();
    }

    #[test]
    fn pushes_and_fetches() {
        let (first, second) = (crate::blake3::hash(b"first"), crate::blake3::hash(b"second"));
        let discovery = Discovery { inputs: vec!["a.h".into()], env: vec!["VARIABLE".into()] };
        let server = cache("server");
        // a cache in front of the remote pushes what is added to it, which the remote then has
        let pusher = cache("pusher").with_remote(Arc::new(served(server.clone(), true, true)));
        add(&pusher, &first, &second, "contents", &discovery);
        assert!(pusher.push(&first, &second, &discovery));
        assert_eq!(server.result(&second).map(|entry| entry.outputs.len()), Some(1));
        assert_eq!(server.manifest(&first), std::slice::from_ref(&discovery));
        assert_eq!(server.stats().blobs, 2);
        // and pushing it again sends no blobs, as it has them
        assert_eq!(pusher.remote().unwrap().missing(&[crate::blake3::hash(b"contents")]), Some(Vec::new()));
        assert!(pusher.push(&first, &second, &discovery));
        // another cache finds the manifest and result there, and fetches the blobs as they are restored
        let puller = cache("puller").with_remote(Arc::new(served(server.clone(), false, true)));
        assert_eq!(puller.manifest(&first), std::slice::from_ref(&discovery));
        let entry = puller.result(&second).unwrap();
        let out = puller.dir().join("restored");
        puller.restore(&entry.outputs[0].blob, &out, false).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "contents");
        assert_eq!(puller.read_blob(&entry.printed).unwrap(), b"printed");
        // which it then has itself, with the result
        assert_eq!(puller.stats().results, 1);
        assert!(puller.remote().unwrap().error().is_none());
        // and what neither has is a miss
        assert!(puller.result(&crate::blake3::hash(b"other")).is_none());
        for cache in [server, pusher, puller] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn pushes_only_where_they_are_taken() {
        let (first, second) = (crate::blake3::hash(b"first"), crate::blake3::hash(b"second"));
        let server = cache("refuses");
        let client = cache("refused").with_remote(Arc::new(served(server.clone(), true, false)));
        add(&client, &first, &second, "contents", &Discovery::default());
        assert!(!client.remote().unwrap().pushes());
        assert!(!client.push(&first, &second, &Discovery::default()));
        assert_eq!(server.stats(), crate::cache::Stats::default());
        // and a server which does not take them refuses them
        assert!(!client.remote().unwrap().put_blob(&crate::blake3::hash(b"x"), b"x"));
        assert!(client.remote().unwrap().error().unwrap().contains("does not take pushes"));
        for cache in [server, client] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn checks_what_it_fetches() {
        // a server which replies to every request with a blob which is not what was asked for
        let (requests, client) = std::io::pipe().unwrap();
        let (server, mut replies) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let mut requests = requests;
            while let Ok(Some((kind, _))) = read_frame(&mut requests) {
                let reply: &[u8] = if kind == HELLO { &[1, 0, 0, 0, 0] } else { b"corrupted" };
                let _ = write_frame(&mut replies, if kind == HELLO { HELLO } else { FOUND }, reply);
            }
        });
        let client_cache = cache("corrupted").with_remote(Arc::new(Remote::streams(server, client, false)));
        let hash = crate::blake3::hash(b"contents");
        assert!(client_cache.read_blob(&hash).is_err());
        assert!(!client_cache.has_blob(&hash));
        // as is a result or manifest
        assert!(client_cache.result(&hash).is_none());
        assert!(client_cache.manifest(&hash).is_empty());
        let _ = std::fs::remove_dir_all(client_cache.dir());
    }

    #[test]
    fn stops_using_a_remote_which_fails() {
        let remote = Remote::command(vec!["rig-no-such-server".into()], true);
        assert!(remote.blob(&[0; 32]).is_none());
        assert!(remote.error().unwrap().contains("rig-no-such-server"), "{:?}", remote.error());
        // and a server which ends, with why on its stderr
        let remote = Remote::command(vec!["sh".into(), "-c".into(), "echo no such cache >&2".into()], true);
        assert!(remote.manifest(&[0; 32]).is_none());
        assert!(remote.error().unwrap().contains("no such cache"), "{:?}", remote.error());
        assert!(!remote.pushes());
    }

    #[test]
    fn refuses_another_version() {
        let dir = cache("version");
        let mut hello = Vec::new();
        let mut request = MAGIC.to_vec();
        request.extend_from_slice(&(VERSION + 1).to_le_bytes());
        write_frame(&mut hello, HELLO, &request).unwrap();
        let mut reply = Vec::new();
        assert!(serve(&dir, hello.as_slice(), &mut reply, false).is_err());
        let (kind, message) = read_frame(&mut reply.as_slice()).unwrap().unwrap();
        assert_eq!(kind, ERROR);
        assert!(String::from_utf8(message).unwrap().contains("version"));
        let _ = std::fs::remove_dir_all(dir.dir());
    }
}
