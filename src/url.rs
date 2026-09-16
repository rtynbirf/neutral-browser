//! Minimal URL parser — a documented subset of WHATWG URL.
//!
//! Supported: scheme://[userinfo@]host[:port]/path[?query]
//!   host: reg-name, IPv4 literal, or bracketed [IPv6] literal.
//! Deliberately NOT implemented (see README status table):
//!   percent-normalization, IDNA/punycode, opaque paths, fragment splitting.
//! Security: host and path are whitelisted to printable ASCII so that no
//! CRLF/control injection can reach the request line or the Host header.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String, // path + optional query, always starts with '/'
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    MissingScheme,
    EmptyHost,
    BadHost,
    BadPort,
    MissingPort,
    BadPath,
}

impl UrlError {
    pub fn code(&self) -> &'static str {
        match self {
            UrlError::MissingScheme => "MISSING_SCHEME",
            UrlError::EmptyHost => "EMPTY_HOST",
            UrlError::BadHost => "BAD_HOST",
            UrlError::BadPort => "BAD_PORT",
            UrlError::MissingPort => "MISSING_PORT",
            UrlError::BadPath => "BAD_PATH",
        }
    }
}

pub fn extract_scheme(raw: &str) -> Option<String> {
    let idx = raw.find("://")?;
    let s = &raw[..idx];
    if s.is_empty() {
        return None;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return None;
    }
    Some(s.to_ascii_lowercase())
}

pub fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '%' | '[' | ']' | ':' | '@'))
}

fn valid_path(p: &str) -> bool {
    p.starts_with('/') && p.chars().all(|c| c.is_ascii_graphic())
}

pub fn parse(raw: &str) -> Result<Url, UrlError> {
    let scheme = extract_scheme(raw).ok_or(UrlError::MissingScheme)?;
    let sep = raw.find("://").unwrap_or(0) + 3;
    let rest = &raw[sep..];

    // Split authority from path/query.
    let auth_end = rest.find(|c| c == '/' || c == '?').unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let path_part = &rest[auth_end..];
    let path = if path_part.is_empty() {
        "/".to_string()
    } else if path_part.starts_with('?') {
        format!("/{}", path_part) // empty path with query
    } else {
        path_part.to_string()
    };
    if !valid_path(&path) {
        return Err(UrlError::BadPath);
    }

    // Drop userinfo: this client never forwards credentials.
    let hostport = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };

    let (host, port_str) = if let Some(stripped) = hostport.strip_prefix('[') {
        // IPv6 literal
        let close = stripped.find(']').ok_or(UrlError::EmptyHost)?;
        let host = &stripped[..close];
        let after = &stripped[close + 1..];
        (host, after.strip_prefix(':'))
    } else {
        match hostport.rfind(':') {
            Some(i) => (&hostport[..i], Some(&hostport[i + 1..])),
            None => (hostport, None),
        }
    };

    if host.is_empty() {
        return Err(UrlError::EmptyHost);
    }
    if !valid_host(host) {
        return Err(UrlError::BadHost);
    }

    let port = match port_str {
        Some("") => default_port(&scheme).ok_or(UrlError::MissingPort)?,
        Some(p) => p.parse::<u16>().map_err(|_| UrlError::BadPort)?,
        None => default_port(&scheme).ok_or(UrlError::MissingPort)?,
    };

    Ok(Url {
        scheme,
        host: host.to_string(),
        port,
        path,
    })
}

/// Host header value per RFC 9110: omit the port when it is the scheme default.
pub fn host_header(u: &Url) -> String {
    let h = if u.host.contains(':') {
        format!("[{}]", u.host)
    } else {
        u.host.clone()
    };
    if Some(u.port) == default_port(&u.scheme) {
        h
    } else {
        format!("{}:{}", h, u.port)
    }
}

/// Address string accepted by std's ToSocketAddrs (brackets for IPv6).
pub fn connect_target(u: &Url) -> String {
    if u.host.contains(':') {
        format!("[{}]:{}", u.host, u.port)
    } else {
        format!("{}:{}", u.host, u.port)
    }
}

/// Origin string "scheme://host[:port]" (bracketed IPv6), re-parseable by
/// `parse`. The port is omitted only when it is the scheme default.
fn origin(u: &Url) -> String {
    let host = if u.host.contains(':') {
        format!("[{}]", u.host)
    } else {
        u.host.clone()
    };
    if default_port(&u.scheme) == Some(u.port) {
        format!("{}://{}", u.scheme, host)
    } else {
        format!("{}://{}:{}", u.scheme, host, u.port)
    }
}

/// RFC 3986 §5.2.4 subset for absolute paths: "." segments are dropped,
/// ".." pops the previous segment (staying at the root when already there),
/// interior empty segments collapse, a trailing "."/".." yields a trailing
/// slash, empty result becomes "/".
fn remove_dot_segments(path: &str) -> String {
    debug_assert!(path.starts_with('/'));
    let mut out: Vec<&str> = Vec::new();
    let mut trailing_slash = false;
    for seg in path.split('/').skip(1) {
        match seg {
            "." | "" => trailing_slash = true,
            ".." => {
                out.pop();
                trailing_slash = true;
            }
            _ => {
                out.push(seg);
                trailing_slash = false;
            }
        }
    }
    let mut s = String::from("/");
    s.push_str(&out.join("/"));
    if trailing_slash && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// Resolve a stylesheet link href against the base URL — a documented
/// subset of RFC 3986 §5:
///   - empty href = the base URL itself (fragment stripped)
///   - absolute URI passthrough (contains "://"; the fetch validates it)
///   - scheme-relative "//host/path" inherits the base scheme
///   - absolute path "/path" merges onto the base origin
///   - query-only "?q" merges onto the base path without its query
///   - relative path merges onto the base directory with dot-segment
///     removal; a colon inside a relative href stays in the path (no
///     scheme sniffing — documented deviation)
///   - fragments are always dropped; no percent-decoding, no backslash
///     magic, no userinfo
/// Returns None when the href cannot yield a fetchable URL (control or
/// non-graphic characters).
pub fn resolve(base: &Url, href: &str) -> Option<String> {
    let h = href.trim_matches(|c: char| c.is_ascii_whitespace());
    let h = match h.find('#') {
        Some(i) => &h[..i],
        None => h,
    };
    if h.is_empty() {
        return Some(format!("{}{}", origin(base), base.path));
    }
    if !h.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    if h.contains("://") {
        return Some(h.to_string());
    }
    if h.starts_with("//") {
        return Some(format!("{}:{}", base.scheme, h));
    }
    if h.starts_with('/') {
        return Some(format!("{}{}", origin(base), remove_dot_segments(h)));
    }
    let bp = base.path.split('?').next().unwrap_or("/");
    if h.starts_with('?') {
        return Some(format!("{}{}{}", origin(base), bp, h));
    }
    let dir_end = bp.rfind('/').map(|i| i + 1).unwrap_or(0);
    let merged = format!("{}{}", &bp[..dir_end], h);
    Some(format!("{}{}", origin(base), remove_dot_segments(&merged)))
}
