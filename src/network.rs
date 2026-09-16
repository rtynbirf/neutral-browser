//! Network — the ONLY module allowed to open TCP connections.
//!
//! Every connect is gated by Policy -> Broker (stage 1 semantics preserved).
//! The capability request is derived by the engine from the parsed URL, not
//! supplied by the requester. Only the user command (--fetch) triggers I/O;
//! there is no background traffic and no telemetry by construction.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::broker::Broker;
use crate::http;
use crate::policy::{Capability, Decision, Policy};
use crate::url::{self, Url};

pub const MAX_HEAD: usize = 64 * 1024;

#[derive(Debug)]
pub enum FetchError {
    UnsupportedScheme(String),
    Url(url::UrlError),
    Denied(Url),
    Dns(std::io::Error),
    Connect(std::io::Error),
    Io(std::io::Error),
    Http(http::HttpError),
    BodyTooLarge,
}

impl FetchError {
    pub fn code(&self) -> &'static str {
        match self {
            FetchError::UnsupportedScheme(_) => "UNSUPPORTED_SCHEME",
            FetchError::Url(e) => e.code(),
            FetchError::Denied(_) => "BROKER_DENIED",
            FetchError::Dns(_) => "DNS",
            FetchError::Connect(_) => "CONNECT",
            FetchError::Io(_) => "IO",
            FetchError::Http(e) => e.code(),
            FetchError::BodyTooLarge => "BODY_TOO_LARGE",
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            FetchError::Dns(e) | FetchError::Connect(e) | FetchError::Io(e) => Some(e.to_string()),
            _ => None,
        }
    }
}

pub struct FetchReport {
    pub url: Url,
    pub subject: String,
    pub resource: String,
    pub operation: String,
    pub scope: String,
    pub lifetime: String,
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers_count: usize,
    pub body: Vec<u8>,
}

/// Fetch a URL over plain HTTP/1.1 through the capability broker.
/// `subject_override` is a diagnostic/testing affordance (see README) —
/// normal use derives the subject from the URL.
pub fn fetch(
    raw_url: &str,
    subject_override: Option<&str>,
    timeout: Duration,
) -> Result<FetchReport, FetchError> {
    let scheme = url::extract_scheme(raw_url).ok_or(FetchError::Url(url::UrlError::MissingScheme))?;
    if scheme != "http" {
        return Err(FetchError::UnsupportedScheme(scheme));
    }
    let parsed = url::parse(raw_url).map_err(FetchError::Url)?;

    // Capability derivation — engine-side, requester-independent.
    let subject = subject_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("web-page:{}://{}", parsed.scheme, parsed.host));
    let cap = Capability {
        subject: subject.clone(),
        resource: "network".into(),
        operation: "connect".into(),
        scope: format!("{}:{}", parsed.host, parsed.port),
        lifetime: "request".into(),
    };

    let broker = Broker {
        policy: Policy {
            default_decision: Decision::Deny,
        },
    };
    if broker.request(&cap) != Decision::Allow {
        return Err(FetchError::Denied(parsed));
    }

    // Resolve + connect (the only connect site in the whole codebase).
    let target = url::connect_target(&parsed);
    let addrs: Vec<_> = target
        .as_str()
        .to_socket_addrs()
        .map_err(FetchError::Dns)?
        .collect();
    if addrs.is_empty() {
        return Err(FetchError::Dns(std::io::Error::new(
            std::io::ErrorKind::Other,
            "resolver returned no addresses",
        )));
    }
    let mut last_err: Option<std::io::Error> = None;
    let mut stream: Option<TcpStream> = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let mut stream = stream.ok_or_else(|| {
        FetchError::Connect(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "connect failed")
        }))
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(FetchError::Io)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(FetchError::Io)?;

    // HTTP/1.1 request (RFC 9110 subset; no HTTP/2, no TLS in this stage).
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: neutral-browser/0.7.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        parsed.path,
        url::host_header(&parsed)
    );
    stream.write_all(request.as_bytes()).map_err(FetchError::Io)?;
    stream.flush().map_err(FetchError::Io)?;

    // Read until end of response head (loop exits only when head end was found).
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 4096];
    loop {
        if http::find_subslice(&buf, b"\r\n\r\n").is_some() {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err(FetchError::Http(http::HttpError::HeadTooLarge));
        }
        let n = stream.read(&mut chunk).map_err(FetchError::Io)?;
        if n == 0 {
            return Err(FetchError::Http(http::HttpError::BadStatusLine));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let (mut resp, body_start) = http::parse_head(&buf).map_err(FetchError::Http)?;
    let mut body: Vec<u8> = buf[body_start..].to_vec();

    // Body framing: chunked -> Content-Length -> read-to-close (RFC 9112).
    let te = http::find_header(&resp, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    if te.contains("chunked") {
        loop {
            match http::decode_chunked(&body) {
                Ok(decoded) => {
                    if decoded.len() > http::MAX_BODY {
                        return Err(FetchError::BodyTooLarge);
                    }
                    resp.body = decoded;
                    break;
                }
                Err(_) => {
                    // Need more bytes before the chunk stream is complete.
                    let n = stream.read(&mut chunk).map_err(FetchError::Io)?;
                    if n == 0 {
                        return Err(FetchError::Http(http::HttpError::BadChunkFormat));
                    }
                    body.extend_from_slice(&chunk[..n]);
                    if body.len() > http::MAX_BODY + 8192 {
                        return Err(FetchError::BodyTooLarge);
                    }
                }
            }
        }
    } else if let Some(cl) = http::find_header(&resp, "content-length") {
        let want: usize = cl
            .trim()
            .parse()
            .map_err(|_| FetchError::Http(http::HttpError::BadContentLength))?;
        if want > http::MAX_BODY {
            return Err(FetchError::BodyTooLarge);
        }
        while body.len() < want {
            let n = stream.read(&mut chunk).map_err(FetchError::Io)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        if body.len() < want {
            return Err(FetchError::Http(http::HttpError::BadContentLength));
        }
        body.truncate(want);
        resp.body = body;
    } else {
        loop {
            let n = stream.read(&mut chunk).map_err(FetchError::Io)?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
            if body.len() > http::MAX_BODY {
                return Err(FetchError::BodyTooLarge);
            }
        }
        resp.body = body;
    }

    Ok(FetchReport {
        url: parsed,
        subject,
        resource: cap.resource,
        operation: cap.operation,
        scope: cap.scope,
        lifetime: cap.lifetime,
        version: resp.version,
        status: resp.status,
        reason: resp.reason,
        headers_count: resp.headers.len(),
        body: resp.body,
    })
}
