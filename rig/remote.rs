//! A remote cache: a [`Cache`] on another machine, or in another process, spoken to over a stream of bytes, such as
//! the stdin and stdout of `ssh host jig serve --stdio`, which [`serve`] answers.
//!
//! The protocol is of requests and their replies, each a frame: its kind as a byte, the id of the request as a
//! little-endian `u32`, which its reply has, the length of its payload as a little-endian `u64`, then the payload.
//! Requests are not answered in turn: a client sends as many as it has, from as many threads as it has, and the server
//! answers them on several threads, in any order, so that one connection, such as one ssh session, carries the
//! requests of a whole build without one waiting for another's round trip. Hashes and keys are 32 bytes, numbers are
//! little-endian `u32`s and strings are their length followed by their UTF-8 bytes.
//!
//! - The client sends `HELLO`, with the magic bytes and the version, and the server replies `HELLO`, with the version
//!   and whether it takes pushes, `1` or `0`, or `ERROR` with why it does not speak the version.
//! - `GET_MANIFEST` and `GET_BLOB`, with a key or hash, are replied to with `FOUND` and the manifest as it is stored,
//!   or the blob, or `NOT_FOUND`.
//! - `GET_RESULTS`, with a number of second keys, is replied to with `FOUND`, the index of the first key whose result
//!   the server has, with every blob it refers to, and the result as it is stored, or `NOT_FOUND`, so that every set of
//!   inputs an edge may have discovered is looked up at once.
//! - `MISSING`, with a number of hashes, is replied to with `MISSING` and those of them the server does not have.
//! - `PUT_BLOB`, with a hash and the blob, `PUT_RESULT`, with a key and the result as it is stored, and
//!   `ADD_TO_MANIFEST`, with a key and the inputs and environment variables an edge discovered, as two lists of
//!   strings, are replied to with `OK`, or `ERROR` with why, as when the server does not take pushes, or what was sent
//!   is not valid.
//!
//! A blob larger than a part, a MiB, is sent in parts, whether it is a reply to `GET_BLOB` or the request `PUT_BLOB`:
//! a frame of the kind `MORE` with the id of the request for each part but the last, then the frame of the reply or
//! request, with the last part, so that the frames of other requests are sent between them, rather than after the
//! whole blob, and neither end holds the whole blob. The parts of a blob which is received are written to a file in the
//! cache as they are, hashed, and the file is added once it is whole and has its hash. A server which finds, as it
//! sends a blob, that it is corrupted, ends it with `NOT_FOUND`, and removes it.
//!
//! What is fetched is checked as what is read from a cache on disk is: a blob against its hash, and a manifest or
//! result against the hash which ends it, so that a server which sends something corrupted is a miss.
//!
//! A [`Remote`] asks for the manifest of a first key once, keeping what it found, or that it found none, for as long as
//! it is used, as a build only adds to a manifest what it pushes, which it has itself. It pushes in the background,
//! each result's blobs together, then the result and its manifest, and [`Remote::wait_for_pushes`] waits for what it
//! has been given to push. A remote which fails, as its connection is lost or the server replies with an error, is not
//! used again, so that the build goes on with the local cache, and [`Remote::error`] says why.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::cache::{Cache, Discovery, Hash};

const MAGIC: &[u8; 4] = b"RIGR";
const VERSION: u32 = 3;

const HELLO: u8 = 1;
const GET_MANIFEST: u8 = 2;
const GET_RESULTS: u8 = 3;
const GET_BLOB: u8 = 4;
const MISSING: u8 = 5;
const PUT_BLOB: u8 = 6;
const PUT_RESULT: u8 = 7;
const ADD_TO_MANIFEST: u8 = 8;
const FOUND: u8 = 64;
const NOT_FOUND: u8 = 65;
const OK: u8 = 66;
const ERROR: u8 = 67;
/// A part of a request or reply, which more frames of the same id follow, the last of which is of its kind.
const MORE: u8 = 68;

/// The largest part of a blob which is sent in a frame.
const PART: usize = 1 << 20;

/// The largest payload of a frame which is read, beyond which the stream is taken as corrupted.
const MAX_FRAME: u64 = 16 << 20;

/// The threads a server answers requests on.
const SERVER_THREADS: usize = 4;

/// A frame: its kind, the id of its request, and its payload.
type Frame = (u8, u32, Vec<u8>);

/// A reply: its kind and its payload.
type Reply = (u8, Vec<u8>);

/// What takes the parts of a reply which is sent in parts, as they are received.
type Sink = Box<dyn FnMut(&[u8]) + Send>;

/// A request waiting for its reply, with what takes the parts of it which come before, if it is sent in parts.
struct Waiter {
    reply: Sender<Reply>,
    sink: Option<Sink>,
}

/// The requests waiting for their replies, by id.
type Waiting = Arc<Mutex<HashMap<u32, Waiter>>>;

fn write_frame(w: &mut dyn Write, kind: u8, id: u32, payload: &[u8]) -> std::io::Result<()> {
    w.write_all(&[kind])?;
    w.write_all(&id.to_le_bytes())?;
    w.write_all(&(payload.len() as u64).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Returns the next frame, or `None` at the end of the stream, before a frame.
fn read_frame(r: &mut dyn Read) -> std::io::Result<Option<Frame>> {
    let mut kind = [0];
    match r.read_exact(&mut kind) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut header = [0; 12];
    r.read_exact(&mut header)?;
    let id = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = u64::from_le_bytes(header[4..].try_into().unwrap_or_default());
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("a frame of {len} bytes")));
    }
    let mut payload = vec![0; usize::try_from(len).map_err(std::io::Error::other)?];
    r.read_exact(&mut payload)?;
    Ok(Some((kind[0], id, payload)))
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

/// A connection to a server, which requests are sent on from any thread, and whose replies a thread of its own reads,
/// giving each to the request which waits for it.
struct Connection {
    writer: Mutex<Box<dyn Write + Send>>,
    waiting: Waiting,
    next: AtomicU32,
    /// Why the connection failed, once it has.
    failed: Arc<Mutex<Option<String>>>,
    /// Whether the server takes pushes.
    pushes: bool,
    process: Mutex<Option<Process>>,
}

/// The process which serves a connection, if one does, with what it writes to stderr, which is kept to tell why it
/// failed, and the thread which reads it.
struct Process {
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
    reading: Option<std::thread::JoinHandle<()>>,
}
impl Process {
    /// Returns the last line the process wrote to stderr, if it wrote one, once it has ended.
    fn stderr(&mut self) -> Option<String> {
        let _ = self.child.wait();
        if let Some(reading) = self.reading.take() {
            let _ = reading.join();
        }
        let stderr = self.stderr.lock().ok()?;
        let text = String::from_utf8_lossy(&stderr);
        text.lines().rev().find(|line| !line.trim().is_empty()).map(|line| line.trim().to_string())
    }
}

impl Connection {
    /// Start a connection over streams, whose replies are read on a thread of its own.
    fn new(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>, process: Option<Process>) -> Self {
        let waiting: Waiting = Arc::default();
        let failed: Arc<Mutex<Option<String>>> = Arc::default();
        let (replies, failure) = (Arc::clone(&waiting), Arc::clone(&failed));
        std::thread::spawn(move || {
            let mut reader = reader;
            let why = loop {
                match read_frame(&mut reader) {
                    Ok(Some((kind, id, payload))) => {
                        let waiter = replies.lock().ok().and_then(|mut waiting| waiting.remove(&id));
                        let Some(mut waiter) = waiter else {
                            continue;
                        };
                        // a part is given to the request's sink, without holding the lock, and the request waits for
                        // the rest
                        if kind == MORE {
                            if let Some(sink) = &mut waiter.sink {
                                sink(&payload);
                            }
                            if let Ok(mut waiting) = replies.lock() {
                                waiting.insert(id, waiter);
                            }
                            continue;
                        }
                        let _ = waiter.reply.send((kind, payload));
                    },
                    Ok(None) => break "the server closed the connection".to_string(),
                    Err(error) => break error.to_string(),
                }
            };
            if let Ok(mut failed) = failure.lock() {
                failed.get_or_insert(why);
            }
            // the requests which wait are given no reply, which fails them
            if let Ok(mut waiting) = replies.lock() {
                waiting.clear();
            }
        });
        Self { writer: Mutex::new(writer), waiting, next: AtomicU32::new(1), failed, pushes: false, process: Mutex::new(process) }
    }
    fn fail(&self, why: String) {
        if let Ok(mut failed) = self.failed.lock() {
            failed.get_or_insert(why);
        }
    }
    fn has_failed(&self) -> bool {
        self.failed.lock().map_or(true, |failed| failed.is_some())
    }
    /// Send a request, returning what its reply is given to, or `None` if the connection has failed.
    fn send(&self, kind: u8, payload: &[u8]) -> Option<Receiver<Reply>> {
        let (id, receiver) = self.register(None)?;
        self.write(kind, id, payload).then_some(receiver)
    }
    /// Send a request whose reply may come in parts, which are given to `sink` as they are received.
    fn send_to(&self, kind: u8, payload: &[u8], sink: Sink) -> Option<Receiver<Reply>> {
        let (id, receiver) = self.register(Some(sink))?;
        self.write(kind, id, payload).then_some(receiver)
    }
    /// Send a request of `head` followed by what is read from a file of a size, in parts, each written on its own, so
    /// that other requests are sent between them.
    fn send_parts(&self, kind: u8, head: &[u8], mut file: impl Read, size: u64) -> Option<Receiver<Reply>> {
        let (id, receiver) = self.register(None)?;
        let mut remaining = size;
        let mut part = vec![0; PART];
        while remaining > PART as u64 {
            if let Err(error) = file.read_exact(&mut part) {
                self.fail(error.to_string());
                return None;
            }
            if !self.write(MORE, id, &part) {
                return None;
            }
            remaining -= PART as u64;
        }
        let mut last = head.to_vec();
        if let Err(error) = file.read_to_end(&mut last) {
            self.fail(error.to_string());
            return None;
        }
        self.write(kind, id, &last).then_some(receiver)
    }
    /// Returns a new request's id, waiting for its reply, or `None` if the connection has failed.
    fn register(&self, sink: Option<Sink>) -> Option<(u32, Receiver<Reply>)> {
        if self.has_failed() {
            return None;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, receiver) = std::sync::mpsc::channel();
        self.waiting.lock().ok()?.insert(id, Waiter { reply, sink });
        Some((id, receiver))
    }
    /// Write a frame, returning whether it was, failing the connection if it could not be.
    fn write(&self, kind: u8, id: u32, payload: &[u8]) -> bool {
        match self.writer.lock().ok().map(|mut writer| write_frame(&mut **writer, kind, id, payload)) {
            Some(Ok(())) => true,
            Some(Err(error)) => {
                self.fail(error.to_string());
                false
            },
            None => false,
        }
    }
    /// Wait for the reply to a request, failing the connection if it is an error.
    fn wait(&self, receiver: Option<Receiver<Reply>>) -> Option<Reply> {
        let reply = receiver?.recv().ok()?;
        if reply.0 == ERROR {
            self.fail(format!("the server failed: {}", String::from_utf8_lossy(&reply.1)));
            return None;
        }
        Some(reply)
    }
    fn request(&self, kind: u8, payload: &[u8]) -> Option<Reply> {
        self.wait(self.send(kind, payload))
    }
    /// Returns why the connection failed, with the last line its process wrote to stderr, once it has ended.
    fn failure(&self) -> Option<String> {
        let why = self.failed.lock().ok()?.clone()?;
        // the server ends once its stdin does
        if let Ok(mut writer) = self.writer.lock() {
            *writer = Box::new(std::io::sink());
        }
        let stderr = self.process.lock().ok().and_then(|mut process| process.as_mut()?.stderr());
        Some(match stderr {
            Some(stderr) => format!("{why}: {stderr}"),
            None => why,
        })
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            *writer = Box::new(std::io::sink());
        }
        if let Ok(Some(process)) = self.process.get_mut().map(Option::as_mut) {
            let _ = process.child.wait();
        }
    }
}

enum State {
    /// Not connected yet, with the streams to connect over if they were given rather than a command.
    Unconnected(Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)>),
    Connected(Arc<Connection>),
    /// It could not connect, for this reason.
    Failed(String),
}

/// A result to push, from the local cache, which has it, by its keys, with what its edge discovered.
struct Push {
    local: Cache,
    first: Hash,
    second: Hash,
    discovery: Discovery,
}

/// A remote cache, which is connected to when it is first used.
pub struct Remote {
    /// The program and arguments which run the server, speaking the protocol on its stdin and stdout.
    command: Vec<String>,
    /// Whether results are pushed to it, if it takes them.
    push: bool,
    state: Mutex<State>,
    /// The manifests found, or not, by first key, which are asked for once.
    manifests: Mutex<HashMap<Hash, Option<Vec<u8>>>>,
    /// The queue of results to push, and the thread which pushes them, once one has been given.
    pusher: Mutex<Option<(Sender<Push>, std::thread::JoinHandle<()>)>>,
}
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Remote").field("command", &self.command).field("push", &self.push).finish_non_exhaustive()
    }
}

impl Remote {
    fn with_state(command: Vec<String>, push: bool, state: State) -> Self {
        Self { command, push, state: Mutex::new(state), manifests: Mutex::default(), pusher: Mutex::default() }
    }
    /// A remote cache which a command serves on its stdin and stdout, such as `ssh host jig serve --stdio`, which
    /// results are pushed to with `push`, if it takes them.
    #[must_use]
    pub fn command(command: Vec<String>, push: bool) -> Self {
        Self::with_state(command, push, State::Unconnected(None))
    }
    /// A remote cache which is served over streams, as by [`serve`] on another thread.
    #[must_use]
    pub fn streams(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, push: bool) -> Self {
        let streams: (Box<dyn Read + Send>, Box<dyn Write + Send>) = (Box::new(reader), Box::new(writer));
        Self::with_state(Vec::new(), push, State::Unconnected(Some(streams)))
    }
    /// Returns why the remote cache failed, if it did, after which it is not used.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        match &*self.state.lock().ok()? {
            State::Failed(error) => Some(error.clone()),
            State::Connected(connection) => connection.failure(),
            State::Unconnected(_) => None,
        }
    }
    /// Whether results are pushed to the remote cache, as it is asked to and it takes them, connecting to it to find
    /// out.
    #[must_use]
    pub fn pushes(&self) -> bool {
        self.push && self.connection().is_some_and(|connection| connection.pushes)
    }
    /// Returns the connection, connecting first if it is not, or `None` if it failed.
    fn connection(&self) -> Option<Arc<Connection>> {
        let mut state = self.state.lock().ok()?;
        if let State::Unconnected(streams) = &mut *state {
            *state = match self.connect(streams.take()) {
                Ok(connection) => State::Connected(Arc::new(connection)),
                Err(error) => State::Failed(error),
            };
        }
        match &*state {
            State::Connected(connection) if !connection.has_failed() => Some(Arc::clone(connection)),
            _ => None,
        }
    }
    fn connect(&self, streams: Option<(Box<dyn Read + Send>, Box<dyn Write + Send>)>) -> Result<Connection, String> {
        let mut connection = if let Some((reader, writer)) = streams {
            Connection::new(reader, writer, None)
        } else {
            let (program, args) = self.command.split_first().ok_or("no command for the remote cache")?;
            let mut command = Command::new(program);
            command.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = command.spawn().map_err(|error| format!("failed to run `{program}`: {error}"))?;
            let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
            let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
                return Err(format!("`{program}` has no stdin or stdout"));
            };
            // stderr is read as it is written, so that the process does not block on it
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
            let process = Process { child, stderr, reading };
            Connection::new(Box::new(BufReader::new(stdout)), Box::new(BufWriter::new(stdin)), Some(process))
        };
        let mut hello = MAGIC.to_vec();
        hello.extend_from_slice(&VERSION.to_le_bytes());
        let receiver = connection.send(HELLO, &hello);
        match receiver.and_then(|receiver| receiver.recv().ok()) {
            Some((HELLO, payload)) if payload.len() == 5 && payload[..4] == VERSION.to_le_bytes() => {
                connection.pushes = payload[4] == 1;
                Ok(connection)
            },
            Some((ERROR, message)) => Err(String::from_utf8_lossy(&message).into_owned()),
            Some(_) => Err("the server does not speak rig's protocol".into()),
            None => {
                connection.fail("the server closed the connection".into());
                Err(connection.failure().unwrap_or_default())
            },
        }
    }
    /// Returns what was found by a request whose reply is `FOUND` or `NOT_FOUND`, or `None` if neither.
    fn get(connection: &Connection, reply: Option<Reply>) -> Option<Vec<u8>> {
        match reply? {
            (FOUND, payload) => Some(payload),
            (NOT_FOUND, _) => None,
            _ => {
                connection.fail("an unexpected reply from the server".into());
                None
            },
        }
    }
    /// Run a request whose reply is `OK`, returning whether it was.
    fn put(connection: &Connection, reply: Option<u8>) -> bool {
        match reply {
            Some(OK) => true,
            Some(_) => {
                connection.fail("an unexpected reply from the server".into());
                false
            },
            None => false,
        }
    }
    /// Returns a manifest as it is stored, if the remote cache has it, which is asked for once.
    #[must_use]
    pub fn manifest(&self, key: &Hash) -> Option<Vec<u8>> {
        if let Some(found) = self.manifests.lock().ok()?.get(key) {
            return found.clone();
        }
        let connection = self.connection()?;
        let reply = connection.request(GET_MANIFEST, key);
        // a manifest which could not be asked for is asked for again, while one which was not found is not
        reply.as_ref()?;
        let found = Self::get(&connection, reply);
        self.manifests.lock().ok()?.insert(*key, found.clone());
        found
    }
    /// Returns the first of the results by second keys which the remote cache has, with every blob it refers to, with
    /// its index, as it is stored.
    #[must_use]
    pub fn first_result(&self, keys: &[Hash]) -> Option<(usize, Vec<u8>)> {
        if keys.is_empty() {
            return None;
        }
        let connection = self.connection()?;
        let found = Self::get(&connection, connection.request(GET_RESULTS, &hashes(keys)))?;
        let mut payload = Payload(&found);
        let index = payload.u32()? as usize;
        (index < keys.len()).then(|| (index, payload.0.to_vec()))
    }
    /// Fetch blobs into a cache, asking for them at once, returning those which were added. The parts of each are
    /// written to the cache as they are received, and each is added once it is whole and has its hash.
    pub fn fetch_blobs(&self, local: &Cache, blobs: &[Hash]) -> Vec<Hash> {
        let Some(connection) = self.connection() else {
            return Vec::new();
        };
        let mut sent = Vec::new();
        for blob in blobs {
            let Ok(writer) = local.blob_writer() else {
                continue;
            };
            let writer = Arc::new(Mutex::new(Some(writer)));
            let parts = Arc::clone(&writer);
            let sink: Sink = Box::new(move |part| {
                if let Ok(mut writer) = parts.lock()
                    && let Some(writer) = writer.as_mut()
                {
                    writer.write(part);
                }
            });
            sent.push((blob, writer, connection.send_to(GET_BLOB, blob, sink)));
        }
        let mut fetched = Vec::new();
        for (blob, writer, receiver) in sent {
            // once the reply has come, the sink has been given every part before it
            let last = Self::get(&connection, connection.wait(receiver));
            let writer = writer.lock().ok().and_then(|mut writer| writer.take());
            if let Some((last, mut writer)) = last.zip(writer) {
                writer.write(&last);
                if writer.finish(blob) {
                    fetched.push(*blob);
                }
            }
        }
        fetched
    }
    /// Returns those of the blobs the remote cache does not have, or `None` if it failed.
    #[must_use]
    pub fn missing(&self, blobs: &[Hash]) -> Option<Vec<Hash>> {
        let connection = self.connection()?;
        if let (MISSING, payload) = connection.request(MISSING, &hashes(blobs))? {
            return Payload(&payload).hashes();
        }
        connection.fail("an unexpected reply from the server".into());
        None
    }
    /// Push a blob, returning whether it was taken.
    #[must_use]
    pub fn put_blob(&self, hash: &Hash, contents: &[u8]) -> bool {
        self.connection().is_some_and(|connection| {
            let receiver = connection.send_parts(PUT_BLOB, hash, contents, contents.len() as u64);
            Self::put(&connection, connection.wait(receiver).map(|(kind, _)| kind))
        })
    }
    /// Queue a result in a local cache to be pushed, by its keys, with what its edge discovered, if the remote cache
    /// takes pushes, returning whether it was queued. It is pushed in the background, in the order queued.
    pub fn queue_push(&self, local: Cache, first: &Hash, second: &Hash, discovery: &Discovery) -> bool {
        if !self.pushes() {
            return false;
        }
        let Some(connection) = self.connection() else {
            return false;
        };
        let Ok(mut pusher) = self.pusher.lock() else {
            return false;
        };
        let (sender, _) = pusher.get_or_insert_with(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<Push>();
            let pushing = std::thread::spawn(move || {
                for push in receiver {
                    push_result(&connection, &push);
                }
            });
            (sender, pushing)
        });
        let push = Push { local, first: *first, second: *second, discovery: discovery.clone() };
        sender.send(push).is_ok()
    }
    /// Wait until what has been queued has been pushed, or failed to be.
    pub fn wait_for_pushes(&self) {
        let pusher = self.pusher.lock().ok().and_then(|mut pusher| pusher.take());
        if let Some((sender, pushing)) = pusher {
            drop(sender);
            let _ = pushing.join();
        }
    }
}

/// Push a result: the blobs the remote cache does not have, at once, then the result and the discovered inputs of
/// its manifest, so that it never has a result without its blobs.
fn push_result(connection: &Connection, push: &Push) {
    let Some(result) = push.local.raw_result(&push.second) else {
        return;
    };
    let Some(blobs) = crate::cache::result_blobs(&result) else {
        return;
    };
    let Some(Some(missing)) = connection.request(MISSING, &hashes(&blobs)).map(|(kind, payload)| {
        (kind == MISSING).then(|| Payload(&payload).hashes()).flatten()
    }) else {
        return;
    };
    // each is sent in parts, read from its file as it is sent
    let mut sent = Vec::new();
    for blob in &missing {
        let Ok((file, size)) = push.local.open_blob(blob) else {
            return;
        };
        sent.push(connection.send_parts(PUT_BLOB, blob, std::io::BufReader::new(file), size));
    }
    if !sent.into_iter().all(|receiver| Remote::put(connection, connection.wait(receiver).map(|(kind, _)| kind))) {
        return;
    }
    let mut manifest = push.first.to_vec();
    push_strs(&mut manifest, &push.discovery.inputs);
    push_strs(&mut manifest, &push.discovery.env);
    let result = connection.send(PUT_RESULT, &[push.second.as_slice(), &result].concat());
    let manifest = connection.send(ADD_TO_MANIFEST, &manifest);
    let kind = |receiver| connection.wait(receiver).map(|(kind, _)| kind);
    let _ = Remote::put(connection, kind(result)) && Remote::put(connection, kind(manifest));
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.wait_for_pushes();
    }
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Serve a cache over streams, as `jig serve --stdio` does on its stdin and stdout, until the client ends the stream,
/// taking pushes if `push`, answering requests on several threads.
///
/// # Errors
///
/// If the streams cannot be read or written, or the client does not speak the protocol.
pub fn serve(cache: &Cache, reader: impl Read, writer: impl Write + Send, push: bool) -> std::io::Result<()> {
    let (mut reader, mut writer) = (BufReader::new(reader), BufWriter::new(writer));
    let Ok(Some((HELLO, id, hello))) = read_frame(&mut reader) else {
        return Err(invalid("the client does not speak rig's protocol"));
    };
    let mut expected = MAGIC.to_vec();
    expected.extend_from_slice(&VERSION.to_le_bytes());
    if hello != expected {
        let message = format!("the server speaks version {VERSION} of rig's protocol, which the client does not");
        write_frame(&mut writer, ERROR, id, message.as_bytes())?;
        return Err(invalid(&message));
    }
    let mut reply = VERSION.to_le_bytes().to_vec();
    reply.push(u8::from(push));
    write_frame(&mut writer, HELLO, id, &reply)?;
    let writer = Mutex::new(writer);
    let (requests, work) = std::sync::mpsc::channel::<Frame>();
    let work = Mutex::new(work);
    let written: Mutex<std::io::Result<()>> = Mutex::new(Ok(()));
    let reply = |kind: u8, id: u32, payload: &[u8]| {
        let result = writer.lock().map_or(Ok(()), |mut writer| write_frame(&mut *writer, kind, id, payload));
        if let Err(error) = result
            && let Ok(mut written) = written.lock()
        {
            *written = Err(error);
        }
    };
    std::thread::scope(|scope| {
        for _ in 0..SERVER_THREADS {
            scope.spawn(|| {
                // the lock is held while waiting for a request, which the other threads wait for in turn
                while let Ok(Ok((kind, id, payload))) = work.lock().map(|work| work.recv()) {
                    if kind == GET_BLOB && let Ok(key) = <Hash>::try_from(payload.as_slice()) {
                        send_blob(cache, &key, &|kind, payload| reply(kind, id, payload));
                        continue;
                    }
                    let (kind, payload) = answer(cache, push, kind, &payload);
                    reply(kind, id, &payload);
                }
            });
        }
        // the parts of the blobs being pushed, by the ids of their requests, which are written as they are read, or
        // `None` for a blob which is not taken
        let mut uploads: HashMap<u32, Option<crate::cache::BlobWriter>> = HashMap::new();
        let read = loop {
            match read_frame(&mut reader) {
                Ok(Some((MORE, id, part))) => {
                    let upload = uploads.entry(id).or_insert_with(|| cache.blob_writer().ok().filter(|_| push));
                    if let Some(upload) = upload {
                        upload.write(&part);
                    }
                },
                Ok(Some((PUT_BLOB, id, payload))) if push => {
                    let (hash, last) = payload.split_at(payload.len().min(32));
                    let upload = uploads.remove(&id).unwrap_or_else(|| cache.blob_writer().ok());
                    let added = <Hash>::try_from(hash).ok().zip(upload).is_some_and(|(hash, mut upload)| {
                        upload.write(last);
                        upload.finish(&hash)
                    });
                    if added {
                        reply(OK, id, &[]);
                    } else {
                        reply(ERROR, id, b"a blob which does not have its hash");
                    }
                },
                Ok(Some((kind, id, payload))) => {
                    uploads.remove(&id);
                    if requests.send((kind, id, payload)).is_err() {
                        break Ok(());
                    }
                },
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        // the threads stop once there are no more requests
        drop(requests);
        read
    })?;
    written.into_inner().unwrap_or(Ok(()))
}

/// Send a blob as the reply to a request, by `reply`, in parts, as it is read, checking it against its hash, and
/// ending it with `NOT_FOUND`, and removing it, if it does not have it, or with `NOT_FOUND` alone if the cache does not
/// have it.
fn send_blob(cache: &Cache, hash: &Hash, reply: &dyn Fn(u8, &[u8])) {
    let Ok((file, size)) = cache.open_blob(hash) else {
        return reply(NOT_FOUND, &[]);
    };
    let mut file = std::io::BufReader::new(file);
    let mut hasher = crate::blake3::Hasher::new();
    let mut remaining = size;
    let mut part = vec![0; PART];
    while remaining > PART as u64 {
        if file.read_exact(&mut part).is_err() {
            return reply(NOT_FOUND, &[]);
        }
        hasher.update(&part);
        reply(MORE, &part);
        remaining -= PART as u64;
    }
    let mut last = Vec::new();
    if file.read_to_end(&mut last).is_err() {
        return reply(NOT_FOUND, &[]);
    }
    hasher.update(&last);
    if hasher.finalize() != *hash {
        cache.remove_blob(hash);
        return reply(NOT_FOUND, &[]);
    }
    reply(FOUND, &last);
}

/// Returns the reply to a request.
fn answer(cache: &Cache, push: bool, kind: u8, payload: &[u8]) -> (u8, Vec<u8>) {
    let error = |message: &str| (ERROR, message.as_bytes().to_vec());
    let found = |found: Option<Vec<u8>>| found.map_or((NOT_FOUND, Vec::new()), |found| (FOUND, found));
    let mut p = Payload(payload);
    if matches!(kind, PUT_BLOB | PUT_RESULT | ADD_TO_MANIFEST) && !push {
        return error("the server does not take pushes");
    }
    if matches!(kind, MISSING | GET_RESULTS) {
        let Some(keys) = p.hashes() else {
            return error("an invalid list of keys");
        };
        if kind == MISSING {
            return (MISSING, hashes(&keys.into_iter().filter(|blob| !cache.has_blob(blob)).collect::<Vec<_>>()));
        }
        let first = keys.iter().enumerate().find_map(|(index, key)| Some((index, cache.raw_result(key)?)));
        return found(first.map(|(index, result)| [(index as u32).to_le_bytes().as_slice(), &result].concat()));
    }
    let Some(key) = p.hash() else {
        return error("a request without a key");
    };
    match kind {
        GET_MANIFEST => found(cache.raw_manifest(&key)),
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
        // in the background, which is waited for
        pusher.wait_for_pushes();
        assert_eq!(server.result(&second).map(|entry| entry.outputs.len()), Some(1));
        assert_eq!(server.manifest(&first), std::slice::from_ref(&discovery));
        assert_eq!(server.stats().blobs, 2);
        // and pushing it again sends no blobs, as it has them
        assert_eq!(pusher.remote().unwrap().missing(&[crate::blake3::hash(b"contents")]), Some(Vec::new()));
        assert!(pusher.push(&first, &second, &discovery));
        pusher.wait_for_pushes();
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
            while let Ok(Some((kind, id, _))) = read_frame(&mut requests) {
                let reply: &[u8] = if kind == HELLO { &[3, 0, 0, 0, 0] } else { b"corrupted" };
                let _ = write_frame(&mut replies, if kind == HELLO { HELLO } else { FOUND }, id, reply);
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
    fn gives_each_reply_to_its_request() {
        // a server which answers two requests for blobs the other way round
        let (a, b) = (crate::blake3::hash(b"a"), crate::blake3::hash(b"b"));
        let (requests, client) = std::io::pipe().unwrap();
        let (server, mut replies) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let mut requests = requests;
            let Ok(Some((HELLO, id, _))) = read_frame(&mut requests) else { return };
            let _ = write_frame(&mut replies, HELLO, id, &[3, 0, 0, 0, 0]);
            let mut pending = Vec::new();
            while let Ok(Some((_, id, payload))) = read_frame(&mut requests) {
                pending.push((id, if payload == a { b"a" } else { b"b" }));
                if pending.len() == 2 {
                    for (id, contents) in pending.drain(..).rev() {
                        let _ = write_frame(&mut replies, FOUND, id, contents);
                    }
                }
            }
        });
        let remote = Remote::streams(server, client, false);
        let local = cache("reply-order");
        assert_eq!(remote.fetch_blobs(&local, &[a, b]), [a, b]);
        assert_eq!((local.read_blob(&a).unwrap(), local.read_blob(&b).unwrap()), (b"a".to_vec(), b"b".to_vec()));
        assert!(remote.error().is_none());
        let _ = std::fs::remove_dir_all(local.dir());
    }

    #[test]
    fn answers_requests_from_many_threads() {
        let server = cache("concurrent");
        let blobs: Vec<Hash> = (0..64).map(|i| server.put_bytes(format!("blob {i}").as_bytes()).unwrap()).collect();
        let remote = Arc::new(served(server.clone(), false, false));
        let local = cache("concurrent-local");
        std::thread::scope(|scope| {
            for chunk in blobs.chunks(8) {
                let (remote, local) = (Arc::clone(&remote), &local);
                scope.spawn(move || assert_eq!(remote.fetch_blobs(local, chunk), chunk));
            }
        });
        assert!(remote.error().is_none());
        assert_eq!(local.stats().blobs, 64);
        for cache in [server, local] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    /// Returns bytes of a length which differ from part to part, so that parts which are swapped or repeated show.
    fn large(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i / 7 % 251) as u8 ^ seed).collect()
    }

    #[test]
    fn sends_large_blobs_in_parts() {
        let server = cache("large-server");
        // larger than several parts, and not a whole number of them
        let fetched = large(3 * PART + 12_345, 1);
        let hash = server.put_bytes(&fetched).unwrap();
        let remote = Arc::new(served(server.clone(), true, true));
        let local = cache("large-local");
        // fetched with small blobs, which are asked for at once with it
        let small: Vec<Hash> = (0..8).map(|i| server.put_bytes(format!("small {i}").as_bytes()).unwrap()).collect();
        let all: Vec<Hash> = std::iter::once(hash).chain(small.iter().copied()).collect();
        assert_eq!(remote.fetch_blobs(&local, &all), all);
        assert_eq!(local.read_blob(&hash).unwrap(), fetched);
        // and pushed in parts
        let pushed = large(2 * PART + 1, 2);
        let pushed_hash = crate::blake3::hash(&pushed);
        assert!(remote.put_blob(&pushed_hash, &pushed));
        assert_eq!(server.read_blob(&pushed_hash).unwrap(), pushed);
        // one exactly a part long is sent whole
        let exact = large(PART, 3);
        assert!(remote.put_blob(&crate::blake3::hash(&exact), &exact));
        // a blob which is not what its hash says is refused, which fails the remote
        assert!(!remote.put_blob(&[0; 32], &pushed));
        assert!(!server.has_blob(&[0; 32]));
        assert!(remote.error().unwrap().contains("does not have its hash"));
        assert_eq!(std::fs::read_dir(server.dir().join("tmp")).unwrap().count(), 0);
        for cache in [server, local] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn ends_a_blob_which_is_corrupted_as_it_is_sent() {
        let server = cache("large-corrupted");
        let contents = large(2 * PART + 100, 4);
        let hash = server.put_bytes(&contents).unwrap();
        // corrupted after its first part, which is sent before it is found to be
        let mut corrupted = contents.clone();
        corrupted[PART + 10] ^= 1;
        std::fs::write(server.blob_path(&hash), &corrupted).unwrap();
        let remote = served(server.clone(), false, false);
        let local = cache("large-corrupted-local");
        assert!(remote.fetch_blobs(&local, &[hash]).is_empty());
        assert!(!local.has_blob(&hash) && !server.has_blob(&hash));
        assert_eq!(std::fs::read_dir(local.dir().join("tmp")).unwrap().count(), 0);
        assert!(remote.error().is_none());
        for cache in [server, local] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn asks_for_a_manifest_once() {
        let (first, second) = (crate::blake3::hash(b"first"), crate::blake3::hash(b"second"));
        let server = cache("manifest-once");
        add(&server, &first, &second, "contents", &Discovery::default());
        let remote = served(server.clone(), false, false);
        assert!(remote.manifest(&first).is_some());
        let other = crate::blake3::hash(b"other");
        assert!(remote.manifest(&other).is_none());
        // what it found, or did not, is kept, rather than asked for again
        let _ = std::fs::remove_dir_all(server.dir().join("mf"));
        server.add_to_manifest(&other, Discovery::default()).unwrap();
        assert!(remote.manifest(&first).is_some());
        assert!(remote.manifest(&other).is_none());
        let _ = std::fs::remove_dir_all(server.dir());
    }

    #[test]
    fn finds_by_every_set_of_inputs_at_once() {
        // a cache whose manifest has none of the sets the remote has finds the remote's result, and keeps the set
        let first = crate::blake3::hash(b"first");
        let discovery = |name: &str| Discovery { inputs: vec![name.into()], env: Vec::new() };
        let second = |discovery: &Discovery| crate::blake3::hash(discovery.inputs[0].as_bytes());
        let server = cache("find-server");
        add(&server, &first, &second(&discovery("old.h")), "old", &discovery("old.h"));
        add(&server, &first, &second(&discovery("new.h")), "new", &discovery("new.h"));
        let client = cache("find-client").with_remote(Arc::new(served(server.clone(), false, false)));
        client.add_to_manifest(&first, discovery("local.h")).unwrap();
        let (key, entry) = client.find(&first, second).unwrap();
        assert_eq!(key, second(&discovery("new.h")));
        assert_eq!(client.read_blob(&entry.outputs[0].blob).unwrap(), b"new");
        // which it has itself once its blobs are fetched, as they are when it is restored
        assert_eq!(client.read_blob(&entry.printed).unwrap(), b"printed");
        assert!(client.without_remote().find(&first, second).is_some());
        for cache in [server, client] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn stops_using_a_remote_which_fails() {
        let remote = Remote::command(vec!["rig-no-such-server".into()], true);
        assert!(remote.manifest(&[0; 32]).is_none());
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
        write_frame(&mut hello, HELLO, 0, &request).unwrap();
        let mut reply = Vec::new();
        assert!(serve(&dir, hello.as_slice(), &mut reply, false).is_err());
        let (kind, _, message) = read_frame(&mut reply.as_slice()).unwrap().unwrap();
        assert_eq!(kind, ERROR);
        assert!(String::from_utf8(message).unwrap().contains("version"));
        let _ = std::fs::remove_dir_all(dir.dir());
    }
}
