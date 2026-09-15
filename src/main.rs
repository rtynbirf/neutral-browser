//! neutral-browser — user-owned browser, stage 2: broker-gated fetch chain.
//!
//! Chain: URL -> Broker -> TCP -> HTTP/1.1 -> body -> machine report.
//! Nothing is done implicitly: the network is touched only by --fetch.

mod broker;
mod http;
mod network;
mod policy;
mod sha256;
mod url;

use std::process::exit;

use broker::Broker;
use policy::{Capability, Decision, Policy};

const VERSION: &str = "0.3.0";

fn main() {
    exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--self-test") {
        return self_test();
    }
    if let Some(i) = args.iter().position(|a| a == "--fetch") {
        let raw = match args.get(i + 1) {
            Some(v) if !v.starts_with("--") => v.clone(),
            _ => {
                println!("FETCH=ERROR");
                println!("ERROR_KIND=USAGE");
                return 2;
            }
        };
        return fetch_command(&args, &raw);
    }
    println!("neutral-browser {}", VERSION);
    println!("STATUS=STAGE2_FETCH");
    println!("CHAIN=URL->BROKER->TCP->HTTP/1.1->BODY");
    println!(
        "NOT_IMPLEMENTED=https,http2,http3,redirects,dns-config,html,dom,css,rendering,javascript"
    );
    println!(
        "USAGE: neutral-browser --fetch <url> [--output <file>] [--subject <s>] [--timeout-ms <n>] | --self-test"
    );
    0
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn fetch_command(args: &[String], raw: &str) -> i32 {
    println!("URL={}", raw);
    let subject_override = flag(args, "--subject");
    let timeout_ms: u64 = flag(args, "--timeout-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10000);
    let timeout = std::time::Duration::from_millis(timeout_ms);

    // Scheme gate before parsing: only plain http exists in this stage.
    match url::extract_scheme(raw) {
        Some(s) if s != "http" => {
            println!("SCHEME={}", s);
            println!("SUPPORT=NOT_IMPLEMENTED");
            if s == "https" {
                println!("PLANNED=vendored-rustls-per-DEPENDENCY_POLICY");
            }
            println!("FETCH=ERROR");
            println!("ERROR_KIND=UNSUPPORTED_SCHEME");
            return 3;
        }
        None => {
            println!("FETCH=ERROR");
            println!("ERROR_KIND=MISSING_SCHEME");
            return 5;
        }
        _ => {}
    }

    match network::fetch(raw, subject_override.as_deref(), timeout) {
        Ok(rep) => {
            println!(
                "SCHEME={} HOST={} PORT={} PATH={}",
                rep.url.scheme, rep.url.host, rep.url.port, rep.url.path
            );
            println!("BROKER_SUBJECT={}", rep.subject);
            println!("BROKER_RESOURCE={}", rep.resource);
            println!("BROKER_OPERATION={}", rep.operation);
            println!("BROKER_SCOPE={}", rep.scope);
            println!("BROKER_LIFETIME={}", rep.lifetime);
            println!("BROKER_DECISION=ALLOW");
            println!("CONNECT=OK");
            println!("HTTP_VERSION={}", rep.version);
            println!("HTTP_STATUS={}", rep.status);
            println!("HTTP_REASON={}", rep.reason);
            println!("HEADERS={}", rep.headers_count);
            println!("BODY_BYTES={}", rep.body.len());
            let digest = sha256::sha256(&rep.body);
            println!("BODY_SHA256={}", sha256::hex(&digest));
            if let Some(out) = flag(args, "--output") {
                match std::fs::write(&out, &rep.body) {
                    Ok(_) => println!("BODY_SAVED={}", out),
                    Err(e) => println!("BODY_SAVE_ERROR={}", e),
                }
            }
            println!("FETCH=OK");
            0
        }
        Err(network::FetchError::UnsupportedScheme(s)) => {
            println!("SCHEME={}", s);
            println!("SUPPORT=NOT_IMPLEMENTED");
            println!("FETCH=ERROR");
            println!("ERROR_KIND=UNSUPPORTED_SCHEME");
            3
        }
        Err(network::FetchError::Denied(u)) => {
            println!(
                "SCHEME={} HOST={} PORT={} PATH={}",
                u.scheme, u.host, u.port, u.path
            );
            println!("BROKER_DECISION=DENY");
            println!("NETWORK=CLOSED");
            println!("FETCH=DENIED");
            4
        }
        Err(e) => {
            println!("FETCH=ERROR");
            println!("ERROR_KIND={}", e.code());
            if let Some(d) = e.detail() {
                println!("DETAIL={}", d);
            }
            5
        }
    }
}

fn self_test() -> i32 {
    // ---- Stage 1 invariants: policy/broker (unchanged semantics) ----
    let broker = Broker {
        policy: Policy {
            default_decision: Decision::Deny,
        },
    };

    let filesystem_read = Capability {
        subject: "web-page:https://example.test".into(),
        resource: "filesystem".into(),
        operation: "read".into(),
        scope: "/home/user/private".into(),
        lifetime: "request".into(),
    };
    let filesystem_write = Capability {
        subject: "web-page:https://example.test".into(),
        resource: "filesystem".into(),
        operation: "write".into(),
        scope: "/home/user/private".into(),
        lifetime: "request".into(),
    };
    let camera_use = Capability {
        subject: "web-page:https://example.test".into(),
        resource: "camera".into(),
        operation: "use".into(),
        scope: "*".into(),
        lifetime: "request".into(),
    };
    let network_connect = Capability {
        subject: "web-page:https://example.test".into(),
        resource: "network".into(),
        operation: "connect".into(),
        scope: "https://example.test".into(),
        lifetime: "request".into(),
    };
    let non_web = Capability {
        subject: "untrusted-process".into(),
        resource: "network".into(),
        operation: "connect".into(),
        scope: "*".into(),
        lifetime: "request".into(),
    };

    assert_eq!(broker.request(&filesystem_read), Decision::Deny);
    assert_eq!(broker.request(&filesystem_write), Decision::Deny);
    assert_eq!(broker.request(&camera_use), Decision::Deny);
    assert_eq!(broker.request(&network_connect), Decision::Allow);
    assert_eq!(broker.request(&non_web), Decision::Deny);

    println!("SELF_TEST=PASS");
    println!("WEB_TO_OS=DENY");
    println!("WEB_NETWORK=ALLOW");
    println!("NON_WEB_NETWORK=DENY");

    // ---- Stage 2: URL subset ----
    let u = url::parse("http://example.test").unwrap();
    assert_eq!(u.scheme, "http");
    assert_eq!(u.host, "example.test");
    assert_eq!(u.port, 80);
    assert_eq!(u.path, "/");

    let u = url::parse("http://127.0.0.1:18080/index.html?a=1").unwrap();
    assert_eq!(u.host, "127.0.0.1");
    assert_eq!(u.port, 18080);
    assert_eq!(u.path, "/index.html?a=1");

    let u = url::parse("http://[::1]:19090/x").unwrap();
    assert_eq!(u.host, "::1");
    assert_eq!(u.port, 19090);
    assert_eq!(url::connect_target(&u), "[::1]:19090");

    assert!(url::parse("http://user:pw@example.test/x").is_ok()); // userinfo dropped
    assert_eq!(url::parse("http://").unwrap_err(), url::UrlError::EmptyHost);
    assert_eq!(
        url::parse("example.test").unwrap_err(),
        url::UrlError::MissingScheme
    );
    assert_eq!(
        url::parse("http://exa mple.test/").unwrap_err(),
        url::UrlError::BadHost
    );
    assert_eq!(
        url::parse("http://ok.test/a b").unwrap_err(),
        url::UrlError::BadPath
    );
    println!("URL_PARSE=PASS");

    // ---- Stage 2: HTTP head parsing ----
    let raw: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello";
    let (resp, off) = http::parse_head(raw).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.version, "HTTP/1.1");
    assert_eq!(resp.reason, "OK");
    assert_eq!(http::find_header(&resp, "content-length"), Some("5"));
    assert_eq!(&raw[off..], b"hello");
    assert!(http::parse_head(b"nonsense\r\n\r\n").is_err());
    assert!(http::parse_head(b"HTTP/1.1 200 OK\r\nbroken\r\n\r\n").is_err());
    println!("HTTP_PARSE=PASS");

    // ---- Stage 2: chunked decoding ----
    let dec = http::decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap();
    assert_eq!(dec, b"Wikipedia");
    let dec = http::decode_chunked(b"3;ext=1\r\nabc\r\n0\r\n\r\n").unwrap();
    assert_eq!(dec, b"abc");
    assert!(http::decode_chunked(b"ZZ\r\nx").is_err());
    println!("CHUNKED_DECODE=PASS");

    // ---- Stage 2: SHA-256 published NIST vectors (FIPS 180-4) ----
    assert_eq!(
        sha256::hex(&sha256::sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256::hex(&sha256::sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256::hex(&sha256::sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    println!("SHA256_VECTORS=PASS");

    0
}
