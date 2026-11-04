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

## Stage 6: block layout -> LAYOUT (v0.7.0)

Stage 6 adds the LAYOUT link: a box tree with used widths, heights and
positions over the computed STYLE, per a CSS 2.1 ch.10 subset:

```
DOM (element tree)                STYLE (computed declarations)
        |                                   |
        +----------------+------------------+
                         v
        BOX TREE CONSTRUCTION (layout.rs, pure, iterative)
             display:none removes the element's own box + subtree (LAYOUT_HIDDEN)
             never-rendered / inline-by-default frozen subsets (no UA stylesheet:
             defaults are honest zeros, documented deviations)
                         v
        WIDTH RESOLUTION  (CSS 2.1 10.3.3 subset)   parse_len: px / % / auto
             content width from the parent's content box;
             auto margins center; over-constrained resolves in LTR
             (margin-right absorbs); width:auto zeroes auto margins;
             box-sizing: border-box honored
                         v
        HEIGHT RESOLUTION  (CSS 2.1 10.6.3 / 10.5 subset)
             px / % (only against a definite container height) / auto =
             sum of children's margin boxes (iterative stack)
                         v
        LAYOUT RESULT
             +--> counters: LAYOUT_BOXES / LAYOUT_HIDDEN / LAYOUT_ERRORS
             |    (unsupported values of known properties are counted and
             |     fall back to defaults; work never aborts)
             +--> canonical dump "LAYOUT v1" (frozen): per box in document
                  order - `B<idx> <path>`, rect / margin / padding / border /
                  content w h
             +--> LayoutOut (input of stage 7)
```

Purity and robustness invariants: `layout.rs` is a pure module (no I/O, no
panics; every walk is iterative - 10 000 nested divs are a unit test); the
box equations `w = bl+pl+content_w+pr+br` and `border_h = bt+pt+content_h+
pb+bb` hold for every box (unit-tested); percentages resolve against the
container WIDTH, including vertical paddings/borders (documented subset).

## Stage 7: display list + software paint -> PAINT (v0.8.0)

Stage 7 turns the box tree into pixels: a display list, then a raster:

```
LayoutOut + StyleApply
        |
        v
DISPLAY LIST (paint.rs, pure)          paint::display_list
     canvas color = CSS 2.1 14.2 subset propagation:
     valid background-color on html paints the whole canvas (html itself has
     no fill), else body's, else white; garbage = one counted error
     per box, document order: Fill{border-box, background-color}
     then 4 non-overlapping strips for solid longhand borders
     (other border styles = counted error, fall back to none);
     border color: border-X-color -> color -> black; alpha 0 = no command
        |
        v
RASTER (paint.rs, pure)                paint::raster
     fixed VIEWPORT_W x VIEWPORT_H RGBA canvas; later commands paint over
     earlier ones; rectangles clipped in i64 (dumps show ideal geometry);
     per-pixel integer over-blending with ceil(0.5) rounding
        |
        v
OUTPUTS
     +--> counters: PAINT_CANVAS / PAINT_BOXES / PAINT_CMDS / PAINT_ERRORS
     +--> canonical dump "PAINT v1" (frozen): `CANVAS <hex>` then
          `F x y w h <hex>` in paint order (#rrggbb at a=255, #rrggbbaa else)
     +--> binary PPM P6 (deterministic, uncompressed)
```

## Stage 8: PNG output (v0.9.0)

Stage 8 closes the output link with a real image format, still with zero
dependencies — the container and its checksums are written from scratch:

```
Canvas (RGBA, stage 7 raster)
        |
        v
SCANLINE FRAMING (png.rs, pure)        filter byte 0 (None) per row
        |
        v
ZLIB STREAM (png.rs)                   zlib_store: CMF=0x78 FLG=0x01,
     STORED deflate blocks only (RFC 1951 3.2.4): every 65535-byte slice
     becomes a byte-aligned block BFINAL?+BTYPE=00+LEN+NLEN(!LEN)+payload;
     big-endian Adler-32 (RFC 1950 8.2) trailer
        |
        v
PNG CONTAINER (png.rs)                 signature 89 50 4E 47 0D 0A 1A 0A
     IHDR (w,h, bit depth 8, color type 6 RGBA, compression 0, filter 0,
     interlace 0) + IDAT (the zlib stream) + IEND; every chunk:
     big-endian length + type + data + CRC-32 over type|data
     (ISO 3309 table-driven, reflected, poly 0xEDB88320)
        |
        v
encode(w, h, rgba) -> Option<Vec<u8>>  honest rejection: non-positive or
     overflowing geometry or a wrong buffer length never panics
```

Anti-circular verification: CRC-32 and Adler-32 are pinned by published
vectors (`"123456789"` -> 0xCBF43926, `"abc"` -> 0x024D0127, ...); the frozen
PNG of the e2e page was decoded by an INDEPENDENT inflater (Python stdlib
zlib) whose scanlines match the PPM ground truth of stage 7 — the writer is
checked against the OS inflater, not only against itself; a test-side
stored-block inflater additionally round-trips multi-block streams. The
output remains byte-for-byte deterministic. `png.rs` is a pure module (no
I/O, no panics, empty import graph); the file write happens in main.rs under
the same I/O error discipline as PPM (exit 5).

## Stage 10: var() + custom properties (v0.11.0)

CSS custom properties and `var()` substitution, honest by construction:

```
--name: raw value            (declaration, case-SENSITIVE name)
apply():                     (per element, document order)
  vars(env)   = parent computed custom props
  own_custom  = cascade winners named --*
  fixpoint:   resolve own props against env + already-resolved own props
              (chains resolve within the pass; props stuck referencing
              each other = reference cycle -> invalid, STYLE_VAR_CYCLES)
  vars(env)  += resolved own props        (children inherit RESOLVED values)
  normal decl: value contains var( -> substitute; unresolved without a
              usable fallback -> declaration dropped at computed-value
              time; an inherited property naturally falls back to the
              parent's value already present in `computed`
```

- Substitution grammar: `var(` only as a function call (preceding char is
  not an ident char — `foovar(`/`-var(` do not count); balanced-paren span;
  first TOP-LEVEL comma splits name/fallback; fallback may itself contain
  var() (resolved recursively).
- Determinism: `MAX_VAR_DEPTH = 32` — deeper nesting counts as unresolved,
  never a panic; the whole engine keeps the zero-panic discipline.
- Counters (main.rs report): `STYLE_VARS_DEFINED` (own custom props that
  computed OK), `STYLE_VAR_REFS` (var() calls encountered),
  `STYLE_VAR_FALLBACK_USED`, `STYLE_VAR_UNRESOLVED` (no value, no fallback),
  `STYLE_VAR_CYCLES` (props invalidated by cycles).
- Honest correction on the stress fixture: GOLOGRAMMA references var() 541
  times while defining 0 custom properties -> `STYLE_VAR_UNRESOLVED=541`;
  `STYLE_INHERITED` 2159 -> 1815 (raw var() text no longer leaks into
  inherited values). The PNG is byte-identical (var()-values were never
  paintable — but now the engine SAYS so instead of staying silent).
- Gate: STAGE10_STRUCTURE pins the substitution functions in css.rs (pure
  module), the counters in main.rs, `MAX_VAR_DEPTH=32` NAME+VALUE, and the
  conditional case-fold (custom names verbatim, other names lowercased).
- e2e T29: tests/fixtures/vars_v1.html — define/use via inheritance,
  fallback, unresolved drop, case mismatch (--Tok vs --tok), reference
  cycle, white-on-var-background contrast; counters and PNG sha256 frozen.
