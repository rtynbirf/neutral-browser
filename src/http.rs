//! HTTP/1.1 subset: response head parsing, header lookup, chunked decoding.
//!
//! Pure functions — no I/O here. Socket I/O lives in network.rs.
//! Basis: RFC 9110 (semantics), RFC 9112 (framing). obs-fold is rejected
//! (deprecated by RFC 9112 §5.2).

pub const MAX_BODY: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>, // names lowercased, order preserved
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    BadStatusLine,
    BadHeader,
    BadContentLength,
    BadChunkFormat,
    HeadTooLarge,
}

impl HttpError {
    pub fn code(&self) -> &'static str {
        match self {
            HttpError::BadStatusLine => "BAD_STATUS_LINE",
            HttpError::BadHeader => "BAD_HEADER",
            HttpError::BadContentLength => "BAD_CONTENT_LENGTH",
            HttpError::BadChunkFormat => "BAD_CHUNK_FORMAT",
            HttpError::HeadTooLarge => "HEAD_TOO_LARGE",
        }
    }
}

pub(crate) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn lines_of(buf: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(buf);
    text.split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect()
}

/// Parse the response head (everything before the first CRLFCRLF).
/// Returns the response with an empty body plus the offset where the body starts.
pub fn parse_head(buf: &[u8]) -> Result<(Response, usize), HttpError> {
    let head_end = find_subslice(buf, b"\r\n\r\n").ok_or(HttpError::HeadTooLarge)?;
    let lines = lines_of(&buf[..head_end]);
    if lines.is_empty() {
        return Err(HttpError::BadStatusLine);
    }
    let mut parts = lines[0].splitn(3, ' ');
    let version = parts.next().unwrap_or("").to_string();
    if !version.starts_with("HTTP/") {
        return Err(HttpError::BadStatusLine);
    }
    let status = parts
        .next()
        .unwrap_or("")
        .parse::<u16>()
        .map_err(|_| HttpError::BadStatusLine)?;
    let reason = parts.next().unwrap_or("").to_string();

    let mut headers = Vec::new();
    for line in &lines[1..] {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(HttpError::BadHeader); // obs-fold rejected
        }
        let (name, value) = line.split_once(':').ok_or(HttpError::BadHeader)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    Ok((
        Response {
            version,
            status,
            reason,
            headers,
            body: Vec::new(),
        },
        head_end + 4,
    ))
}

pub fn find_header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Decode a chunked body (RFC 9112 §7.1) from a complete byte buffer.
/// Chunk extensions (`;ext=...`) are ignored; the terminating chunk ends
/// decoding; trailers after it are not consumed (subset, documented).
pub fn decode_chunked(data: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    loop {
        let line_end = find_subslice(&data[pos..], b"\r\n").ok_or(HttpError::BadChunkFormat)?;
        let size_line = String::from_utf8_lossy(&data[pos..pos + line_end]).to_string();
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| HttpError::BadChunkFormat)?;
        pos += line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        if pos + size + 2 > data.len() {
            return Err(HttpError::BadChunkFormat);
        }
        out.extend_from_slice(&data[pos..pos + size]);
        if out.len() > MAX_BODY {
            return Err(HttpError::BadChunkFormat);
        }
        pos += size;
        if data.get(pos) == Some(&b'\r') {
            pos += 1;
        }
        if data.get(pos) == Some(&b'\n') {
            pos += 1;
        }
    }
}
