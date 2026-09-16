#!/usr/bin/env python3
"""Architecture gate v2 for neutral-browser (stdlib only, no third-party deps).

History:
  v1 (stages 2-5): substring `assert token in src` checks; grew per stage;
      stage-6 block had a NameError (readme used before assignment) - gate was RED.
  v2 (this file): rewritten after external review. What changed and why:

  ACCEPTED from the review (adapted to the frozen zero-dependency policy):
    - all failures collected and reported at once, exit code 1 (v1 died on first)
    - is_file() instead of exists() (a directory named src/main.rs passed v1)
    - unexpected *.rs under src/ is a failure (a rogue module bypassed v1 checks)
    - required tokens are word-boundary regexes over COMMENT-STRIPPED source
      (v1 could be passed by writing the tokens inside comments; the stripper
      is string-aware and self-tested below)
    - forbidden I/O is a shared check_pure() over ALL pure modules
      (policy/broker/url/sha256/html/dom/css/http/layout), not copy-paste
    - layering: explicit allowed-imports whitelist per module (VERIFIED graph);
      any new `crate::x` edge is a failure until the architecture doc is changed
    - network bypass hardening: TcpStream/UdpSocket/std::net/ToSocketAddrs
      forbidden everywhere except network.rs
    - Decision encapsulation: `enum Decision` may exist only in policy.rs,
      `impl Decision` nowhere; broker must delegate to policy.evaluate
    - panic discipline: .unwrap()/.expect(/panic!/todo! forbidden in production
      parts (before #[cfg(test)]) of the zero-panic modules; css.rs keeps its
      3 documented tokenizer invariant-unwraps as a hard ceiling
    - DoS limits frozen by NAME and VALUE (MAX_INPUT_CHARS/MAX_BODY/MAX_HEAD/
      MAX_LINK_SHEETS/VIEWPORT_W/VIEWPORT_H)
    - Cargo gate: [dependencies] section must be EMPTY, [dev-dependencies]/
      [build-dependencies]/[features]/[patch]/[target...] forbidden (the review
      proposed allowing dev-deps - REJECTED: DEPENDENCY_POLICY.md freezes zero
      deps in every profile; dev-deps would break offline `cargo build --locked`)
    - Cargo.lock must contain exactly ONE package: neutral-browser (real
      lockfile-level proof of the zero-dependency invariant)
    - tools/ py_compile + e2e contract marker
    - cargo check --locked inside the gate (v1 could pass on non-compiling code)
    - explicit encoding="utf-8" everywhere

  REJECTED (with reason, so it stays decided):
    - anyhow/thiserror/mockall/criterion/proptest/tokio-behind-feature, GitHub
      Actions, cargo-deny, pre-commit -> third-party tooling violates the frozen
      zero-dependency policy and the offline machine-verification loop
    - tomllib parsing of Cargo.toml -> the text check is STRICTER (nothing at
      all may follow [dependencies] except empty lines)
    - custom clippy lints / cargo-public-api / AST parsing -> overkill for 11
      flat modules; comment-stripped regex closes the facade hole
    - RunReport/StageMetric/HttpClient-trait/PolicyEngine<P>/lib.rs/error.rs/
      async refactors -> touch VERIFIED stages without a stage goal; backlog
    - binary-size assert -> no contract requires it
"""

import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# ---------------------------------------------------------------- infrastructure
FAILURES = []  # (phase, message)


def fail(phase, msg):
    FAILURES.append((phase, msg))


def read(p):
    """Read repo file as UTF-8 (explicit encoding, review point)."""
    return (ROOT / p).read_text(encoding="utf-8")


_RAW_STR = re.compile(r'(?:br|r)(#*)"')
_CHAR_LIT = re.compile(r"'(\\u\{[^}]*\}|\\.|[^'\\])'")


def strip_rust_comments(src):
    """Remove // and /* */ (nestable) comments, string-aware.

    Handles: line comments, nested block comments, "strings" with escapes,
    raw strings r"..." / r#"..."# / br#"..."#, char literals vs lifetimes.
    Block comments are replaced by one space (tokens cannot fuse).
    """
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        prev_ok = i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")
        m = _RAW_STR.match(src, i) if prev_ok else None
        if m:
            end = src.find('"' + m.group(1), m.end())
            if end < 0:  # unterminated raw string: keep as-is
                out.append(src[i:])
                break
            out.append(src[i:end + 1 + len(m.group(1))])
            i = end + 1 + len(m.group(1))
        elif c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            i = n if j < 0 else j  # keep the newline itself
        elif c == "/" and i + 1 < n and src[i + 1] == "*":
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth += 1
                    i += 2
                elif src.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            out.append(" ")
        elif c == '"':
            out.append(c)
            i += 1
            while i < n:
                if src[i] == "\\":
                    i += 2
                elif src[i] == '"':
                    out.append('"')
                    i += 1
                    break
                else:
                    out.append(src[i])
                    i += 1
        elif c == "'":
            m2 = _CHAR_LIT.match(src, i)
            if m2:
                out.append(m2.group(0))
                i = m2.end()
            else:  # lifetime like Tokenizer<'a> - copy through
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


# stripper self-test: the gate must be able to trust its own instrument
_st = strip_rust_comments
assert "struct Capability" not in _st("// struct Capability\nlet x = 1;")
assert "struct Capability" in _st("/* note */ struct Capability;")
assert "http://x" in _st('let s = "http://x"; // tail')
assert "tail" not in _st('let s = "http://x"; // tail')
assert "/* x */" not in _st("a/* x */b") and _st("a/* x */b").strip() == "a b"
assert "struct Deep" not in _st("/* outer /* inner */ struct Deep */")
_lt = "Tokenizer<'a> { let c = 'x'; }"
assert _st(_lt) == _lt  # lifetimes and char literals survive untouched
assert "raw# inside" in _st('r#"raw# inside"#')
assert "escaped" in _st('let s = "quoted \\" escaped";')

# ---------------------------------------------------------------- P0: files
REQUIRED = [
    "Cargo.toml",
    "Cargo.lock",
    "src/main.rs",
    "src/policy.rs",
    "src/broker.rs",
    "src/url.rs",
    "src/http.rs",
    "src/network.rs",
    "src/sha256.rs",
    "src/html.rs",
    "src/dom.rs",
    "src/css.rs",
    "src/layout.rs",
    "src/paint.rs",
    "src/png.rs",
    "tools/serve_test_page.py",
    "tools/e2e.py",
    "docs/ARCHITECTURE.md",
    "docs/DEPENDENCY_POLICY.md",
    "README.md",
]

SRC_WHITELIST = {
    "main.rs", "policy.rs", "broker.rs", "url.rs", "http.rs", "network.rs",
    "sha256.rs", "html.rs", "dom.rs", "css.rs", "layout.rs", "paint.rs", "png.rs",
}


def check_files():
    missing = [p for p in REQUIRED if not (ROOT / p).is_file()]
    if missing:
        fail("FILES", f"missing files: {sorted(missing)}")
    # every .rs under src/ (any depth) must be a known module
    actual = [str(p.relative_to(ROOT / "src")) for p in (ROOT / "src").rglob("*.rs")]
    unexpected = [a for a in actual if a.rsplit("/", 1)[-1] not in SRC_WHITELIST]
    if unexpected:
        fail("FILES", f"unexpected src modules (add consciously): {sorted(unexpected)}")


# ---------------------------------------------------------------- P1: purity
PURE_MODULES = ["policy", "broker", "url", "sha256", "html", "dom", "css", "http", "layout", "paint", "png"]

FORBIDDEN_PURE = [
    "TcpStream", "UdpSocket", "std::net", "ToSocketAddrs", "lookup_host",
    "std::fs", "std::io", "std::process", "std::env", "std::thread",
    "std::time", "std::os", "File::", "unsafe", 'extern "C"',
    "tokio", "reqwest", "hyper", "openssl", "native_tls", "rustls",
    "libc", "getrandom", "rand",
]


def check_pure():
    for mod in PURE_MODULES:
        txt = strip_rust_comments(read(f"src/{mod}.rs"))
        for bad in FORBIDDEN_PURE:
            if bad in txt:
                fail("PURE_MODULES", f"{mod}.rs must not contain {bad!r} (pure module)")


# ---------------------------------------------------------------- P2: layering
# VERIFIED dependency graph (stage 6). Any new edge = conscious architecture change.
ALLOWED_IMPORTS = {
    "policy": set(),
    "broker": {"policy"},
    "url": set(),
    "sha256": set(),
    "html": set(),
    "dom": {"html"},
    "css": {"dom"},
    "layout": {"css", "dom"},
    "http": set(),
    "network": {"broker", "http", "policy", "url"},
    "paint": {"css", "layout", "dom", "sha256"},
    "png": set(),
}

_CRATE_REF = re.compile(r"\bcrate::([a-z_][a-z_0-9]*)")


def check_layering():
    for mod, allowed in ALLOWED_IMPORTS.items():
        txt = strip_rust_comments(read(f"src/{mod}.rs"))
        refs = set(_CRATE_REF.findall(txt))
        bad = refs - allowed - {mod}
        if bad:
            fail("LAYERING", f"{mod}..rs imports upward/sideways: {sorted(bad)} "
                             f"(allowed: {sorted(allowed)})")
    # network is the ONLY module allowed to know TcpStream::connect
    net = strip_rust_comments(read("src/network.rs"))
    for mod in SRC_WHITELIST - {"network.rs"}:
        txt = strip_rust_comments(read(f"src/{mod}"))
        for bad in ("TcpStream", "UdpSocket", "std::net", "ToSocketAddrs",
                    "connect_timeout", "lookup_host"):
            if bad in txt:
                fail("LAYERING", f"{mod} must not touch sockets: found {bad!r}")
    if "TcpStream::connect" not in net:
        fail("LAYERING", "network.rs must perform the connect")
    if "broker.request(" not in net:
        fail("LAYERING", "fetch must pass through the broker")


# ---------------------------------------------------------------- P3: stage tokens
def stage_checks():
    stripped = {m: strip_rust_comments(read(f"src/{m}.rs"))
                for m in ("policy", "broker", "url", "http", "network", "sha256",
                          "html", "dom", "css", "layout", "paint", "png", "main")}
    main_txt = read("src/main.rs")  # CLI flags live inside string literals too

    def need(phase, mod, patterns):
        for pat in patterns:
            if not re.search(pat, stripped[mod]):
                fail(phase, f"{mod}.rs missing required definition: {pat}")

    def need_main(phase, tokens):
        for tok in tokens:
            if tok not in main_txt:
                fail(phase, f"main.rs missing stage token: {tok}")

    # Stage 1: capability/policy/broker model
    need("STAGE1_STRUCTURE", "policy", [
        r"\bstruct\s+Capability\b", r"\bstruct\s+Policy\b",
        r"\benum\s+Decision\b", r"\bfn\s+evaluate\b",
    ])
    need("STAGE1_STRUCTURE", "broker", [
        r"\bstruct\s+Broker\b", r"\bfn\s+request\b", r"self\.policy\.evaluate",
    ])
    # Decision encapsulation: defined only in policy.rs, never impl'd, never
    # supplied by the requester (v1 kept), broker delegates.
    who = [m for m, t in stripped.items() if re.search(r"\benum\s+Decision\b", t)]
    if who != ["policy"]:
        fail("STAGE1_STRUCTURE", f"enum Decision must exist only in policy.rs, found: {who}")
    if re.search(r"\bimpl\s+Decision\b", "".join(stripped.values())):
        fail("STAGE1_STRUCTURE", "impl Decision is forbidden (policy owns semantics)")
    if "cap.decision" in "".join(stripped.values()):
        fail("STAGE1_STRUCTURE", "requester-supplied decisions are forbidden (cap.decision)")

    # Stage 2: HTTP/1.1 over broker-gated TCP
    need("STAGE2_STRUCTURE", "network", [r"\bpub\s+fn\s+fetch\b", r"\bMAX_HEAD\b"])
    need("STAGE2_STRUCTURE", "http", [r"\bfn\s+parse_head\b", r"\bfn\s+decode_chunked\b"])
    need("STAGE2_STRUCTURE", "sha256", [r"\bfn\s+sha256\b"])
    need_main("STAGE2_STRUCTURE", ["--fetch", "--self-test"])

    # Stage 3: HTML tokenizer -> DOM (WHATWG subset)
    need("STAGE3_STRUCTURE", "html", [
        r"\bfn\s+tokenize\b", r"\bstruct\s+TokControl\b", r"\benum\s+Token\b",
        r"\bMAX_INPUT_CHARS\b",
    ])
    need("STAGE3_STRUCTURE", "dom", [
        r"\bstruct\s+Dom\b", r"\benum\s+NodeData\b", r"\bfn\s+dump\b",
        r"\bfn\s+build\b", r"\bfn\s+counts\b",
    ])
    need_main("STAGE3_STRUCTURE", ["--dom-file", "--dom-output",
                                   "PARSE_ERRORS", "DOM_ELEMENTS"])

    # Stage 4: CSS tokenizer -> parser -> selectors -> cascade -> STYLE
    need("STAGE4_STRUCTURE", "css", [
        r"\bfn\s+tokenize\b", r"\bstruct\s+Selector\b", r"\bstruct\s+Compound\b",
        r"\bfn\s+parse\b", r"\bfn\s+apply\b", r"\bfn\s+matches\b",
    ])
    need_main("STAGE4_STRUCTURE", ["--style", "--css-file", "--style-output",
                                   "STYLE_ELEMENTS_STYLED", "STYLE_RULES"])

    # Stage 5: inheritance + <link rel=stylesheet> sheets (described in css.rs,
    # fetched by main.rs through the same gated network::fetch)
    need("STAGE5_STRUCTURE", "css", [
        r"\bfn\s+collect_sheets\b", r"\benum\s+SheetRef\b",
        r"\bstruct\s+SheetList\b", r"\bfn\s+is_inherited\b",
    ])
    need("STAGE5_STRUCTURE", "url", [
        r"\bpub\s+fn\s+resolve\b", r"\bfn\s+remove_dot_segments\b",
        r"\bpub\s+fn\s+parse\b",
    ])
    need_main("STAGE5_STRUCTURE", [
        "network::fetch", "MAX_LINK_SHEETS", "STYLE_LINK_SHEETS",
        "STYLE_LINK_SHEETS_FETCHED", "STYLE_LINK_SHEETS_FAILED",
        "LINK_SHEET_FAIL", "STYLE_INHERITED", "STYLE_INHERITED_ELEMENTS",
    ])

    # Stage 6: block layout (CSS 2.1 ch.10 subset), LAYOUT v1 dump
    need("STAGE6_STRUCTURE", "layout", [
        r"\bpub\s+fn\s+run\b", r"\bpub\s+fn\s+dump\b", r"\bpub\s+enum\s+Disp\b",
        r"\bpub\s+enum\s+Len\b", r"\bpub\s+fn\s+parse_len\b",
        r"\bstruct\s+LBox\b", r"\bstruct\s+LayoutOut\b",
        r"\bVIEWPORT_W\b", r"\bVIEWPORT_H\b",
    ])
    need_main("STAGE6_STRUCTURE", ["--layout", "--layout-output", "LAYOUT_BOXES",
                                   "LAYOUT_HIDDEN", "LAYOUT_ERRORS"])
    if not re.search(r"\bfn\s+layout_stage\b", main_txt):
        fail("STAGE6_STRUCTURE", "layout wrapper must live in main.rs")

    # Stage 7: display list + software paint (PAINT v1 dump, binary PPM P6)
    need("STAGE7_STRUCTURE", "paint", [
        r"\bpub\s+fn\s+display_list\b", r"\benum\s+Cmd\b",
        r"\bstruct\s+DisplayList\b", r"\bstruct\s+Canvas\b",
        r"\bpub\s+fn\s+raster\b", r"\bpub\s+fn\s+parse_color\b",
        r"\bpub\s+fn\s+to_ppm\b", r"\bpub\s+fn\s+dump\b",
        r"\bstruct\s+Rgba\b", r"\bfn\s+over\b",
    ])
    need_main("STAGE7_STRUCTURE", ["--paint", "--paint-output", "--paint-dump",
                                   "PAINT_CANVAS", "PAINT_BOXES", "PAINT_CMDS",
                                   "PAINT_ERRORS"])
    if not re.search(r"\bfn\s+paint_stage\b", main_txt):
        fail("STAGE7_STRUCTURE", "paint wrapper must live in main.rs")

    # Stage 8: PNG output (own CRC-32 / Adler-32 / zlib-stored / PNG writer)
    need("STAGE8_STRUCTURE", "png", [
        r"\bpub\s+fn\s+crc32\b", r"\bpub\s+fn\s+adler32\b",
        r"\bpub\s+fn\s+zlib_store\b", r"\bpub\s+fn\s+encode\b",
        r'b"IHDR"', r'b"IDAT"', r'b"IEND"',
    ])
    need_main("STAGE8_STRUCTURE", ["--png-output", "PAINT_PNG_SAVED",
                                   "PAINT_PNG_SAVE_ERROR"])
    # --png must participate in the paint chain (implies layout -> style):
    # both call sites of paint_stage must receive the png path argument
    n_calls = len(re.findall(r"paint_stage\s*\(", main_txt))
    n_wired = len(re.findall(r"flag\(args,\s*\"--png-output\"\)", main_txt))
    if n_calls < 2 or n_wired < 2:
        fail("STAGE8_STRUCTURE", f"--png-output must be wired at both paint_stage "
                                 f"call sites (calls={n_calls}, wired={n_wired})")


# ---------------------------------------------------------------- P4: panic discipline
_PANIC = re.compile(r"\.unwrap\(\)|\.expect\(|\bpanic!|\btodo!|\bunimplemented!")
ZERO_PANIC_MODULES = ["policy", "broker", "network", "dom", "html", "http",
                      "sha256", "url", "layout", "paint", "png"]
CSS_PANIC_CEILING = 3  # documented tokenizer invariant-unwraps (validated hex / bounds)


def check_panics():
    for mod in ZERO_PANIC_MODULES:
        txt = strip_rust_comments(read(f"src/{mod}.rs"))
        prod = txt.split("#[cfg(test)]", 1)[0]
        hits = _PANIC.findall(prod)
        if hits:
            fail("PANIC_DISCIPLINE", f"{mod}.rs production code must be panic-free, "
                                     f"found {len(hits)}: {sorted(set(hits))}")
    css_prod = strip_rust_comments(read("src/css.rs")).split("#[cfg(test)]", 1)[0]
    n = len(_PANIC.findall(css_prod))
    if n > CSS_PANIC_CEILING:
        fail("PANIC_DISCIPLINE", f"css.rs production panic sites {n} > ceiling {CSS_PANIC_CEILING}")
    # main.rs is exempt only inside the runtime self-test contract; flag raw
    # unwrap/expect outside it is not checked textually - self-test must fail
    # loudly by design.


# ---------------------------------------------------------------- P5: DoS limits
LIMITS = [
    ("src/html.rs", "MAX_INPUT_CHARS", 8 * 1024 * 1024),
    ("src/css.rs", "MAX_INPUT_CHARS", 8 * 1024 * 1024),
    ("src/http.rs", "MAX_BODY", 10 * 1024 * 1024),
    ("src/network.rs", "MAX_HEAD", 64 * 1024),
    ("src/main.rs", "MAX_LINK_SHEETS", 8),
    ("src/layout.rs", "VIEWPORT_W", 800),
    ("src/layout.rs", "VIEWPORT_H", 600),
]

_CONST_VAL = re.compile(r"const\s+(\w+)\s*:\s*\w+\s*=\s*([\d\s*]+?);")


def check_limits():
    for path, name, expected in LIMITS:
        txt = read(path)
        m = re.search(rf"const\s+{name}\s*:\s*\w+\s*=\s*([\d\s*]+?);", txt)
        if not m:
            fail("LIMITS", f"{path}: limit {name} disappeared")
            continue
        expr = m.group(1)
        if not re.fullmatch(r"[\d\s*]+", expr):
            fail("LIMITS", f"{path}:{name}: non-numeric expression {expr!r}")
            continue
        value = eval(expr, {"__builtins__": {}}, {})  # digits, spaces, '*' only
        if value != expected:
            fail("LIMITS", f"{path}:{name} = {value}, contract freezes {expected}")


# ---------------------------------------------------------------- P6: Cargo / zero deps
def check_cargo():
    toml = read("Cargo.toml")
    # [dependencies] section must be empty; forbidden sections must not exist.
    lines = toml.splitlines()
    try:
        di = lines.index("[dependencies]")
    except ValueError:
        fail("CARGO_ZERO_DEP", "[dependencies] section missing")
        return
    body = []
    for ln in lines[di + 1:]:
        if ln.strip().startswith("["):
            break
        body.append(ln.strip())
    if any(body):
        fail("CARGO_ZERO_DEP", f"[dependencies] must stay empty, found: {body}")
    for sec in ("[dev-dependencies]", "[build-dependencies]", "[features]",
                "[patch", "[target."):
        if sec in toml:
            fail("CARGO_ZERO_DEP", f"section {sec} is forbidden by DEPENDENCY_POLICY.md")
    if not re.search(r'(?m)^edition\s*=\s*"2024"', toml):
        fail("CARGO_ZERO_DEP", 'edition = "2024" expected')
    if not re.search(r"(?m)^rust-version\s*=", toml):
        fail("CARGO_ZERO_DEP", "rust-version must be pinned")
    # lockfile: exactly one package - the real zero-dep proof
    lock = read("Cargo.lock")
    n_pkgs = lock.count("[[package]]")
    if n_pkgs != 1:
        fail("CARGO_ZERO_DEP", f"Cargo.lock must contain exactly 1 package, found {n_pkgs}")
    if not re.search(r'(?m)^name = "neutral-browser"', lock):
        fail("CARGO_ZERO_DEP", "Cargo.lock package must be neutral-browser")


# ---------------------------------------------------------------- P7: README honesty
README_PHRASES = [
    "NOT_IMPLEMENTED", "VERIFIED", "foster", "at-rules", "наследование",
    "initial", "STYLE v2", "LAYOUT v1", "PAINT v1", "PPM", "рендеринг",
    "STAGE7_PAINT", "STAGE8_PNG", "PNG", "crc32", "adler32", "HTTPS",
]


def check_readme():
    readme = read("README.md")
    low = readme.lower()
    for ph in README_PHRASES:
        if ph.lower() not in low:
            fail("README_HONESTY", f"README must document: {ph}")


# ---------------------------------------------------------------- P8: tools
def check_tools():
    for tool in ("tools/e2e.py", "tools/serve_test_page.py"):
        r = subprocess.run([sys.executable, "-m", "py_compile", str(ROOT / tool)],
                           capture_output=True, text=True)
        if r.returncode != 0:
            fail("TOOLS_SYNTAX", f"{tool} does not compile: {r.stderr.strip()[:200]}")
    e2e = read("tools/e2e.py")
    if "E2E_TEST=PASS" not in e2e or "run_client" not in e2e:
        fail("TOOLS_SYNTAX", "tools/e2e.py lost its contract surface")
    if "def do_GET" not in read("tools/serve_test_page.py"):
        fail("TOOLS_SYNTAX", "tools/serve_test_page.py lost its HTTP surface")


# ---------------------------------------------------------------- P9: compile gate
def check_compile():
    env_path = None
    cargo = shutil.which("cargo")
    if not cargo:
        cand = Path.home() / ".cargo" / "bin" / "cargo"
        if cand.is_file():
            cargo = str(cand)
    if not cargo:
        fail("COMPILE", "cargo not found")
        return
    r = subprocess.run([cargo, "check", "--locked"], cwd=ROOT,
                       capture_output=True, text=True, timeout=600)
    if r.returncode != 0:
        tail = (r.stderr or r.stdout).strip().splitlines()[-12:]
        fail("COMPILE", "cargo check --locked failed:\n" + "\n".join(tail))


# ---------------------------------------------------------------- runner
_MARKERS = ["FILES", "PURE_MODULES", "LAYERING", "STAGE1_STRUCTURE", "STAGE2_STRUCTURE",
            "STAGE3_STRUCTURE", "STAGE4_STRUCTURE", "STAGE5_STRUCTURE",
            "STAGE6_STRUCTURE", "STAGE7_STRUCTURE", "STAGE8_STRUCTURE",
            "PANIC_DISCIPLINE", "LIMITS",
            "CARGO_ZERO_DEP", "README_HONESTY", "TOOLS_SYNTAX", "COMPILE"]

check_files()
check_pure()
check_layering()
stage_checks()
check_panics()
check_limits()
check_cargo()
check_readme()
check_tools()
check_compile()

for phase in _MARKERS:
    ok = not any(f[0] == phase for f in FAILURES)
    print(f"{phase}=PASS" if ok else f"{phase}=FAIL")

if FAILURES:
    print(f"ARCHITECTURE_TEST=FAIL ({len(FAILURES)} failure(s))")
    for i, (phase, msg) in enumerate(FAILURES, 1):
        print(f"  [{i}] {phase}: {msg}")
    sys.exit(1)

print("POLICY_BROKER_MODEL=PASS")
print("ARCHITECTURE_TEST=PASS")
sys.exit(0)
