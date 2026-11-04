//! An HTTP/1.1 client, for a remote cache which is a store of files served over HTTP, such as a directory served by
//! nginx with `WebDAV`'s `PUT`, laid out as a cache's directory is, as [`crate::remote`] describes.
//!
//! Only `http://` is spoken, as `std` has no TLS, so a cache which is not on the machine or a trusted network is put
//! behind a proxy which speaks TLS to it, such as stunnel. Connections are kept alive, and are pooled, so that requests
//! from several threads go at once on connections of their own. A body is sent with its length, and read by its
//! `Content-Length`, in chunks with `Transfer-Encoding: chunked`, or until the connection is closed. A request on a
//! connection which was pooled, which the server may have closed meanwhile, is sent again once on a new one if it
//! fails before its response starts.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::net::TcpStream;
use std::sync::Mutex;

/// How much of a body is read or written at once.
const PART: usize = 64 << 10;

/// The longest status line or header which is read.
const MAX_LINE: usize = 16 << 10;

/// The URL of a server, as `http://[<user>:<password>@]<host>[:<port>][/<path>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    /// The host and port, as the `Host` header and the address to connect to have them.
    pub authority: String,
    /// The path the paths of requests are under, without a `/` at its end, or empty.
    pub path: String,
    /// The user and password, as `<user>:<password>`, if there are any.
    pub user: Option<String>,
}
impl Url {
    /// Parse a URL, `http://[<user>:<password>@]<host>[:<port>][/<path>]`, whose port is 80 without one.
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("http://")?;
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(authority, path)| (authority, path));
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_string()), host),
            None => (None, authority),
        };
        if host.is_empty() || host.starts_with(':') {
            return None;
        }
        // a port, after the host or after the brackets of an IPv6 address
        let has_port = host.rsplit_once(':').is_some_and(|(before, _)| !before.ends_with(':') && !host.ends_with(']'));
        let authority = if has_port { host.to_string() } else { format!("{host}:80") };
        if let Some((_, port)) = authority.rsplit_once(':')
            && port.parse::<u16>().is_err()
        {
            return None;
        }
        let path = path.trim_end_matches('/');
        let path = if path.is_empty() { String::new() } else { format!("/{path}") };
        Some(Self { authority, path, user })
    }
}

/// The body of a request.
pub enum Body<'a> {
    None,
    Bytes(&'a [u8]),
    /// A file, of a size, which is read as it is sent.
    File(&'a mut std::fs::File, u64),
}

/// A connection which is kept alive, with what reads its responses.
struct Stream {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

/// A client of a server.
pub struct Client {
    url: Url,
    /// The value of the `Authorization` header which every request has, if there is one.
    authorization: Option<String>,
    /// The connections which are kept alive, and not in use.
    idle: Mutex<Vec<Stream>>,
}
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Client").field("url", &self.url).finish_non_exhaustive()
    }
}

impl Client {
    /// A client of the server at a URL, which authenticates with its user and password, if it has them, by HTTP's
    /// Basic scheme, or with a bearer token, if one is given.
    #[must_use]
    pub fn new(url: Url, token: Option<&[u8]>) -> Self {
        let authorization = match (&url.user, token) {
            (_, Some(token)) => Some(format!("Bearer {}", String::from_utf8_lossy(token))),
            (Some(user), None) => Some(format!("Basic {}", base64(user.as_bytes()))),
            (None, None) => None,
        };
        Self { url, authorization, idle: Mutex::default() }
    }
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }
    /// Send a request for a path under the URL's, returning its status once its body is given to `sink`, as it is
    /// read, in parts.
    ///
    /// # Errors
    ///
    /// If the server cannot be connected to, or the request sent, or the response is not HTTP.
    pub fn request(&self, method: &str, path: &str, mut body: Body, sink: &mut dyn FnMut(&[u8])) -> std::io::Result<u16> {
        let pooled = self.idle.lock().ok().and_then(|mut idle| idle.pop());
        let reused = pooled.is_some();
        let stream = match pooled {
            Some(stream) => stream,
            None => self.connect()?,
        };
        match self.exchange(stream, method, path, &mut body, sink) {
            Ok(status) => Ok(status),
            // a connection which was kept alive may have been closed by the server, so the request is sent again, once,
            // on a new one, if nothing of its response was read
            Err((_, false)) if reused => {
                if let Body::File(file, _) = &mut body {
                    file.rewind()?;
                }
                let stream = self.connect()?;
                self.exchange(stream, method, path, &mut body, sink).map_err(|(error, _)| error)
            },
            Err((error, _)) => Err(error),
        }
    }
    fn connect(&self) -> std::io::Result<Stream> {
        use std::net::ToSocketAddrs;
        let mut error = std::io::Error::new(std::io::ErrorKind::NotFound, "the address resolves to nothing");
        for address in self.url.authority.to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, std::time::Duration::from_secs(10)) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    return Ok(Stream { reader: BufReader::new(stream.try_clone()?), writer: stream });
                },
                Err(failed) => error = failed,
            }
        }
        Err(error)
    }
    /// Send a request and read its response, returning the connection to the pool if it is kept alive, or the error,
    /// with whether any of the response was read.
    fn exchange(
        &self,
        mut stream: Stream,
        method: &str,
        path: &str,
        body: &mut Body,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<u16, (std::io::Error, bool)> {
        let started = |error| (error, false);
        let length = match body {
            Body::None => 0,
            Body::Bytes(bytes) => bytes.len() as u64,
            Body::File(_, size) => *size,
        };
        let mut head = format!("{method} {}{path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rig\r\n", self.url.path, self.url.authority);
        if method != "GET" && method != "HEAD" {
            let _ = write!(head, "Content-Length: {length}\r\n");
        }
        if let Some(authorization) = &self.authorization {
            let _ = write!(head, "Authorization: {authorization}\r\n");
        }
        head.push_str("\r\n");
        let mut writer = std::io::BufWriter::with_capacity(PART, &stream.writer);
        writer.write_all(head.as_bytes()).map_err(started)?;
        match body {
            Body::None => (),
            Body::Bytes(bytes) => writer.write_all(bytes).map_err(started)?,
            Body::File(file, size) => {
                let copied = std::io::copy(&mut file.take(*size), &mut writer).map_err(started)?;
                if copied != *size {
                    return Err(started(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the file is shorter")));
                }
            },
        }
        writer.flush().map_err(started)?;
        drop(writer);
        // the status line, which is the first of the response
        let line = read_line(&mut stream.reader).map_err(started)?;
        let read = |error| (error, true);
        let mut words = line.split(' ');
        let (Some(version), Some(status)) = (words.next(), words.next()) else {
            return Err(read(invalid("a response which is not HTTP")));
        };
        let status: u16 = status.parse().map_err(|_| read(invalid("a response without a status")))?;
        let mut length = None;
        let mut chunked = false;
        let mut close = version == "HTTP/1.0";
        loop {
            let line = read_line(&mut stream.reader).map_err(read)?;
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = Some(value.parse::<u64>().map_err(|_| read(invalid("an invalid length")))?),
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                "connection" => close = value.eq_ignore_ascii_case("close"),
                _ => (),
            }
        }
        // a response to `HEAD`, and one without content, has no body whatever its headers say
        let empty = method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status);
        if empty {
        } else if chunked {
            loop {
                let line = read_line(&mut stream.reader).map_err(read)?;
                let size = line.split(';').next().unwrap_or_default().trim();
                let size = u64::from_str_radix(size, 16).map_err(|_| read(invalid("an invalid chunk")))?;
                if size == 0 {
                    // the trailers, which end with an empty line
                    while !read_line(&mut stream.reader).map_err(read)?.is_empty() {}
                    break;
                }
                copy_to(&mut stream.reader, size, sink).map_err(read)?;
                read_line(&mut stream.reader).map_err(read)?;
            }
        } else if let Some(length) = length {
            copy_to(&mut stream.reader, length, sink).map_err(read)?;
        } else {
            // the body ends with the connection
            close = true;
            copy_to(&mut stream.reader, u64::MAX, sink).map_err(read)?;
        }
        if !close && let Ok(mut idle) = self.idle.lock() {
            idle.push(stream);
        }
        Ok(status)
    }
}

/// Read a line, ending with `\r\n`, without it.
fn read_line(reader: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    let mut line = Vec::new();
    let read = reader.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the server closed the connection"));
    }
    if !line.ends_with(b"\n") {
        return Err(invalid("a line which is too long"));
    }
    while line.last().is_some_and(|byte| matches!(byte, b'\r' | b'\n')) {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| invalid("a line which is not UTF-8"))
}

/// Give up to `length` bytes of a reader to a sink, in parts, all of them unless `length` is `u64::MAX`, which is
/// until the end.
fn copy_to(reader: &mut impl Read, length: u64, sink: &mut dyn FnMut(&[u8])) -> std::io::Result<()> {
    let mut remaining = length;
    let mut part = vec![0; PART];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(PART as u64)).unwrap_or(PART);
        let read = reader.read(&mut part[..want])?;
        if read == 0 {
            if length == u64::MAX {
                return Ok(());
            }
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the body is shorter than its length"));
        }
        sink(&part[..read]);
        if length != u64::MAX {
            remaining -= read as u64;
        }
    }
    Ok(())
}

fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Encode bytes in base64, with padding, as HTTP's Basic scheme has the user and password.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0u32, |triple, (i, &byte)| triple | u32::from(byte) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                encoded.push(char::from(ALPHABET[(triple >> (18 - 6 * i)) as usize & 63]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

/// A server of the files in a directory, for tests: `GET`, `HEAD` and `PUT`, which keeps connections alive, sends
/// bodies in chunks if `chunked`, and refuses a request without the `Authorization` header `authorization`, if one is
/// given. It returns the address it listens at, and serves until the process ends.
#[cfg(test)]
pub(crate) fn serve_directory(dir: std::path::PathBuf, chunked: bool, authorization: Option<String>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (dir, authorization) = (dir.clone(), authorization.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                while let Ok(line) = read_line(&mut reader) {
                    let mut words = line.split(' ');
                    let (method, path) = (words.next().unwrap_or_default().to_string(), words.next().unwrap_or_default());
                    let path = dir.join(path.trim_start_matches('/'));
                    let (mut length, mut authorized) = (0, authorization.is_none());
                    while let Ok(header) = read_line(&mut reader) {
                        if header.is_empty() {
                            break;
                        }
                        let (name, value) = header.split_once(':').unwrap_or_default();
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                        if name.eq_ignore_ascii_case("authorization") {
                            authorized = authorization.as_deref() == Some(value.trim());
                        }
                    }
                    let mut body = vec![0; length];
                    if reader.read_exact(&mut body).is_err() {
                        return;
                    }
                    let reply = |status: &str, body: &[u8], head: bool| -> Vec<u8> {
                        let mut reply = format!("HTTP/1.1 {status}\r\n").into_bytes();
                        if chunked && !head {
                            reply.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
                            for chunk in body.chunks(1000) {
                                reply.extend_from_slice(format!("{:x};x=y\r\n", chunk.len()).as_bytes());
                                reply.extend_from_slice(chunk);
                                reply.extend_from_slice(b"\r\n");
                            }
                            reply.extend_from_slice(b"0\r\nTrailer: x\r\n\r\n");
                        } else {
                            reply.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
                            if !head {
                                reply.extend_from_slice(body);
                            }
                        }
                        reply
                    };
                    let response = match method.as_str() {
                        _ if !authorized => reply("401 Unauthorized", b"", false),
                        "GET" | "HEAD" => match std::fs::read(&path) {
                            Ok(contents) => reply("200 OK", &contents, method == "HEAD"),
                            Err(_) => reply("404 Not Found", b"", method == "HEAD"),
                        },
                        "PUT" => {
                            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                            std::fs::write(&path, &body).unwrap();
                            reply("201 Created", b"", false)
                        },
                        _ => reply("405 Method Not Allowed", b"", false),
                    };
                    if writer.write_all(&response).is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let url = |url| Url::parse(url);
        assert_eq!(url("http://cache:8080/rig/"), Some(Url { authority: "cache:8080".into(), path: "/rig".into(), user: None }));
        assert_eq!(url("http://cache"), Some(Url { authority: "cache:80".into(), path: String::new(), user: None }));
        let with_user = url("http://ci:secret@cache:8080").unwrap();
        assert_eq!((with_user.authority.as_str(), with_user.user.as_deref()), ("cache:8080", Some("ci:secret")));
        assert_eq!(url("http://[::1]:8080/x").unwrap().authority, "[::1]:8080");
        assert_eq!(url("http://[::1]/x").unwrap().authority, "[::1]:80");
        for invalid in ["https://cache", "http://", "http://:80", "http://cache:port", "cache:80"] {
            assert_eq!(url(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"Aladdin:open sesame"), "QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn sends_and_reads_bodies() {
        for chunked in [false, true] {
            let dir = std::env::temp_dir().join(format!("rig-http-{}-{chunked}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let address = serve_directory(dir.clone(), chunked, None);
            let client = Client::new(Url::parse(&format!("http://{address}/base")).unwrap(), None);
            let get = |path: &str| {
                let mut body = Vec::new();
                let status = client.request("GET", path, Body::None, &mut |part| body.extend_from_slice(part)).unwrap();
                (status, body)
            };
            assert_eq!(get("/missing").0, 404);
            let large: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
            assert_eq!(client.request("PUT", "/a/b", Body::Bytes(&large), &mut |_| ()).unwrap(), 201);
            assert_eq!(get("/a/b"), (200, large.clone()));
            assert!(dir.join("base/a/b").is_file());
            // a file, read as it is sent
            let path = dir.join("file");
            std::fs::write(&path, b"from a file").unwrap();
            let mut file = std::fs::File::open(&path).unwrap();
            assert_eq!(client.request("PUT", "/f", Body::File(&mut file, 11), &mut |_| ()).unwrap(), 201);
            assert_eq!(get("/f"), (200, b"from a file".to_vec()));
            assert_eq!(client.request("HEAD", "/f", Body::None, &mut |_| panic!("a body")).unwrap(), 200);
            // on connections which are kept alive, at most as many as were used at once
            assert_eq!(client.idle.lock().unwrap().len(), 1);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn authenticates() {
        let dir = std::env::temp_dir().join(format!("rig-http-{}-auth", std::process::id()));
        let address = serve_directory(dir.clone(), false, Some("Basic Y2k6c2VjcmV0".into()));
        let status = |url: &str, token: Option<&[u8]>| {
            Client::new(Url::parse(url).unwrap(), token).request("GET", "/x", Body::None, &mut |_| ()).unwrap()
        };
        assert_eq!(status(&format!("http://ci:secret@{address}"), None), 404);
        assert_eq!(status(&format!("http://ci:wrong@{address}"), None), 401);
        assert_eq!(status(&format!("http://{address}"), Some(b"token")), 401);
        let address = serve_directory(dir.clone(), false, Some("Bearer token".into()));
        assert_eq!(status(&format!("http://{address}"), Some(b"token")), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
