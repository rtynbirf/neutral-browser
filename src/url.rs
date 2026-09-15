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
