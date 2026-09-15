# Neutral Browser — architecture

## Stage 1: policy and capability broker

The requester does not supply the authorization decision. A web-content request is evaluated by `Policy`, and `Broker` returns the resulting decision.

```text
WEB CONTENT
    |
    v
Capability Request
    |
    v
Broker
    |
    v
Policy
    |
    +---- DENY ----> OS resource blocked
    |
    `---- ALLOW ---> permitted capability
```

Current bootstrap policy:

- web content has no ambient OS authority;
- filesystem, camera, microphone and clipboard requests are denied;
- network connection is the only capability admitted by this stage;
- non-web subjects are denied;
- the requester cannot set its own decision.

This is a policy/broker prototype, not yet an OS sandbox.

## Stage 2: broker-gated fetch chain (v0.3.0)

The first real slice of the engine chain from the master prompt (`URL -> NETWORK -> RESPONSE`):
a user command fetches a page through the broker. Nothing runs implicitly.

```text
USER COMMAND (--fetch URL)
    |
    v
URL parse (subset: http only; host/path whitelisted against CRLF injection)
    |
    v
Capability derivation (engine-side, not requester-side)
    |
    v
Broker -> Policy -> DENY --> NETWORK=CLOSED (no socket is ever opened)
    |
    ALLOW
    v
TCP connect  (the only TcpStream::connect in the codebase, verified by tests)
    |
    v
HTTP/1.1 GET (Host, Connection: close; no HTTP/2, no TLS in this stage)
    |
    v
Response head parse -> body framing (chunked | Content-Length | read-to-close)
    |
    v
Machine report: SCHEME/HOST/PORT/PATH, BROKER_*, HTTP_STATUS, HEADERS,
BODY_BYTES, BODY_SHA256 (FIPS 180-4), FETCH=OK
```

Module split: `policy.rs` + `broker.rs` carry stage 1 semantics unchanged;
`url.rs`, `http.rs`, `network.rs` are new; `sha256.rs` provides zero-dependency
integrity output.

Honest limitations (anti-facade, master prompt §23): HTTPS/TLS NOT_IMPLEMENTED
(planned: vendored rustls per DEPENDENCY_POLICY.md); HTTP/2 and QUIC
NOT_IMPLEMENTED; redirects NOT_IMPLEMENTED; scope matching in Policy
NOT_IMPLEMENTED (scope is recorded in the capability but not yet checked);
DNS uses the OS resolver, user-configurable DNS NOT_IMPLEMENTED; obs-fold and
compressed transfer codings are rejected, NOT_IMPLEMENTED.

## Stage 3: HTML tokenizer -> DOM (v0.4.0)

The next link of the engine chain (`RESPONSE -> HTML PARSER -> DOM`): the
fetched body is tokenized and built into a DOM tree by a documented subset of
the WHATWG HTML Standard. Nothing runs implicitly: parsing happens on the
`--fetch ... --dom` body or on the user's own file via `--dom-file`.

```text
RESPONSE BODY (bytes, lossy UTF-8)
    |
    v
HTML TOKENIZER (src/html.rs, WHATWG §13.5 subset)
    Data / TagOpen / attribute family / comments / bogus comment
    DOCTYPE(name) / RAWTEXT / RCDATA / PLAINTEXT / character references
    |
    |  TokControl: the tree builder flips the tokenizer's raw modes
    v
TREE CONSTRUCTION (src/dom.rs, WHATWG §13.2.6 subset)
    initial -> before html -> before head -> in head -> after head
    -> in body (text, after body, after after body)
    implied html/head/body, void elements, p/li/dd/dt auto-close,
    </br> -> <br>, image -> img, first-wins attribute dedup,
    text merging, error recovery (never aborts)
    |
    v
DOM ARENA (NodeData: Document/Doctype/Element/Text/Comment)
    |
    +--> counts: NODES/ELEMENTS/TEXTS/COMMENTS/DEPTH/DOCTYPE
    +--> canonical dump (frozen format, iterative, byte-exact)
    +--> machine report: DOM_*, PARSE_ERRORS, PARSE_ERROR_KINDS
```

Both modules are pure (no network, no filesystem I/O — machine-checked by
`tests/architecture.py`). Dump and counts are iterative: 10,000-deep nesting
parses without stack overflow (unit test).

Documented subset deviations: no foster parenting / in-table modes
(`<table><tr>` keeps `tr` as a direct child of `table`); no adoption agency /
active formatting elements; SVG/MathML parsed as ordinary HTML elements; no
template insertion modes; DOCTYPE name only; 80 common named character
references with a required `;` (unknown names stay literal — spec behavior)
plus full numeric references with the normative windows-1252 remap and U+FFFD
rules. Everything above is listed in the README status table, not hidden.

## Stage 4: CSS parser -> STYLE (v0.5.0)

The next link of the engine chain (`DOM -> CSS -> STYLE`): the `<style>`
elements of the parsed DOM (and, explicitly requested, the user's own CSS
file) are tokenized, parsed into rules, matched against the tree and cascaded
into per-element computed declarations. Nothing runs implicitly: the stage
runs on `--fetch ... --style` or `--dom-file ... --style`; an external
stylesheet is never fetched.

```text
<style> ELEMENTS OF THE DOM  +  USER CSS FILE (--css-file, user principal)
    |
    v
CSS TOKENIZER (src/css.rs, CSS Syntax L3 subset)
    ident / function / at-keyword / hash / string / bad-string
    url / bad-url / number / percentage / dimension / delim
    punctuation; backslash escapes (hex <= 6 digits, U+FFFD rules)
    |
    v
PARSER (subset)
    qualified rules -> selectors + declaration block
    declarations: name (lowercased) : value-span [!important]
    error recovery: empty/garbage/nested/EOF -> counted, dropped, no abort
    at-rules: consumed and ignored, counted (STYLE_AT_RULES_SKIPPED)
    |
    v
SELECTORS (Selectors L3 subset)
    type / universal / .class / #id / [attr] / [attr="value"]
    descendant + child combinators, comma grouping
    specificity (a, b, c)
    |
    v
CASCADE (author origin)          MATCHING (right-to-left over the tree)
    winner per property =            class list split on whitespace
    max(important, inline,           exact attribute comparison
    specificity, source order)       iterative ancestor walks
    |
    v
STYLE RESULT (per element, document order)
    +--> counters: STYLE_SRC_ELEMENTS / RULES / DECLS / AT_RULES_SKIPPED /
    |              ERRORS / ELEMENTS_STYLED / PROPS
    +--> canonical dump "STYLE v1" (frozen format, alphabetical properties)
```

`src/css.rs` is pure (no network, no filesystem I/O — machine-checked by
`tests/architecture.py`); the walk, matching and dump are iterative — deep
trees cascade without stack overflow (unit test).

Documented subset deviations: at-rules are never applied (no @media
evaluation, no @import fetching); no inheritance, shorthand expansion or
var(); no pseudo-classes/pseudo-elements, no `+`/`~` combinators, no
attribute operators `~= ^= $= *= |=`, no namespaces (such selectors are
counted as parse errors and the rule is dropped); declaration values are kept
verbatim (comments stripped, ends trimmed, whitespace not collapsed);
external stylesheets are not fetched. Inline `style` attributes participate
in the cascade with the standard author-origin priority (inline important >
sheet important > inline normal > sheet normal). Everything above is listed
in the README status table, not hidden.



## Stage 5: inheritance + <link> sheets -> computed STYLE (v0.6.0)

Stage 5 deepens the STYLE link of the master chain with two capabilities,
both reusing verified components — no new Cargo dependencies:

```
URL -> BROKER -> TCP -> HTTP/1.1 -> BODY -> HTML -> DOM
                                                     |
             +---------------------------------------+
             |
      SHEET COLLECTION (document order)            css::collect_sheets (pure)
             <style> -> Inline(raw text)            rel must contain "stylesheet"
             <link rel=stylesheet> -> Link(href)    and not "alternate" (case-free)
             |
             v
      LINK RESOLUTION (main.rs, I/O boundary)      url::resolve(base, href)
             absolute / //host / /path / rel+dot-segments / ?query / empty
             fragment dropped; non-graphic ASCII -> counted BAD_URL
             |
             v
      network::fetch (SAME broker-gated chain)     capability derived per sheet URL
             dedupe by resolved URL, MAX_LINK_SHEETS = 8
             failure kinds: NO_BASE_URL / NO_HREF / BAD_URL / HTTP_<status> /
             UNSUPPORTED_SCHEME / DNS / CONNECT / IO / LIMIT
             a failed sheet never aborts the cascade — counted + one machine line
             |
             v
      CASCADE (per sheet parse, document order)    source order spans all sheets
             |
             v
      INHERITANCE (css.rs, pure, iterative)        is_inherited(): frozen 28-prop subset
             child gets the parent's COMPUTED value; own cascade winner overrides;
             non-inherited properties never propagate; NO initial values
             |
             v
      STYLE RESULT
             +--> counters: STYLE_SRC_ELEMENTS / STYLE_LINK_SHEETS(_FETCHED/_FAILED)
             |              STYLE_LINK_DUPES / RULES / DECLS / AT_RULES_SKIPPED /
             |              ERRORS / ELEMENTS_STYLED / PROPS / STYLE_INHERITED(_ELEMENTS)
             +--> canonical dump "STYLE v2" (frozen): own `name: value` lines then
                  `~name: value` lines for received values, both alphabetical
```

Security/purity invariants held: `css.rs` still performs zero I/O — it only
DESCRIBES link sheets (`SheetRef::Link { href }`); the actual fetch lives in
`main.rs` and goes through the same broker-gated `network::fetch` with the
capability derived by the engine from the sheet's own URL. Link fetching is
possible only with `--fetch` (a base URL exists); `--dom-file` reports
`NO_BASE_URL` per link. The per-page cap (8 unique sheets) and the URL
dedupe bound engine-triggered network I/O.

Documented subset deviations (honest, machine-visible): `media`/`disabled`
attributes are ignored (every collected sheet applies); `@import` inside a
fetched sheet is NOT followed (fetch depth = 1); `inherit`/`initial`/
`unset`/`revert` keywords stay verbatim values; there are no initial values
— an inherited property without an ancestor value stays absent (honest
absence, not an invented defaults table); a colon inside a relative href
stays in the path (no scheme sniffing); the `User-Agent` now reports the
current version. All of the above is listed in the README status table.
