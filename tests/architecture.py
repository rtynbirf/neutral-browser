from pathlib import Path

root = Path(__file__).resolve().parents[1]
required = [
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
    "tools/serve_test_page.py",
    "tools/e2e.py",
    "docs/ARCHITECTURE.md",
    "docs/DEPENDENCY_POLICY.md",
]
missing = [p for p in required if not (root / p).exists()]
assert not missing, f"missing={missing}"

# All Rust sources concatenated (stage1 checked main.rs only; the tree grew).
src = ""
for p in sorted((root / "src").glob("*.rs")):
    src += p.read_text() + "\n"

# ---- Stage 1 invariants must survive the module split ----
for token in [
    "struct Capability",
    "struct Policy",
    "struct Broker",
    "fn evaluate",
    "fn request",
    "Decision::Deny",
    "Decision::Allow",
]:
    assert token in src, f"missing token: {token}"
assert "cap.decision" not in src, "decision must not be supplied by the requester"

# ---- Stage 2 structural invariants ----
net = (root / "src/network.rs").read_text()
others = ""
for p in sorted((root / "src").glob("*.rs")):
    if p.name != "network.rs":
        others += p.read_text()
assert "TcpStream::connect" in net, "network.rs must perform the connect"
assert "TcpStream::connect" not in others, "connect allowed only in network.rs"
assert "broker.request(" in net, "fetch must pass through the broker"

assert "decode_chunked" in (root / "src/http.rs").read_text()
assert "sha256" in (root / "src/sha256.rs").read_text()
main_txt = (root / "src/main.rs").read_text()
assert "--fetch" in main_txt and "--self-test" in main_txt

# ---- Stage 3 structural invariants ----
html_txt = (root / "src/html.rs").read_text()
dom_txt = (root / "src/dom.rs").read_text()

for token in ["fn tokenize", "struct TokControl", "enum Token"]:
    assert token in html_txt, f"html.rs missing token: {token}"
for token in ["struct Dom", "enum NodeData", "fn dump", "fn build", "fn counts"]:
    assert token in dom_txt, f"dom.rs missing token: {token}"

# html.rs and dom.rs are pure: no I/O of any kind (network or filesystem).
for name, txt in (("html.rs", html_txt), ("dom.rs", dom_txt)):
    assert "TcpStream" not in txt, f"{name} must not touch the network"
    assert "std::fs" not in txt, f"{name} must not touch the filesystem"
    assert "tokio" not in txt and "reqwest" not in txt, f"{name} must stay dependency-free"

# CLI surface of stage 3.
for flag in ["--dom-file", "--dom-output", "PARSE_ERRORS", "DOM_ELEMENTS"]:
    assert flag in main_txt, f"main.rs missing stage-3 token: {flag}"

# ---- Stage 4 structural invariants ----
css_txt = (root / "src/css.rs").read_text()

for token in ["fn tokenize", "struct Selector", "struct Compound", "fn parse", "fn apply", "fn matches"]:
    assert token in css_txt, f"css.rs missing token: {token}"

# css.rs is pure: no I/O of any kind (network or filesystem).
assert "TcpStream" not in css_txt, "css.rs must not touch the network"
assert "std::fs" not in css_txt, "css.rs must not touch the filesystem"
assert "tokio" not in css_txt and "reqwest" not in css_txt, "css.rs must stay dependency-free"

# CLI surface of stage 4.
for flag in ["--style", "--css-file", "--style-output", "STYLE_ELEMENTS_STYLED", "STYLE_RULES"]:
    assert flag in main_txt, f"main.rs missing stage-4 token: {flag}"

# Zero external runtime dependencies; lockfile present.
toml = (root / "Cargo.toml").read_text()
assert "[dependencies]" in toml
after = toml.split("[dependencies]", 1)[1].strip()
assert after == "", f"dependencies list must stay empty, found: {after!r}"

# Anti-facade statuses documented.
readme = (root / "README.md").read_text()
assert "NOT_IMPLEMENTED" in readme, "honest status table required in README"
# Stage 3 must be honestly documented as a subset.
assert "foster" in readme.lower(), "README must document the no-foster-parenting subset"
# Stage 4 must honestly document the at-rules subset.
assert "at-rules" in readme.lower(), "README must document the at-rules-are-skipped subset"

print("ARCHITECTURE_TEST=PASS")
print("POLICY_BROKER_MODEL=PASS")
print("STAGE2_STRUCTURE=PASS")
print("STAGE3_STRUCTURE=PASS")
print("STAGE4_STRUCTURE=PASS")
