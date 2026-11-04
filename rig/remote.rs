//! A remote cache: a [`Cache`] on another machine, or in another process, spoken to over a stream of bytes, such as
//! the stdin and stdout of `ssh host jig serve --stdio`, which [`serve`] answers, or a store of files served over
//! HTTP, by [`crate::http`]. A server of the stream's protocol can also be a worker, which runs actions, as
//! [`crate::worker`] describes.
//!
//! Over HTTP, a blob is at `<url>/cas/<the first two digits of its hash>/<the rest>`, a result at `<url>/ac/<key>` and a
//! manifest at `<url>/mf/<key>`, in hexadecimal, as they are in a cache's directory, so that a server of the files of
//! one serves it as it is, which are got with `GET`, and pushed with `PUT`, after
//! `HEAD` finds which blobs the server does not have. A manifest is pushed merged with the server's, which a push from
//! elsewhere at once may replace, which loses one set of inputs or the other, as a miss. Requests from several threads,
//! such as for the blobs of a result, go at once, on connections of their own.
//!
//! The protocol is of requests and their replies, each a frame: its kind as a byte, the id of the request as a
//! little-endian `u32`, which its reply has, the length of its payload as a little-endian `u64`, then the payload.
//! Requests are not answered in turn: a client sends as many as it has, from as many threads as it has, and the server
//! answers them on several threads, in any order, so that one connection, such as one ssh session, carries the
//! requests of a whole build without one waiting for another's round trip. Hashes and keys are 32 bytes, numbers are
//! little-endian `u32`s and strings are their length followed by their UTF-8 bytes.
//!
//! - The client sends `HELLO`, with the magic bytes and the version, and the server replies `HELLO`, with the version,
//!   whether it takes pushes, `1` or `0`, the number of actions it runs at once as a worker, or `0` if it is not one,
//!   and whether it needs a token, `1` followed by a random nonce of 32 bytes,
//!   or `0`, or `ERROR` with why it does not speak the version.
//! - A server which needs a token, as one which is reached over TCP does, answers nothing until the client sends
//!   `AUTH`, with the BLAKE3 hash of the token and the nonce, which it replies to with `OK`, or `ERROR`, ending the
//!   connection, if it is not of its token. The token itself is never sent, and an answer which is overheard is of no
//!   use with another nonce. The connection is not encrypted, so a remote cache reached over TCP belongs on a private
//!   network, while ssh both authenticates and encrypts.
//! - `GET_MANIFEST` and `GET_BLOB`, with a key or hash, are replied to with `FOUND` and the manifest as it is stored,
//!   or the blob, or `NOT_FOUND`.
//! - `GET_RESULTS`, with a number of second keys, is replied to with `FOUND`, the index of the first key whose result
//!   the server has, with every blob it refers to, and the result as it is stored, or `NOT_FOUND`, so that every set of
//!   inputs an edge may have discovered is looked up at once.
//! - `MISSING`, with a number of hashes, is replied to with `MISSING` and those of them the server does not have.
//! - `PUT_BLOB`, with a hash and the blob, `PUT_RESULT`, with a key and the result as it is stored, and
//!   `ADD_TO_MANIFEST`, with a key and the inputs and environment variables an edge discovered, as two lists of
//!   strings, are replied to with `OK`, or `ERROR` with why, as when the server does not take pushes, or what was sent
//!   is not valid. A worker takes blobs, as the inputs of the actions it runs, though not results or manifests, unless
//!   it takes pushes.
//! - `EXECUTE`, with a [`crate::worker::Request`], is replied to by a worker with `FOUND`, whether the action
//!   succeeded, `1` or `0`, and its result as a cache stores one, whose blobs the worker has, `NOT_FOUND` if it does
//!   not have the toolchain, or `ERROR` with why it could not run the action. Actions run on threads of their own, so
//!   that requests for blobs are answered while they run.
//! - `GET_CAPABILITIES` is replied to by a worker with `FOUND` and its [`crate::worker::Capabilities`].
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
use crate::worker::{Executed, Refused, Request};

const MAGIC: &[u8; 4] = b"RIGR";
const VERSION: u32 = 6;

const HELLO: u8 = 1;
const GET_MANIFEST: u8 = 2;
const GET_RESULTS: u8 = 3;
const GET_BLOB: u8 = 4;
const MISSING: u8 = 5;
const PUT_BLOB: u8 = 6;
const PUT_RESULT: u8 = 7;
const ADD_TO_MANIFEST: u8 = 8;
const AUTH: u8 = 9;
const EXECUTE: u8 = 10;
const GET_CAPABILITIES: u8 = 11;
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

pub(crate) fn push_str(bytes: &mut Vec<u8>, string: &str) {
    bytes.extend_from_slice(&(string.len() as u32).to_le_bytes());
    bytes.extend_from_slice(string.as_bytes());
}

pub(crate) fn push_strs(bytes: &mut Vec<u8>, strings: &[String]) {
    bytes.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    for string in strings {
        push_str(bytes, string);
    }
}

/// Reads the parts of a payload.
pub(crate) struct Payload<'a>(pub(crate) &'a [u8]);
impl<'a> Payload<'a> {
    pub(crate) fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(taken)
    }
    pub(crate) fn hash(&mut self) -> Option<Hash> {
        self.take(32)?.try_into().ok()
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub(crate) fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        Some(std::str::from_utf8(self.take(len)?).ok()?.to_string())
    }
    pub(crate) fn strs(&mut self) -> Option<Vec<String>> {
        (0..self.u32()?).map(|_| self.str()).collect()
    }
    pub(crate) fn hashes(&mut self) -> Option<Vec<Hash>> {
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
    /// The number of actions the server runs at once, as a worker, or `0` if it is not one.
    jobs: usize,
    process: Mutex<Option<Process>>,
    /// The socket of a connection over TCP, which is shut down to end it, as the thread which reads it holds it too.
    socket: Option<std::net::TcpStream>,
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
        let process = Mutex::new(process);
        let writer = Mutex::new(writer);
        Self { writer, waiting, next: AtomicU32::new(1), failed, pushes: false, jobs: 0, process, socket: None }
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
        self.close();
        let stderr = self.process.lock().ok().and_then(|mut process| process.as_mut()?.stderr());
        Some(match stderr {
            Some(stderr) => format!("{why}: {stderr}"),
            None => why,
        })
    }
}
impl Connection {
    /// End the connection: close what is written to the server, which it ends once it reads to the end of, and shut
    /// down its socket, if it has one.
    fn close(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.flush();
            *writer = Box::new(std::io::sink());
        }
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
        if let Ok(Some(process)) = self.process.get_mut().map(Option::as_mut) {
            let _ = process.child.wait();
        }
    }
}

/// How a remote cache is reached.
#[derive(Debug)]
enum Transport {
    /// A server of files over HTTP, which `http` speaks to.
    Http,
    /// A program and arguments which run the server, speaking the protocol on its stdin and stdout.
    Command(Vec<String>),
    /// The address of a server which listens for connections over TCP, as `<host>:<port>`.
    Tcp(String),
    /// Streams which were given, as to a server on another thread.
    Streams,
}

enum State {
    /// Not connected yet, with the streams to connect over if they were given rather than a transport.
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
    transport: Transport,
    /// The token which proves the client to a server which needs one.
    token: Option<Vec<u8>>,
    /// Whether results are pushed to it, if it takes them.
    push: bool,
    state: Mutex<State>,
    /// The manifests found, or not, by first key, which are asked for once.
    manifests: Mutex<HashMap<Hash, Option<Vec<u8>>>>,
    /// The queue of results to push, and the thread which pushes them, once one has been given.
    pusher: Mutex<Option<(Sender<Push>, std::thread::JoinHandle<()>)>>,
    /// The client of a server over HTTP, with why it failed, once it has, if the remote cache is one.
    http: Option<Arc<Http>>,
    /// The identities of the toolchains which the server, as a worker, does not have.
    refused: Mutex<std::collections::HashSet<String>>,
}

/// A server of files over HTTP, and why it failed, once it has, after which it is not used.
struct Http {
    client: crate::http::Client,
    failed: Mutex<Option<String>>,
}
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Remote").field("transport", &self.transport).field("push", &self.push).finish_non_exhaustive()
    }
}

impl Remote {
    fn with_state(transport: Transport, push: bool, state: State) -> Self {
        let (manifests, pusher, refused) = (Mutex::default(), Mutex::default(), Mutex::default());
        Self { transport, token: None, push, state: Mutex::new(state), manifests, pusher, http: None, refused }
    }
    /// A remote cache which a command serves on its stdin and stdout, such as `ssh host jig serve --stdio`, which
    /// results are pushed to with `push`, if it takes them.
    #[must_use]
    pub fn command(command: Vec<String>, push: bool) -> Self {
        Self::with_state(Transport::Command(command), push, State::Unconnected(None))
    }
    /// A remote cache which a server listening at an address, `<host>:<port>`, serves over TCP, as [`listen`] does.
    #[must_use]
    pub fn tcp(address: String, push: bool) -> Self {
        Self::with_state(Transport::Tcp(address), push, State::Unconnected(None))
    }
    /// A remote cache which is served over streams, as by [`serve`] on another thread.
    #[must_use]
    pub fn streams(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static, push: bool) -> Self {
        let streams: (Box<dyn Read + Send>, Box<dyn Write + Send>) = (Box::new(reader), Box::new(writer));
        Self::with_state(Transport::Streams, push, State::Unconnected(Some(streams)))
    }
    /// A remote cache which is a store of files served over HTTP, under a URL, which results are pushed to with
    /// `push`, which authenticates with a bearer token, if there is one, or else the URL's user and password.
    #[must_use]
    pub fn http(url: crate::http::Url, token: Option<&[u8]>, push: bool) -> Self {
        let mut remote = Self::with_state(Transport::Http, push, State::Unconnected(None));
        let client = crate::http::Client::new(url, token);
        remote.http = Some(Arc::new(Http { client, failed: Mutex::default() }));
        remote
    }
    /// Prove the client with a token, to a server which needs one.
    #[must_use]
    pub fn with_token(mut self, token: Vec<u8>) -> Self {
        self.token = Some(token);
        self
    }
    /// Returns why the remote cache failed, if it did, after which it is not used.
    #[must_use]
    pub fn error(&self) -> Option<String> {
        if let Some(http) = &self.http {
            return http.failed.lock().ok()?.clone();
        }
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
        if let Some(http) = &self.http {
            return self.push && !http.has_failed();
        }
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
        } else if let Transport::Tcp(address) = &self.transport {
            let stream = connect_tcp(address).map_err(|error| format!("failed to connect to {address}: {error}"))?;
            let clone = |stream: &std::net::TcpStream| stream.try_clone().map_err(|error| error.to_string());
            let (reader, socket) = (clone(&stream)?, clone(&stream)?);
            let mut connection = Connection::new(Box::new(BufReader::new(reader)), Box::new(BufWriter::new(stream)), None);
            connection.socket = Some(socket);
            connection
        } else {
            let Transport::Command(command) = &self.transport else {
                return Err("no way to reach the remote cache".into());
            };
            let (program, args) = command.split_first().ok_or("no command for the remote cache")?;
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
            Some((HELLO, payload)) if payload.len() >= 10 && payload[..4] == VERSION.to_le_bytes() => {
                connection.pushes = payload[4] == 1;
                connection.jobs = u32::from_le_bytes([payload[5], payload[6], payload[7], payload[8]]) as usize;
                if payload[9] == 1 {
                    let nonce = payload.get(10..42).ok_or("the server sent no nonce")?;
                    let token = self.token.as_deref().ok_or("the server needs a token")?;
                    // the reply is taken as it is, as the server ends the connection once it refuses the token
                    let receiver = connection.send(AUTH, &proof(token, nonce));
                    match receiver.and_then(|receiver| receiver.recv().ok()) {
                        Some((OK, _)) => (),
                        Some((ERROR, message)) => return Err(String::from_utf8_lossy(&message).into_owned()),
                        _ => return Err(connection.failure().unwrap_or_else(|| "the server refused the token".into())),
                    }
                }
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
        // a manifest which could not be asked for is asked for again, while one which was not found is not
        let found = if let Some(http) = &self.http {
            http.get(&format!("/mf/{}", crate::cache::hex(key))).ok()?
        } else {
            let connection = self.connection()?;
            let reply = connection.request(GET_MANIFEST, key);
            reply.as_ref()?;
            Self::get(&connection, reply)
        };
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
        if let Some(http) = &self.http {
            // each asked for at once, of which the first found in order is taken
            let found = in_parallel(keys, |key| http.get(&format!("/ac/{}", crate::cache::hex(key))).ok().flatten());
            return found.into_iter().enumerate().find_map(|(index, result)| Some((index, result?)));
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
        if let Some(http) = &self.http {
            let fetched = in_parallel(blobs, |blob| http.fetch_blob(local, blob).then_some(*blob));
            return fetched.into_iter().flatten().collect();
        }
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
        if let Some(http) = &self.http {
            return http.missing(blobs);
        }
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
        if let Some(http) = &self.http {
            return http.put(&cas_path(hash), crate::http::Body::Bytes(contents));
        }
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
        let backend = match &self.http {
            Some(http) => Backend::Http(Arc::clone(http)),
            None => match self.connection() {
                Some(connection) => Backend::Rig(connection),
                None => return false,
            },
        };
        let Ok(mut pusher) = self.pusher.lock() else {
            return false;
        };
        let (sender, _) = pusher.get_or_insert_with(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<Push>();
            let pushing = std::thread::spawn(move || {
                for push in receiver {
                    match &backend {
                        Backend::Rig(connection) => push_result(connection, &push),
                        Backend::Http(http) => http.push_result(&push),
                    }
                }
            });
            (sender, pushing)
        });
        let push = Push { local, first: *first, second: *second, discovery: discovery.clone() };
        sender.send(push).is_ok()
    }
    /// Returns the number of actions the server runs at once, as a worker, connecting to it to find out, or `0` if it
    /// is not one, or has failed.
    #[must_use]
    pub fn jobs(&self) -> usize {
        if self.http.is_some() {
            return 0;
        }
        self.connection().map_or(0, |connection| connection.jobs)
    }
    /// Returns the number of actions the server runs at once, as a worker, if it has been connected to, without waiting
    /// for a connection which is being made, or `0`.
    #[must_use]
    pub fn connected_jobs(&self) -> usize {
        match self.state.try_lock().as_deref() {
            Ok(State::Connected(connection)) if !connection.has_failed() => connection.jobs,
            _ => 0,
        }
    }
    /// Returns what the server, as a worker, has: its host, tags and toolchains.
    ///
    /// # Errors
    ///
    /// If it is not a worker, or cannot be reached.
    pub fn capabilities(&self) -> Result<crate::worker::Capabilities, String> {
        let failed = || self.error().unwrap_or_else(|| "the worker cannot be reached".into());
        let connection = self.connection().ok_or_else(failed)?;
        if connection.jobs == 0 {
            return Err("the server is not a worker".into());
        }
        match connection.send(GET_CAPABILITIES, &[]).and_then(|receiver| receiver.recv().ok()) {
            Some((FOUND, payload)) => {
                crate::worker::Capabilities::decode(&payload).ok_or_else(|| "an invalid reply from the worker".into())
            },
            Some((ERROR, message)) => Err(String::from_utf8_lossy(&message).into_owned()),
            _ => Err(failed()),
        }
    }
    /// Whether the server, as a worker, may have a toolchain: whether it has not refused an action of it.
    #[must_use]
    pub fn may_have(&self, identity: &str) -> bool {
        self.refused.lock().is_ok_and(|refused| !refused.contains(identity))
    }
    /// Run an action on the server, as a worker, first sending the blobs of its inputs which it does not have, from
    /// their files, relative to the build's directory. The blobs of the outputs and what it printed are left on the
    /// worker, to be fetched.
    ///
    /// # Errors
    ///
    /// If the worker does not have the toolchain, which it is not asked again, or could not run the action, as when an
    /// input changed as it was sent, or the connection failed, after which the worker is not used again.
    pub fn execute(&self, dir: &std::path::Path, request: &Request) -> Result<Executed, Refused> {
        let failed = || Refused::Error(self.error().unwrap_or_else(|| "the worker cannot be reached".into()));
        let connection = self.connection().ok_or_else(failed)?;
        let missing = self.missing(&request.blobs()).ok_or_else(failed)?;
        let mut sent = Vec::new();
        for blob in &missing {
            let Some(input) = request.inputs.iter().find(|input| input.blob == *blob) else {
                continue;
            };
            let path = dir.join(&input.path);
            let (file, size) = std::fs::File::open(&path)
                .and_then(|file| Ok((file.metadata()?.len(), file)))
                .map(|(size, file)| (file, size))
                .map_err(|error| Refused::Error(format!("failed to read {path:?}: {error}")))?;
            sent.push(connection.send_parts(PUT_BLOB, blob, BufReader::new(file), size));
        }
        // a blob which is refused, as its file changed after it was hashed, fails only this action
        for receiver in sent {
            match receiver.and_then(|receiver| receiver.recv().ok()) {
                Some((OK, _)) => (),
                Some((ERROR, message)) => return Err(Refused::Error(String::from_utf8_lossy(&message).into_owned())),
                _ => return Err(failed()),
            }
        }
        let reply = connection.send(EXECUTE, &request.encode()).and_then(|receiver| receiver.recv().ok());
        match reply {
            Some((FOUND, payload)) => Executed::decode(&payload).ok_or_else(|| {
                connection.fail("an invalid reply from the worker".into());
                failed()
            }),
            Some((NOT_FOUND, _)) => {
                if let Ok(mut refused) = self.refused.lock() {
                    refused.insert(request.identity.clone());
                }
                Err(Refused::Toolchain)
            },
            Some((ERROR, message)) => Err(Refused::Error(String::from_utf8_lossy(&message).into_owned())),
            _ => Err(failed()),
        }
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

/// Returns the path of a blob over HTTP, as it is in a cache's directory: `/cas/<the first two hexadecimal digits of
/// its hash>/<the rest>`.
fn cas_path(hash: &Hash) -> String {
    let hex = crate::cache::hex(hash);
    format!("/cas/{}/{}", &hex[..2], &hex[2..])
}

/// What a remote cache is spoken to by, which the thread which pushes has.
enum Backend {
    Rig(Arc<Connection>),
    Http(Arc<Http>),
}

/// That a server over HTTP failed, which [`Http::fail`] has recorded why.
struct Failed;

/// The most requests over HTTP which are sent at once, each on a connection of its own.
const HTTP_AT_ONCE: usize = 8;

/// Returns what `f` returns for each of `items`, in their order, running it for several at once, each on a thread.
fn in_parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    let chunk = items.len().div_ceil(HTTP_AT_ONCE);
    std::thread::scope(|scope| {
        let f = &f;
        let threads: Vec<_> = items.chunks(chunk).map(|items| scope.spawn(move || items.iter().map(f).collect::<Vec<_>>())).collect();
        threads.into_iter().flat_map(|thread| thread.join().unwrap_or_default()).collect()
    })
}

impl Http {
    fn has_failed(&self) -> bool {
        self.failed.lock().map_or(true, |failed| failed.is_some())
    }
    fn fail(&self, why: String) {
        if let Ok(mut failed) = self.failed.lock() {
            failed.get_or_insert(why);
        }
    }
    /// Send a request, returning its status, if it was answered with one which is `2xx` or `404`, or `None` if the
    /// server failed, or failed now, when it is not used again.
    fn request(&self, method: &str, path: &str, body: crate::http::Body, sink: &mut dyn FnMut(&[u8])) -> Option<u16> {
        if self.has_failed() {
            return None;
        }
        let url = self.client.url();
        match self.client.request(method, path, body, sink) {
            Ok(status) if (200..300).contains(&status) || status == 404 => Some(status),
            Ok(status) => {
                self.fail(format!("{method} {}{path} was answered with {status}", url.path));
                None
            },
            Err(error) => {
                self.fail(format!("failed to {method} {}{path} at {}: {error}", url.path, url.authority));
                None
            },
        }
    }
    /// Returns a file, or `None` if the server does not have it, failing if the server did.
    fn get(&self, path: &str) -> Result<Option<Vec<u8>>, Failed> {
        let mut body = Vec::new();
        let status = self.request("GET", path, crate::http::Body::None, &mut |part| body.extend_from_slice(part));
        Ok((status.ok_or(Failed)? != 404).then_some(body))
    }
    fn put(&self, path: &str, body: crate::http::Body) -> bool {
        self.request("PUT", path, body, &mut |_| ()).is_some_and(|status| status != 404)
    }
    /// Fetch a blob into a cache, written to it in parts as they are read, returning whether it was added.
    fn fetch_blob(&self, local: &Cache, blob: &Hash) -> bool {
        let Ok(mut writer) = local.blob_writer() else {
            return false;
        };
        let path = cas_path(blob);
        let status = self.request("GET", &path, crate::http::Body::None, &mut |part| writer.write(part));
        status == Some(200) && writer.finish(blob)
    }
    /// Returns those of the blobs the server does not have, asking for each at once.
    fn missing(&self, blobs: &[Hash]) -> Option<Vec<Hash>> {
        let found = in_parallel(blobs, |blob| {
            let path = cas_path(blob);
            self.request("HEAD", &path, crate::http::Body::None, &mut |_| ()).map(|status| (*blob, status == 404))
        });
        let found: Option<Vec<_>> = found.into_iter().collect();
        Some(found?.into_iter().filter(|&(_, missing)| missing).map(|(blob, _)| blob).collect())
    }
    /// Push a result: the blobs the server does not have, at once, each sent from its file, then the result, then the
    /// manifest, merged with the server's, so that it never has a result without its blobs.
    fn push_result(&self, push: &Push) {
        let Some(result) = push.local.raw_result(&push.second) else {
            return;
        };
        let Some(missing) = crate::cache::result_blobs(&result).and_then(|blobs| self.missing(&blobs)) else {
            return;
        };
        let pushed = in_parallel(&missing, |blob| {
            let Ok((mut file, size)) = push.local.open_blob(blob) else {
                return false;
            };
            self.put(&cas_path(blob), crate::http::Body::File(&mut file, size))
        });
        if !pushed.into_iter().all(|pushed| pushed) {
            return;
        }
        if !self.put(&format!("/ac/{}", crate::cache::hex(&push.second)), crate::http::Body::Bytes(&result)) {
            return;
        }
        let path = format!("/mf/{}", crate::cache::hex(&push.first));
        let Ok(stored) = self.get(&path) else {
            return;
        };
        let manifest = crate::cache::merged_manifest(stored.as_deref(), push.discovery.clone());
        let _ = self.put(&path, crate::http::Body::Bytes(&manifest));
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

/// Returns the proof of a token, for a nonce: the hash of both.
fn proof(token: &[u8], nonce: &[u8]) -> Hash {
    crate::cache::Key::new("rig remote token").part(token).part(nonce).finish()
}

/// Returns a random nonce, from the system's source of random bytes.
fn nonce() -> std::io::Result<[u8; 32]> {
    let mut nonce = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    Ok(nonce)
}

/// Connect to a server listening at an address over TCP, trying each address it resolves to in turn, and giving up on
/// one after 10 seconds.
fn connect_tcp(address: &str) -> std::io::Result<std::net::TcpStream> {
    use std::net::ToSocketAddrs;
    let mut error = std::io::Error::new(std::io::ErrorKind::NotFound, "the address resolves to nothing");
    for resolved in address.to_socket_addrs()? {
        match std::net::TcpStream::connect_timeout(&resolved, std::time::Duration::from_secs(10)) {
            Ok(stream) => {
                // requests are small, and waited for, so they are not held back to be sent together
                stream.set_nodelay(true)?;
                return Ok(stream);
            },
            Err(failed) => error = failed,
        }
    }
    Err(error)
}

/// What a server serves: a cache, which takes pushes if `push`, to clients which prove they have the token, if there
/// is one, and runs actions as a worker, with the executor, if there is one.
#[derive(Clone, Copy)]
pub struct Server<'a> {
    pub cache: &'a Cache,
    pub push: bool,
    pub token: Option<&'a [u8]>,
    pub executor: Option<&'a crate::worker::Executor>,
}

/// Serve a cache to every client which connects to a listener over TCP, each on a thread of its own, as `jig serve
/// --listen` does, taking pushes if `push`, from clients which prove they have the token. `after` is run once each
/// client is done, as to collect the cache. It serves until the listener fails.
///
/// # Errors
///
/// If the listener cannot accept a connection.
pub fn listen(
    cache: &Cache,
    listener: &std::net::TcpListener,
    push: bool,
    token: &[u8],
    after: &(dyn Fn() + Sync),
) -> std::io::Result<()> {
    Server { cache, push, token: Some(token), executor: None }.listen(listener, after)
}

impl Server<'_> {
    /// Serve a client over streams until it ends them, answering requests on several threads, and running actions on
    /// as many as the executor runs at once.
    ///
    /// # Errors
    ///
    /// If the streams cannot be read or written, the client does not speak the protocol, or does not have the token.
    pub fn serve(&self, reader: impl Read, writer: impl Write + Send) -> std::io::Result<()> {
        serve_as(*self, reader, writer)
    }
    /// Serve every client which connects to a listener over TCP, each on a thread of its own, running `after` once each
    /// is done, until the listener fails.
    ///
    /// # Errors
    ///
    /// If the listener cannot accept a connection.
    pub fn listen(&self, listener: &std::net::TcpListener, after: &(dyn Fn() + Sync)) -> std::io::Result<()> {
        std::thread::scope(|scope| {
            loop {
                let (stream, _) = listener.accept()?;
                scope.spawn(move || {
                    let _ = stream.set_nodelay(true);
                    if let Ok(reader) = stream.try_clone() {
                        // a client which fails, or is refused, ends only its connection
                        let _ = self.serve(reader, stream);
                    }
                    after();
                });
            }
        })
    }
}

/// Serve a cache over streams, as `jig serve --stdio` does on its stdin and stdout, until the client ends the stream,
/// taking pushes if `push`, answering requests on several threads.
///
/// # Errors
///
/// If the streams cannot be read or written, or the client does not speak the protocol.
pub fn serve(cache: &Cache, reader: impl Read, writer: impl Write + Send, push: bool) -> std::io::Result<()> {
    serve_with_token(cache, reader, writer, push, None)
}

/// Serve a cache over streams, as [`serve`] does, to a client which proves it has the token, if there is one.
///
/// # Errors
///
/// If the streams cannot be read or written, the client does not speak the protocol, or does not have the token.
pub fn serve_with_token(
    cache: &Cache,
    reader: impl Read,
    writer: impl Write + Send,
    push: bool,
    token: Option<&[u8]>,
) -> std::io::Result<()> {
    Server { cache, push, token, executor: None }.serve(reader, writer)
}

fn serve_as(server: Server, reader: impl Read, writer: impl Write + Send) -> std::io::Result<()> {
    let Server { cache, push, token, executor } = server;
    // a worker takes the blobs of the inputs of its actions, which are checked against their hashes, though not
    // results, which only a cache which takes pushes does
    let blobs = push || executor.is_some();
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
    reply.extend_from_slice(&(executor.map_or(0, |executor| executor.jobs.max(1)) as u32).to_le_bytes());
    let nonce = token.map(|_| nonce()).transpose()?;
    match &nonce {
        Some(nonce) => {
            reply.push(1);
            reply.extend_from_slice(nonce);
        },
        None => reply.push(0),
    }
    write_frame(&mut writer, HELLO, id, &reply)?;
    if let (Some(token), Some(nonce)) = (token, nonce) {
        let proved = match read_frame(&mut reader) {
            Ok(Some((AUTH, id, proof_sent))) => Some((id, same(&proof_sent, &proof(token, &nonce)))),
            _ => None,
        };
        match proved {
            Some((id, true)) => write_frame(&mut writer, OK, id, &[])?,
            Some((id, false)) => {
                write_frame(&mut writer, ERROR, id, b"the token is not the server's")?;
                return Err(invalid("a client with another token"));
            },
            None => return Err(invalid("a client which did not prove its token")),
        }
    }
    let writer = Mutex::new(writer);
    let (requests, work) = std::sync::mpsc::channel::<Frame>();
    let work = Mutex::new(work);
    // actions run on threads of their own, so that they do not hold up the requests for blobs
    let (actions, running) = std::sync::mpsc::channel::<(u32, Vec<u8>)>();
    let running = Mutex::new(running);
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
                    if kind == GET_CAPABILITIES && let Some(executor) = executor {
                        reply(FOUND, id, &executor.capabilities().encode());
                        continue;
                    }
                    let (kind, payload) = answer(cache, push, blobs, kind, &payload);
                    reply(kind, id, &payload);
                }
            });
        }
        for _ in 0..executor.map_or(0, |executor| executor.jobs.max(1)) {
            scope.spawn(|| {
                while let Ok(Ok((id, payload))) = running.lock().map(|running| running.recv()) {
                    let Some(executor) = executor else {
                        break;
                    };
                    let (kind, payload) = match crate::worker::Request::decode(&payload) {
                        Some(request) => match executor.execute(cache, &request) {
                            Ok(executed) => (FOUND, executed.encode()),
                            Err(crate::worker::Refused::Toolchain) => (NOT_FOUND, Vec::new()),
                            Err(crate::worker::Refused::Error(message)) => (ERROR, message.into_bytes()),
                        },
                        None => (ERROR, b"an invalid action".to_vec()),
                    };
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
                    let upload = uploads.entry(id).or_insert_with(|| cache.blob_writer().ok().filter(|_| blobs));
                    if let Some(upload) = upload {
                        upload.write(&part);
                    }
                },
                Ok(Some((PUT_BLOB, id, payload))) if blobs => {
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
                Ok(Some((EXECUTE, id, payload))) if executor.is_some() => {
                    if actions.send((id, payload)).is_err() {
                        break Ok(());
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
        // the threads stop once there are no more requests, and the actions which run have finished
        drop(requests);
        drop(actions);
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

/// Whether two byte strings are the same, comparing every byte, so that how long it takes does not tell where they
/// differ.
fn same(lhs: &[u8], rhs: &[u8]) -> bool {
    lhs.len() == rhs.len() && lhs.iter().zip(rhs).fold(0, |differ, (lhs, rhs)| differ | (lhs ^ rhs)) == 0
}

/// Returns the reply to a request.
fn answer(cache: &Cache, push: bool, blobs: bool, kind: u8, payload: &[u8]) -> (u8, Vec<u8>) {
    let error = |message: &str| (ERROR, message.as_bytes().to_vec());
    let found = |found: Option<Vec<u8>>| found.map_or((NOT_FOUND, Vec::new()), |found| (FOUND, found));
    let mut p = Payload(payload);
    if (matches!(kind, PUT_RESULT | ADD_TO_MANIFEST) && !push) || (kind == PUT_BLOB && !blobs) {
        return error("the server does not take pushes");
    }
    if matches!(kind, EXECUTE | GET_CAPABILITIES) {
        return error("the server is not a worker");
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

/// Returns a worker which runs actions with an executor, with `cache` as its store, served over pipes.
#[cfg(test)]
pub(crate) fn served_worker(cache: Cache, executor: crate::worker::Executor) -> Remote {
    let (requests, client) = std::io::pipe().unwrap();
    let (server, replies) = std::io::pipe().unwrap();
    std::thread::spawn(move || {
        Server { cache: &cache, push: false, token: None, executor: Some(&executor) }.serve(requests, replies)
    });
    Remote::streams(server, client, false)
}

/// Returns a remote cache which `cache` is served as, over pipes, to a client which has the token, if there is one,
/// which proves itself with `client`, if it is given.
#[cfg(test)]
pub(crate) fn served_with_token(cache: Cache, token: Option<&'static [u8]>, client: Option<&[u8]>) -> Remote {
    let (requests, writer) = std::io::pipe().unwrap();
    let (reader, replies) = std::io::pipe().unwrap();
    std::thread::spawn(move || serve_with_token(&cache, requests, replies, false, token));
    let remote = Remote::streams(reader, writer, false);
    match client {
        Some(client) => remote.with_token(client.to_vec()),
        None => remote,
    }
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
                let reply = if kind == HELLO { hello_reply() } else { b"corrupted".to_vec() };
                let _ = write_frame(&mut replies, if kind == HELLO { HELLO } else { FOUND }, id, &reply);
            }
        });
        let client_cache = cache("corrupted").with_remote(Arc::new(Remote::streams(server, client, false)));
        let hash = crate::blake3::hash(b"contents");
        assert!(client_cache.read_blob(&hash).is_err());
        assert!(!client_cache.has_blob(&hash));
        // as is a result or manifest
        assert!(client_cache.result(&hash).is_none());
        assert_eq!(client_cache.manifest(&hash), Vec::<Discovery>::new());
        let _ = std::fs::remove_dir_all(client_cache.dir());
    }

    #[test]
    fn runs_actions_only_as_a_worker() {
        use crate::worker::{Capabilities, Executor, Input, Toolchain};
        let dir = std::env::temp_dir().join(format!("rig-remote-{}-worker", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/in"), "input\n").unwrap();
        let blob = crate::blake3::hash(b"input\n");
        let request = Request {
            identity: "toolchain".into(),
            install: String::new(),
            action: crate::Action::Shell("cp in out && echo copied".into()),
            env: Vec::new(),
            locating: vec!["PATH".into()],
            inputs: vec![Input { path: "in".into(), blob, executable: false }],
            outputs: vec!["out".into()],
        };
        // a cache which is not a worker runs nothing, and does not take blobs
        let plain = cache("not-a-worker");
        let _ = std::fs::remove_dir_all(plain.dir());
        let server = served(plain, false, false);
        assert_eq!(server.jobs(), 0);
        assert!(server.capabilities().is_err());
        assert!(matches!(server.execute(&dir.join("build"), &request), Err(Refused::Error(_))));
        // a worker is sent the input, runs the action, and keeps its output for the client to fetch
        let store = cache("worker-store");
        let toolchain = Toolchain { identity: "toolchain".into(), release: "1.97.0".into(), ..Toolchain::default() };
        let capabilities = Capabilities { host: "host".into(), tags: vec!["fast".into()], toolchains: vec![toolchain] };
        let reported = capabilities.clone();
        let executor = Executor::new(store.dir().join("work"), 3, move || reported.clone());
        let worker = served_worker(store.clone(), executor);
        assert_eq!(worker.jobs(), 3);
        assert_eq!(worker.capabilities(), Ok(capabilities));
        let executed = worker.execute(&dir.join("build"), &request).unwrap();
        assert!(executed.succeeded);
        assert!(store.has_blob(&blob));
        let local = cache("worker-client");
        let blobs: Vec<Hash> = crate::cache::entry_blobs(&executed.entry).copied().collect();
        assert_eq!(worker.fetch_blobs(&local, &blobs).len(), 2);
        assert_eq!(local.read_blob(&executed.entry.outputs[0].blob).unwrap(), b"input\n");
        assert_eq!(local.read_blob(&executed.entry.printed).unwrap(), b"copied\n");
        // another toolchain is refused, and not asked for again, while the worker goes on
        let other = Request { identity: "other".into(), ..request.clone() };
        assert_eq!(worker.execute(&dir.join("build"), &other), Err(Refused::Toolchain));
        assert!(!worker.may_have("other") && worker.may_have("toolchain"));
        assert!(worker.execute(&dir.join("build"), &request).is_ok());
        assert!(worker.error().is_none());
        for cache in [store, local] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reply to `HELLO` of a server which does not take pushes, is not a worker, and needs no token.
    fn hello_reply() -> Vec<u8> {
        [VERSION.to_le_bytes().as_slice(), &[0], &0u32.to_le_bytes(), &[0]].concat()
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
            let _ = write_frame(&mut replies, HELLO, id, &hello_reply());
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
        assert_eq!(remote.fetch_blobs(&local, &[hash]), Vec::<Hash>::new());
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
    fn serves_clients_with_the_token() {
        let server = cache("token");
        let blob = server.put_bytes(b"contents").unwrap();
        let local = cache("token-local");
        let fetch = |token, client| served_with_token(server.clone(), token, client).fetch_blobs(&local, &[blob]);
        assert_eq!(fetch(Some(b"secret"), Some(b"secret")), [blob]);
        // a server without one takes a client with one
        assert_eq!(fetch(None, Some(b"secret")), [blob]);
        // while a client with another, or none, is refused
        let remote = served_with_token(server.clone(), Some(b"secret"), Some(b"other"));
        assert_eq!(remote.fetch_blobs(&local, &[blob]), Vec::<Hash>::new());
        assert!(remote.error().unwrap().contains("the token is not the server's"), "{:?}", remote.error());
        let remote = served_with_token(server.clone(), Some(b"secret"), None);
        assert!(remote.manifest(&blob).is_none());
        assert_eq!(remote.error().as_deref(), Some("the server needs a token"));
        // and the proof is of the nonce, so one for another nonce is not of use
        assert_ne!(proof(b"secret", &[1; 32]), proof(b"secret", &[2; 32]));
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
        for cache in [server, local] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn serves_over_tcp() {
        let server = cache("tcp");
        let blob = server.put_bytes(b"over tcp").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let listening = server.clone();
        let collected = Arc::new(AtomicU32::new(0));
        let after = Arc::clone(&collected);
        std::thread::spawn(move || {
            let after = move || {
                after.fetch_add(1, Ordering::SeqCst);
            };
            listen(&listening, &listener, true, b"secret", &after)
        });
        // two clients at once, one of which pushes
        let pusher = cache("tcp-pusher").with_remote(Arc::new(Remote::tcp(address.clone(), true).with_token(b"secret".to_vec())));
        let local = cache("tcp-local");
        let fetcher = Remote::tcp(address.clone(), false).with_token(b"secret".to_vec());
        assert_eq!(fetcher.fetch_blobs(&local, &[blob]), [blob]);
        assert_eq!(local.read_blob(&blob).unwrap(), b"over tcp");
        let blob_pushed = crate::blake3::hash(b"pushed");
        assert!(pusher.remote().unwrap().put_blob(&blob_pushed, b"pushed"));
        assert!(server.has_blob(&blob_pushed));
        // a client without the token is refused, and the server goes on
        let refused = Remote::tcp(address.clone(), false);
        assert!(refused.manifest(&blob).is_none());
        assert_eq!(fetcher.fetch_blobs(&local, &[blob_pushed]), [blob_pushed]);
        // each client which is done is collected after
        drop((fetcher, refused, pusher));
        let waited = std::time::Instant::now();
        while collected.load(Ordering::SeqCst) < 3 && waited.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::yield_now();
        }
        assert_eq!(collected.load(Ordering::SeqCst), 3);
        // and an address which nothing listens at fails
        let closed = Remote::tcp("127.0.0.1:1".into(), false);
        assert!(closed.manifest(&blob).is_none());
        assert!(closed.error().unwrap().contains("failed to connect to 127.0.0.1:1"));
        for cache in [server, local] {
            let _ = std::fs::remove_dir_all(cache.dir());
        }
    }

    #[test]
    fn pushes_to_and_fetches_from_a_server_of_files_over_http() {
        for chunked in [false, true] {
            let files = std::env::temp_dir().join(format!("rig-remote-{}-http-{chunked}", std::process::id()));
            let _ = std::fs::remove_dir_all(&files);
            let address = crate::http::serve_directory(files.clone(), chunked, None);
            let url = || crate::http::Url::parse(&format!("http://{address}/cache")).unwrap();
            let (first, second) = (crate::blake3::hash(b"first"), crate::blake3::hash(b"second"));
            let discovery = Discovery { inputs: vec!["a.h".into()], env: Vec::new() };
            // a result with a large blob, which is sent from its file, and one which is small
            let contents = large(3 * PART + 7, 5);
            let pusher = cache("http-pusher").with_remote(Arc::new(Remote::http(url(), None, true)));
            let blob = pusher.put_bytes(&contents).unwrap();
            let printed = pusher.put_bytes(b"printed").unwrap();
            let entry = Entry { outputs: vec![Output { path: "out".into(), blob, executable: false }], printed };
            pusher.put_result(&second, &entry).unwrap();
            pusher.add_to_manifest(&first, discovery.clone()).unwrap();
            assert!(pusher.push(&first, &second, &discovery));
            pusher.wait_for_pushes();
            assert!(pusher.remote().unwrap().error().is_none(), "{:?}", pusher.remote().unwrap().error());
            // which the server keeps as a cache's directory has them
            let server = Cache::open(&files.join("cache")).unwrap();
            assert_eq!(server.read_blob(&blob).unwrap(), contents);
            assert!(server.result(&second).is_some());
            assert_eq!(server.manifest(&first), std::slice::from_ref(&discovery));
            assert_eq!(pusher.remote().unwrap().missing(&[blob, crate::blake3::hash(b"other")]).unwrap().len(), 1);
            // another set of inputs is merged into the server's manifest
            let other = Discovery { inputs: vec!["b.h".into()], env: Vec::new() };
            let third = crate::blake3::hash(b"third");
            pusher.put_result(&third, &entry).unwrap();
            assert!(pusher.push(&first, &third, &other));
            pusher.wait_for_pushes();
            assert_eq!(server.manifest(&first), [other.clone(), discovery.clone()]);
            // and another cache finds the result there, and fetches its blobs
            let puller = cache("http-puller").with_remote(Arc::new(Remote::http(url(), None, false)));
            let second_of = |found: &Discovery| if *found == discovery { second } else { crate::blake3::hash(b"unknown") };
            let (key, found) = puller.find(&first, second_of).unwrap();
            assert_eq!((key, found.outputs.len()), (second, 1));
            puller.fetch_blobs(&[blob, printed]);
            assert_eq!(puller.read_blob(&blob).unwrap(), contents);
            assert!(puller.without_remote().find(&first, second_of).is_some());
            assert!(puller.remote().unwrap().error().is_none());
            for cache in [pusher, puller] {
                let _ = std::fs::remove_dir_all(cache.dir());
            }
            let _ = std::fs::remove_dir_all(&files);
        }
    }

    #[test]
    fn stops_using_a_server_over_http_which_refuses() {
        let files = std::env::temp_dir().join(format!("rig-remote-{}-http-refuses", std::process::id()));
        let address = crate::http::serve_directory(files.clone(), false, Some("Bearer token".into()));
        let url = crate::http::Url::parse(&format!("http://{address}")).unwrap();
        let remote = Remote::http(url.clone(), Some(b"wrong"), true);
        assert!(remote.manifest(&[0; 32]).is_none());
        assert!(remote.error().unwrap().contains("GET /mf/"), "{:?}", remote.error());
        assert!(remote.error().unwrap().contains("401"));
        assert!(!remote.pushes());
        let remote = Remote::http(url, Some(b"token"), true);
        assert!(remote.manifest(&[0; 32]).is_none());
        assert!(remote.error().is_none() && remote.pushes());
        // and one which nothing listens at
        let closed = Remote::http(crate::http::Url::parse("http://127.0.0.1:1").unwrap(), None, false);
        assert!(closed.manifest(&[0; 32]).is_none());
        assert!(closed.error().unwrap().contains("at 127.0.0.1:1"));
        let _ = std::fs::remove_dir_all(&files);
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
