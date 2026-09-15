//! neutral-browser — user-owned browser, stage 5: computed STYLE.
//!
//! Chain: URL -> Broker -> TCP -> HTTP/1.1 -> body -> HTML -> DOM -> CSS -> STYLE.
//! Nothing is done implicitly: the network is touched only by --fetch;
//! parsing happens on the fetched body (--dom/--style) or on the user's own
//! files (--dom-file/--css-file, the user is the principal — not a web
//! capability). Stage 5: <link rel=stylesheet> sheets are fetched through
//! the same broker-gated chain (base URL from --fetch only), and inherited
//! properties propagate computed values down the DOM (STYLE v2 dump).

mod broker;
mod css;
mod dom;
mod html;
mod http;
mod network;
mod policy;
mod sha256;
mod url;

use std::process::exit;

use broker::Broker;
use policy::{Capability, Decision, Policy};

const VERSION: &str = "0.6.0";

/// Cap on distinct <link rel=stylesheet> sheets fetched per page: link
/// fetches are engine-triggered network I/O and must stay bounded.
const MAX_LINK_SHEETS: usize = 8;

fn main() {
    exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--self-test") {
        return self_test();
    }
    if let Some(i) = args.iter().position(|a| a == "--dom-file") {
        let raw = match args.get(i + 1) {
            Some(v) if !v.starts_with("--") => v.clone(),
            _ => {
                println!("DOM=ERROR");
                println!("ERROR_KIND=USAGE");
                return 2;
            }
        };
        return dom_file_command(&args, &raw);
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
    println!("STATUS=STAGE5_COMPUTED");
    println!("CHAIN=URL->BROKER->TCP->HTTP/1.1->BODY->HTML->DOM->CSS->STYLE");
    println!(
        "NOT_IMPLEMENTED=https,http2,http3,redirects,dns-config,css-at-rules,css-initial-values,css-inherit-keyword,css-link-media-attr,css-import-following,css-advanced-selectors,rendering,javascript,foster-parenting,adoption-agency,foreign-content,template-modes,full-named-entities"
    );
    println!(
        "USAGE: neutral-browser --fetch <url> [--dom] [--style] [--dom-output <f>] [--style-output <f>] [--output <f>] [--subject <s>] [--timeout-ms <n>] | --dom-file <path> [--style] [--css-file <path>] [--dom-output <f>] [--style-output <f>] | --self-test"
    );
    0
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Parse a user-supplied local HTML file. The user is the principal here:
/// this is the user acting on their own file from their own command line,
/// not a capability of web content (web content never gets file access;
/// the stage-1 broker policy is untouched).
fn dom_file_command(args: &[String], path: &str) -> i32 {
    println!("DOM_INPUT={}", path);
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!("DOM=ERROR");
            println!("ERROR_KIND=IO");
            println!("DETAIL={}", e);
            return 5;
        }
    };
    let html_src = String::from_utf8_lossy(&bytes).into_owned();
    let r = dom::build(&html_src);
    let rc = dom_report(&r, flag(args, "--dom-output").as_deref());
    if rc != 0 {
        return rc;
    }
    if has_flag(args, "--style") {
        return style_stage(
            &r.dom,
            flag(args, "--css-file").as_deref(),
            flag(args, "--style-output").as_deref(),
            None,
            std::time::Duration::ZERO,
            None,
        );
    }
    0
}

/// Print the machine-readable DOM report block from a parsed tree.
/// Exit code 0 on success (parse errors do not abort — HTML recovery is
/// normative), 5 if the dump file cannot be written.
fn dom_report(r: &dom::ParseResult, dump_out: Option<&str>) -> i32 {
    let c = r.dom.counts();
    println!("DOM_NODES={}", c.nodes);
    println!("DOM_ELEMENTS={}", c.elements);
    println!("DOM_TEXTS={}", c.texts);
    println!("DOM_COMMENTS={}", c.comments);
    println!("DOM_DOCTYPE={}", c.doctype.unwrap_or_default());
    println!("DOM_DEPTH={}", c.depth);
    println!("PARSE_ERRORS={}", r.errors);
    if !r.kinds.is_empty() {
        println!("PARSE_ERROR_KINDS={}", r.kinds.join(","));
    }
    if let Some(out) = dump_out {
        match std::fs::write(out, r.dom.dump()) {
            Ok(_) => println!("DOM_SAVED={}", out),
            Err(e) => {
                println!("DOM_SAVE_ERROR={}", e);
                println!("DOM=ERROR");
                return 5;
            }
        }
    }
    println!("DOM=OK");
    0
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Outcome of one <link rel=stylesheet> sheet acquisition.
enum LinkOutcome {
    Fetched(String, String), // body text, resolved url
    Failed(String),          // machine kind
}

/// Fetch one link sheet through the broker-gated chain. Resolve the href
/// against the page base URL (only --fetch provides one), dedupe and cap
/// upstream. A failed sheet never aborts the cascade — it is counted.
fn link_sheet(
    href: &str,
    base: Option<&url::Url>,
    timeout: std::time::Duration,
    subject_override: Option<&str>,
) -> LinkOutcome {
    let Some(b) = base else {
        return LinkOutcome::Failed("NO_BASE_URL".into());
    };
    if href.is_empty() {
        return LinkOutcome::Failed("NO_HREF".into());
    }
    let Some(u) = url::resolve(b, href) else {
        return LinkOutcome::Failed("BAD_URL".into());
    };
    match network::fetch(&u, subject_override, timeout) {
        Ok(rep) if rep.status == 200 => {
            LinkOutcome::Fetched(String::from_utf8_lossy(&rep.body).into_owned(), u)
        }
        Ok(rep) => LinkOutcome::Failed(format!("HTTP_{}", rep.status)),
        Err(e) => LinkOutcome::Failed(e.code().to_string()),
    }
}

/// Sanitize an href for the report: control characters become '?'.
fn display_href(h: &str) -> String {
    h.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// Stage 5: cascade the stylesheets over the DOM and report STYLE.
/// CSS sources, in document order: <style> elements and <link rel=stylesheet>
/// sheets fetched over the same broker-gated chain (base URL comes only
/// from --fetch); after them the user's own --css-file (user principal).
/// Sheets are parsed in document order — the cascade source order spans
/// all sheets. CSS parse errors and failed link sheets never abort (both
/// are counted); only file I/O failures (css file, dump write) exit 5.
fn style_stage(
    tree: &dom::Dom,
    css_file: Option<&str>,
    style_output: Option<&str>,
    base: Option<&url::Url>,
    timeout: std::time::Duration,
    subject_override: Option<&str>,
) -> i32 {
    let list = css::collect_sheets(tree);
    let mut rules = Vec::new();
    let mut errors = 0usize;
    let mut at_skip = 0usize;
    let mut decls_total = 0usize;

    println!("STYLE_SRC_ELEMENTS={}", list.style_elems);
    println!("STYLE_LINK_SHEETS={}", list.link_elems);

    let mut fetched = 0usize;
    let mut failed = 0usize;
    let mut dupes = 0usize;
    let mut attempts = 0usize;
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for sheet in &list.sheets {
        match sheet {
            css::SheetRef::Inline(text) => {
                let o = css::parse(text);
                decls_total += o.decl_count();
                rules.extend(o.rules);
                errors += o.errors;
                at_skip += o.at_rules_skipped;
            }
            css::SheetRef::Link { href } => {
                if !href.is_empty() {
                    if let Some(u) = base.and_then(|b| url::resolve(b, href)) {
                        if !seen.insert(u.clone()) {
                            dupes += 1;
                            continue; // same URL: one sheet, browser-style dedupe
                        }
                    }
                }
                if attempts >= MAX_LINK_SHEETS {
                    failed += 1;
                    println!("LINK_SHEET_FAIL={}", display_href(href));
                    println!("LINK_SHEET_KIND=LIMIT");
                    continue;
                }
                attempts += 1;
                match link_sheet(href, base, timeout, subject_override) {
                    LinkOutcome::Fetched(text, u) => {
                        fetched += 1;
                        println!("LINK_SHEET_FETCHED={}", u);
                        let o = css::parse(&text);
                        decls_total += o.decl_count();
                        rules.extend(o.rules);
                        errors += o.errors;
                        at_skip += o.at_rules_skipped;
                    }
                    LinkOutcome::Failed(kind) => {
                        failed += 1;
                        println!("LINK_SHEET_FAIL={}", display_href(href));
                        println!("LINK_SHEET_KIND={}", kind);
                    }
                }
            }
        }
    }
    println!("STYLE_LINK_SHEETS_FETCHED={}", fetched);
    println!("STYLE_LINK_SHEETS_FAILED={}", failed);
    println!("STYLE_LINK_DUPES={}", dupes);
    if let Some(p) = css_file {
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                println!("STYLE=ERROR");
                println!("ERROR_KIND=IO");
                println!("DETAIL={}", e);
                return 5;
            }
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let o2 = css::parse(&text);
        decls_total += o2.decl_count();
        rules.extend(o2.rules);
        errors += o2.errors;
        at_skip += o2.at_rules_skipped;
    }
    println!("STYLE_RULES={}", rules.len());
    println!("STYLE_DECLS={}", decls_total);
    println!("STYLE_AT_RULES_SKIPPED={}", at_skip);
    println!("STYLE_ERRORS={}", errors);
    let ap = css::apply(tree, &rules);
    println!("STYLE_ELEMENTS_STYLED={}", ap.styled);
    println!("STYLE_PROPS={}", ap.props);
    println!("STYLE_INHERITED={}", ap.inherited);
    println!("STYLE_INHERITED_ELEMENTS={}", ap.inherited_elements);
    if let Some(out_path) = style_output {
        match std::fs::write(&out_path, css::dump(&ap)) {
            Ok(_) => println!("STYLE_SAVED={}", out_path),
            Err(e) => {
                println!("STYLE_SAVE_ERROR={}", e);
                println!("STYLE=ERROR");
                return 5;
            }
        }
    }
    println!("STYLE=OK");
    0
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
            if has_flag(args, "--dom") || has_flag(args, "--style") {
                let html_src = String::from_utf8_lossy(&rep.body).into_owned();
                let r = dom::build(&html_src);
                if has_flag(args, "--dom") {
                    let rc = dom_report(&r, flag(args, "--dom-output").as_deref());
                    if rc != 0 {
                        return rc;
                    }
                }
                if has_flag(args, "--style") {
                    let rc = style_stage(
                        &r.dom,
                        flag(args, "--css-file").as_deref(),
                        flag(args, "--style-output").as_deref(),
                        Some(&rep.url),
                        timeout,
                        subject_override.as_deref(),
                    );
                    if rc != 0 {
                        return rc;
                    }
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

    // ---- Stage 3: HTML tokenizer (WHATWG §13.5 subset) ----
    let (toks, e) = html::collect(r#"<p CLASS=a data-x="1&amp;2" q='z'>t &amp; u</p>"#);
    assert_eq!(e, 0);
    assert_eq!(
        toks[0],
        html::Token::StartTag {
            name: "p".into(),
            attrs: vec![
                ("class".into(), "a".into()),
                ("data-x".into(), "1&2".into()),
                ("q".into(), "z".into()),
            ],
            self_closing: false,
        }
    );
    assert_eq!(toks[1], html::Token::Text("t & u".into()));
    assert_eq!(toks[2], html::Token::EndTag { name: "p".into() });

    let (toks, e) = html::collect("&#128;&copy;&unknown;");
    assert_eq!(e, 0);
    // windows-1252 remap (128 -> euro), named ref, unknown ref stays literal
    assert_eq!(toks[0], html::Token::Text("\u{20AC}\u{A9}&unknown;".into()));

    let (toks, _) = html::collect(r#"<script>var s="</p>"; if(a<b) X();</script>after"#);
    assert_eq!(
        toks[1],
        html::Token::Text(r#"var s="</p>"; if(a<b) X();"#.into())
    );
    assert_eq!(toks[2], html::Token::EndTag { name: "script".into() });
    assert_eq!(toks[3], html::Token::Text("after".into()));

    let (toks, e) = html::collect("<!----><!--x--!>");
    let comments: Vec<String> = toks
        .into_iter()
        .filter_map(|t| match t {
            html::Token::Comment(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(comments, vec!["".to_string(), "x".to_string()]);
    // spec: `--!>` close = 2 parse errors (bang + emit)
    assert_eq!(e, 2);
    println!("HTML_TOKENIZER=PASS");

    // ---- Stage 3: DOM tree construction (WHATWG §13.2.6 subset) ----
    let r = dom::build("<p>hi</p>");
    assert_eq!(
        r.dom.dump(),
        "#document\n  <html>\n    <head>\n    <body>\n      <p>\n        \"hi\"\n"
    );
    assert_eq!(r.errors, 0);

    let r = dom::build(
        "<!doctype html><html><head><title>t</title></head><body><!--c--><h1>h</h1><p>p</p></body></html>",
    );
    let c = r.dom.counts();
    assert_eq!(c.elements, 6);
    assert_eq!(c.texts, 3);
    assert_eq!(c.comments, 1);
    assert_eq!(c.nodes, 12);
    assert_eq!(c.depth, 4);
    assert_eq!(c.doctype, Some("html".into()));
    assert_eq!(r.errors, 0);

    let r = dom::build("</br>x");
    assert!(r.dom.dump().contains("<br>"));
    let r = dom::build("<div><p>one<p>two<ul><li>i<li>j</ul></div>");
    assert_eq!(r.dom.counts().elements, 9); // auto-close p and li

    let r = dom::build("<script>if(a<b){x=\"</div>\"}</script>");
    assert!(r.dom.dump().contains("\"if(a<b){x=\\\"</div>\\\"}\""));

    // deep nesting: iterative algorithms, no stack overflow
    let n = 1000;
    let mut s = String::new();
    for _ in 0..n {
        s.push_str("<div>");
    }
    for _ in 0..n {
        s.push_str("</div>");
    }
    let c = dom::build(&s).dom.counts();
    assert_eq!(c.elements, n + 3);
    assert_eq!(c.depth, n + 2);
    println!("DOM_TREE=PASS");

    // ---- Stage 4: CSS tokenizer (CSS Syntax L3 subset) ----
    let (t, e) = css::tokenize("a.b#c[x=\"1\"] { color: red !important }");
    assert_eq!(e, 0);
    assert_eq!(t[0].t, css::Tok::Ident("a".into()));
    assert_eq!(t[1].t, css::Tok::Delim('.'));
    assert_eq!(t[2].t, css::Tok::Ident("b".into()));
    assert_eq!(t[3].t, css::Tok::Hash("c".into()));
    assert_eq!(t[4].t, css::Tok::LBracket);
    assert_eq!(t[5].t, css::Tok::Ident("x".into()));
    assert_eq!(t[6].t, css::Tok::Delim('='));
    assert_eq!(t[7].t, css::Tok::Str("1".into()));
    assert_eq!(t[8].t, css::Tok::RBracket);
    assert_eq!(t[10].t, css::Tok::LBrace);
    assert_eq!(t[13].t, css::Tok::Colon);
    assert!(t.iter().any(|x| x.t == css::Tok::Delim('!')));
    let (t, e) = css::tokenize("url(a/b.png) 12px 50% rgb(1,2,3) \"s\"");
    assert_eq!(e, 0);
    assert_eq!(t[0].t, css::Tok::Url("a/b.png".into()));
    assert_eq!(t[2].t, css::Tok::Dim("12".into(), "px".into()));
    assert_eq!(t[4].t, css::Tok::Percent("50".into()));
    assert_eq!(t[6].t, css::Tok::Function("rgb".into()));
    assert_eq!(t[14].t, css::Tok::Str("s".into()));
    assert_eq!(css::tokenize("\"a").1, 1); // EOF in string is an error
    assert_eq!(css::tokenize("url(a b)").1, 1); // bad url is an error
    println!("CSS_TOKENIZER=PASS");

    // ---- Stage 4: CSS parse (rules, selectors, recovery) ----
    let o = css::parse("p, .x { color: red; font-size : 12px }");
    assert_eq!(o.errors, 0);
    assert_eq!(o.rules.len(), 1);
    assert_eq!(o.rules[0].selectors[0].spec, (0, 0, 1));
    assert_eq!(o.rules[0].selectors[1].spec, (0, 1, 0));
    assert_eq!(o.rules[0].decls[1].value, "12px");

    let o = css::parse(
        "#y { color: blue !important } @media screen { p { color: white } } p { color: red }",
    );
    assert_eq!(o.errors, 0);
    assert_eq!(o.at_rules_skipped, 1);
    assert_eq!(o.rules.len(), 2);
    assert!(o.rules[0].decls[0].important);

    let o = css::parse("p:hover { color: red } a { color: }"); // selector dropped, decl dropped
    assert_eq!(o.rules.len(), 1); // rule `a` survives with zero declarations
    assert_eq!(o.rules[0].decls.len(), 0);
    assert_eq!(o.errors, 2);
    println!("CSS_PARSE=PASS");

    // ---- Stage 4: cascade -> STYLE ----
    let doc = dom::build(r#"<p class="x" id="y">t</p>"#);
    let o = css::parse("p { color: red } .x { color: green } #y { color: blue }");
    let ap = css::apply(&doc.dom, &o.rules);
    assert_eq!(ap.styled, 1);
    assert_eq!(ap.elements[0].index, 3); // html head body p
    assert_eq!(ap.elements[0].path, "html>body>p");
    let color = ap.elements[0]
        .decls
        .iter()
        .find(|(k, _)| k == "color")
        .unwrap();
    assert_eq!(color.1, "blue"); // id (1,0,0) beats class and type

    let doc = dom::build(r#"<div id="a"><p style="color: black">x</p></div>"#);
    let o = css::parse("p { color: red }");
    let ap = css::apply(&doc.dom, &o.rules);
    let p = &ap.elements.iter().find(|e| e.index == 4).unwrap();
    assert_eq!(
        p.decls.iter().find(|(k, _)| k == "color").unwrap().1,
        "black"
    ); // inline beats sheet normal
    let o = css::parse("#a p { color: green !important }");
    let ap = css::apply(&doc.dom, &o.rules);
    let p = &ap.elements.iter().find(|e| e.index == 4).unwrap();
    assert_eq!(
        p.decls.iter().find(|(k, _)| k == "color").unwrap().1,
        "green"
    ); // sheet important beats inline normal

    let doc = dom::build(r#"<p style="color: red">x</p>"#);
    let o = css::parse("p { margin: 0 }");
    let ap = css::apply(&doc.dom, &o.rules);
    assert_eq!(
        css::dump(&ap),
        "STYLE v2\nE3 html>body>p\n  color: red\n  margin: 0\n"
    );
    println!("CSS_CASCADE=PASS");

    // ---- Stage 5: URL resolution for link sheets ----
    let b = url::parse("http://127.0.0.1:18080/dir/page.html?x=1").unwrap();
    assert_eq!(
        url::resolve(&b, "http://other.test/a.css").as_deref(),
        Some("http://other.test/a.css")
    );
    assert_eq!(
        url::resolve(&b, "//other.test/a.css").as_deref(),
        Some("http://other.test/a.css")
    );
    assert_eq!(
        url::resolve(&b, "/root.css").as_deref(),
        Some("http://127.0.0.1:18080/root.css")
    );
    assert_eq!(
        url::resolve(&b, "sheet.css").as_deref(),
        Some("http://127.0.0.1:18080/dir/sheet.css")
    );
    assert_eq!(
        url::resolve(&b, "../up.css").as_deref(),
        Some("http://127.0.0.1:18080/up.css")
    );
    assert_eq!(
        url::resolve(&b, "?y=2").as_deref(),
        Some("http://127.0.0.1:18080/dir/page.html?y=2")
    );
    assert_eq!(
        url::resolve(&b, "s.css#frag").as_deref(),
        Some("http://127.0.0.1:18080/dir/s.css")
    );
    assert_eq!(
        url::resolve(&b, "").as_deref(),
        Some("http://127.0.0.1:18080/dir/page.html?x=1")
    );
    assert_eq!(
        url::resolve(&b, "/a/../b.css").as_deref(),
        Some("http://127.0.0.1:18080/b.css")
    );
    assert_eq!(url::resolve(&b, "a b.css"), None); // space in path
    assert_eq!(url::resolve(&b, "a\tb.css"), None); // control char
    println!("URL_RESOLVE=PASS");

    // ---- Stage 5: inheritance (computed values propagate down the tree) ----
    let doc = dom::build("<html><body><div><p>x<em>e</em></p></div><span>s</span>");
    let o = css::parse(
        "body { color: green } div > p { text-align: center } em { border-width: 3px }",
    );
    let ap = css::apply(&doc.dom, &o.rules);
    assert_eq!(ap.styled, 3); // body, p, em carry own decls
    assert_eq!(ap.inherited, 5); // div, p, em(x2), span receive values
    assert_eq!(ap.inherited_elements, 4);
    assert!(css::is_inherited("color") && !css::is_inherited("margin"));
    println!("CSS_INHERIT=PASS");

    0
}
