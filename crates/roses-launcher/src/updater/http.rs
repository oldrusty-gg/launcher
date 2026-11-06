//! A small blocking HTTP/1.1 GET for the updater: the two kinds of request it
//! needs (a JSON answer and a file) over the TLS the platform already has
//! (rustls with Mozilla's roots, `platform_net::tls`). Every address, redirect
//! targets included, passes [`Policy`].

use super::Policy;
use platform_net::tls::{client_config, Trust};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A download silent for this long has stopped.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const MAX_LINE: u64 = 16 * 1024;
const MAX_HEADERS: usize = 200;

/// A parsed address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path and query.
    pub path: String,
}

impl Target {
    /// `http(s)://host[:port][/path][?query]`; no user name, no IPv6 literal.
    pub fn parse(url: &str) -> Option<Self> {
        if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return None;
        }
        let (tls, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
            (Some(rest), _) => (true, rest),
            (_, Some(rest)) => (false, rest),
            _ => return None,
        };
        let rest = rest.split('#').next()?;
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(split);
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
            return None;
        }
        let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
        Some(Self { tls, host: host.to_ascii_lowercase(), port, path })
    }
}

trait Connection: Read + Write + Send {}
impl<T: Read + Write + Send> Connection for T {}

enum Framing {
    Length(u64),
    Chunked { left: u64, finished: bool },
    /// Until the server closes (a TLS close without notice is an error).
    Close,
}

/// The status and the body as a reader. A connection that ends early is a
/// read error, never a short body.
pub struct Response {
    pub status: u16,
    /// The size the server promises, when it says.
    pub length: Option<u64>,
    reader: BufReader<Box<dyn Connection>>,
    framing: Framing,
}

fn early_end() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the connection closed early")
}

fn read_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = Vec::new();
    reader.take(MAX_LINE).read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(early_end());
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_owned())
}

impl Read for Response {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.framing {
            Framing::Close => self.reader.read(buffer),
            Framing::Length(left) => {
                if *left == 0 || buffer.is_empty() {
                    return Ok(0);
                }
                let room = buffer.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                let read = self.reader.read(&mut buffer[..room])?;
                if read == 0 {
                    return Err(early_end());
                }
                *left -= read as u64;
                Ok(read)
            }
            Framing::Chunked { left, finished } => {
                if *finished || buffer.is_empty() {
                    return Ok(0);
                }
                if *left == 0 {
                    let line = read_line(&mut self.reader)?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    *left = u64::from_str_radix(size, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
                    if *left == 0 {
                        // Trailers, then the end.
                        while !read_line(&mut self.reader)?.is_empty() {}
                        *finished = true;
                        return Ok(0);
                    }
                }
                let room = buffer.len().min(usize::try_from(*left).unwrap_or(usize::MAX));
                let read = self.reader.read(&mut buffer[..room])?;
                if read == 0 {
                    return Err(early_end());
                }
                *left -= read as u64;
                if *left == 0 {
                    read_line(&mut self.reader)?;
                }
                Ok(read)
            }
        }
    }
}

fn open(target: &Target) -> Result<Box<dyn Connection>, String> {
    let unreachable = |e: &dyn std::fmt::Display| format!("Could not reach {} ({e}).", target.host);
    let addresses = (target.host.as_str(), target.port).to_socket_addrs().map_err(|e| unreachable(&e))?;
    let mut last = None;
    let mut socket = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(connected) => {
                socket = Some(connected);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let Some(socket) = socket else {
        return Err(unreachable(&last.map_or("no address".to_owned(), |e| e.to_string())));
    };
    socket.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| unreachable(&e))?;
    socket.set_write_timeout(Some(READ_TIMEOUT)).map_err(|e| unreachable(&e))?;
    if !target.tls {
        return Ok(Box::new(socket));
    }
    let config = client_config(&Trust::WebPki).map_err(|e| unreachable(&e))?;
    let name = ServerName::try_from(target.host.clone()).map_err(|e| unreachable(&e))?;
    let connection = ClientConnection::new(config, name).map_err(|e| unreachable(&e))?;
    Ok(Box::new(StreamOwned::new(connection, socket)))
}

fn request(target: &Target, accept: &str) -> Result<(Response, Option<String>), String> {
    let failed = |e: &dyn std::fmt::Display| format!("Could not reach {} ({e}).", target.host);
    let mut connection = open(target)?;
    let release = crate::RELEASE.unwrap_or("development");
    // GitHub refuses a request without a User-Agent. HTTP/1.1 with
    // `Connection: close` keeps one request per connection.
    let head = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: oldrusty-launcher/{release}\r\nAccept: {accept}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        target.path, target.host
    );
    connection.write_all(head.as_bytes()).and_then(|()| connection.flush()).map_err(|e| failed(&e))?;
    let mut reader = BufReader::with_capacity(64 * 1024, connection);
    let status_line = read_line(&mut reader).map_err(|e| failed(&e))?;
    let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).filter(|_| status_line.starts_with("HTTP/1."));
    let Some(status) = status else {
        return Err(failed(&"its answer is not HTTP"));
    };
    let mut length = None;
    let mut chunked = false;
    let mut location = None;
    for _ in 0..MAX_HEADERS {
        let line = read_line(&mut reader).map_err(|e| failed(&e))?;
        if line.is_empty() {
            let framing = match (chunked, length) {
                (true, _) => Framing::Chunked { left: 0, finished: false },
                (false, Some(length)) => Framing::Length(length),
                (false, None) => Framing::Close,
            };
            let length = if chunked { None } else { length };
            return Ok((Response { status, length, reader, framing }, location));
        }
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            "location" => location = Some(value.to_owned()),
            _ => {}
        }
    }
    Err(failed(&"its answer has too many headers"))
}

/// GET `url`, following redirects, each checked against `policy`.
pub fn get(url: &str, accept: &str, policy: &Policy) -> Result<Response, String> {
    let mut next = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let target = Target::parse(&next).filter(|t| policy.allows(t)).ok_or_else(|| {
            let host = Target::parse(&next).map_or("an unreadable address".to_owned(), |t| t.host);
            format!("Refused to fetch from {host}: updates only come from GitHub over https.")
        })?;
        let (response, location) = request(&target, accept)?;
        if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        next = location.ok_or("GitHub redirected to nowhere.")?;
    }
    Err("GitHub redirected too many times.".into())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    fn body(url: &str, policy: &Policy) -> Result<Vec<u8>, String> {
        let mut response = get(url, "*/*", policy)?;
        let mut out = Vec::new();
        response.read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }

    #[test]
    fn addresses_parse() {
        let t = Target::parse("https://API.github.com/a/b?c=1#frag").unwrap();
        assert_eq!((t.tls, t.host.as_str(), t.port, t.path.as_str()), (true, "api.github.com", 443, "/a/b?c=1"));
        assert_eq!(Target::parse("http://127.0.0.1:8080").unwrap().path, "/");
        for bad in ["ftp://x/", "https://", "https://a@b/", "https://a b/", "https://a/\r\nX: y", "https://a:port/", "file:///c:/x"] {
            assert_eq!(Target::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_shipped_policy_allows_github_only() {
        let shipped = Policy { loopback_http: false };
        for good in [
            "https://api.github.com/x",
            "https://release-assets.githubusercontent.com/x",
            "https://objects.githubusercontent.com/x",
        ] {
            assert!(shipped.allows(&Target::parse(good).unwrap()), "{good}");
        }
        for bad in [
            "http://api.github.com/x",
            "https://github.com.evil.org/x",
            "https://evil.githubusercontent.com/x",
            "https://api.github.com:8443/x",
            "http://127.0.0.1/x",
            "https://127.0.0.1/x",
        ] {
            assert!(!shipped.allows(&Target::parse(bad).unwrap()), "{bad}");
        }
    }

    #[test]
    fn redirects_are_followed_and_checked() {
        let target = serve(|_| ok(b"file"));
        let address = target.clone();
        let url = serve(move |path| match path {
            "/good" => format!("HTTP/1.1 302 Found\r\nLocation: {address}/file\r\nContent-Length: 0\r\n\r\n").into_bytes(),
            "/away" => b"HTTP/1.1 302 Found\r\nLocation: http://example.org/x\r\nContent-Length: 0\r\n\r\n".to_vec(),
            "/plain" => b"HTTP/1.1 302 Found\r\nLocation: http://api.github.com/x\r\nContent-Length: 0\r\n\r\n".to_vec(),
            _ => b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        });
        assert_eq!(body(&format!("{url}/good"), &seam()).unwrap(), b"file");
        assert!(body(&format!("{url}/away"), &seam()).unwrap_err().contains("Refused to fetch from example.org"));
        assert!(body(&format!("{url}/plain"), &seam()).unwrap_err().contains("Refused"));
        assert!(body(&format!("{url}/none"), &seam()).unwrap_err().contains("nowhere"));
    }

    #[test]
    fn a_redirect_loop_ends() {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let inside = shared.clone();
        let url = serve(move |_| format!("HTTP/1.1 302 Found\r\nLocation: {}/again\r\nContent-Length: 0\r\n\r\n", inside.lock().unwrap()).into_bytes());
        *shared.lock().unwrap() = url.clone();
        assert!(body(&format!("{url}/a"), &seam()).unwrap_err().contains("too many"));
    }

    #[test]
    fn chunked_bodies_are_decoded_and_a_cut_is_an_error() {
        let url = serve(|path| match path {
            "/whole" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n".to_vec(),
            _ => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWi".to_vec(),
        });
        assert_eq!(body(&format!("{url}/whole"), &seam()).unwrap(), b"Wikipedia");
        assert!(body(&format!("{url}/cut"), &seam()).is_err());
    }

    #[test]
    fn a_cut_header_is_an_error() {
        let url = serve(|_| b"HTTP/1.1 200 OK\r\nContent-Le".to_vec());
        assert!(body(&format!("{url}/x"), &seam()).unwrap_err().contains("Could not reach"));
    }
}
