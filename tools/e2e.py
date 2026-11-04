#!/usr/bin/env python3
"""End-to-end machine verification for neutral-browser (stages 2-9).

Starts a local HTTP test server, drives the REAL compiled binary and asserts
its machine output line by line. Verdict per case (E2E_Tn_*=PASS/FAIL) and a
final E2E_TEST=PASS / E2E_TEST=FAIL.

Usage: python3 tools/e2e.py [path-to-binary]
Build first: cargo build --locked
"""
import hashlib
import socket
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "debug" / "neutral-browser"

# Stale-binary guard: e2e MUST refuse to run against a binary that is not the
# expected stage. A green suite against a stale binary is a machine lie
# (happened in real life: target/debug stayed at 0.9.0 while the tree was
# already 0.10.0 — every frozen hash "matched" the wrong engine).
EXPECTED_VERSION = "0.11.0"

PAGE = (
    b"<!doctype html><html><head><title>neutral-browser stage2 e2e</title></head>"
    b"<body><h1>STAGE2 E2E OK</h1><p>broker-gated fetch</p></body></html>"
)
PAGE_SHA = hashlib.sha256(PAGE).hexdigest()


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def wait_ready(port: int, deadline: float = 10.0) -> bool:
    end = time.time() + deadline
    while time.time() < end:
        s = socket.socket()
        s.settimeout(0.5)
        try:
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return True
        finally:
            s.close()
        time.sleep(0.1)
    return False


def run_client(args):
    p = subprocess.run([str(BIN)] + args, capture_output=True, text=True, timeout=30)
    out = {}
    for line in p.stdout.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return p.returncode, out, p.stdout


def main() -> int:
    if not BIN.exists():
        print(f"E2E_TEST=FAIL binary-not-found: {BIN}")
        return 1

    # refuse stale binaries BEFORE any case runs
    ver = subprocess.run([str(BIN), "--version"], capture_output=True, text=True, timeout=10)
    ver_line = ver.stdout.splitlines()[0].strip() if ver.stdout else ""
    if ver_line != f"neutral-browser {EXPECTED_VERSION}":
        print(f"E2E_TEST=FAIL stale-binary: got '{ver_line}', expected "
              f"'neutral-browser {EXPECTED_VERSION}' — rebuild: cargo build --locked")
        return 1
    print(f"E2E_BIN_VERSION=PASS ({ver_line})")

    port = free_port()
    server = subprocess.Popen(
        [sys.executable, str(ROOT / "tools" / "serve_test_page.py"), str(port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    results = []

    def check(name: str, cond, note: str = "") -> None:
        results.append((name, bool(cond), note))

    try:
        if not wait_ready(port):
            print("E2E_TEST=FAIL server-not-ready")
            return 1
        base = f"http://127.0.0.1:{port}"

        # T1: plain GET, Content-Length framing
        rc, o, raw = run_client(["--fetch", f"{base}/index.html"])
        check("E2E_T1_BROKER_ALLOW", o.get("BROKER_DECISION") == "ALLOW", raw)
        check("E2E_T1_CONNECT", o.get("CONNECT") == "OK", raw)
        check("E2E_T1_STATUS", o.get("HTTP_STATUS") == "200", raw)
        check("E2E_T1_SHA", o.get("BODY_SHA256") == PAGE_SHA, raw)
        check("E2E_T1_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)

        # T2: chunked framing decodes to the same bytes
        rc, o, raw = run_client(["--fetch", f"{base}/chunked"])
        check("E2E_T2_STATUS", o.get("HTTP_STATUS") == "200", raw)
        check("E2E_T2_SHA", o.get("BODY_SHA256") == PAGE_SHA, raw)
        check("E2E_T2_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)

        # T3: 404 is a protocol-level success for the fetch tool
        rc, o, raw = run_client(["--fetch", f"{base}/missing"])
        check("E2E_T3_STATUS", o.get("HTTP_STATUS") == "404", raw)
        check("E2E_T3_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)

        # T4: broker deny via diagnostic subject -> no socket at all
        rc, o, raw = run_client(
            ["--fetch", f"{base}/index.html", "--subject", "untrusted-process"]
        )
        check("E2E_T4_DENIED", o.get("BROKER_DECISION") == "DENY", raw)
        check("E2E_T4_CLOSED", o.get("NETWORK") == "CLOSED", raw)
        check("E2E_T4_NO_SOCKET", "CONNECT=OK" not in raw, raw)
        check("E2E_T4_EXIT4", rc == 4, raw)

        # T5: unsupported scheme never connects
        rc, o, raw = run_client(["--fetch", "file:///etc/passwd"])
        check("E2E_T5_UNSUPPORTED", o.get("ERROR_KIND") == "UNSUPPORTED_SCHEME", raw)
        check("E2E_T5_NO_SOCKET", "CONNECT=OK" not in raw, raw)
        check("E2E_T5_EXIT3", rc == 3, raw)

        # T6: connection refused is reported honestly
        dead = free_port()
        rc, o, raw = run_client(
            ["--fetch", f"http://127.0.0.1:{dead}/x", "--timeout-ms", "2000"]
        )
        check(
            "E2E_T6_ERROR",
            o.get("FETCH") == "ERROR" and o.get("ERROR_KIND") in ("CONNECT", "DNS"),
            raw,
        )
        check("E2E_T6_EXIT5", rc == 5, raw)

        # ---- Stage 3: HTML tokenizer -> DOM ----

        # T7: fetch + DOM of the known page (implied structure, no errors)
        rc, o, raw = run_client(["--fetch", f"{base}/index.html", "--dom"])
        check("E2E_T7_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
        check("E2E_T7_DOM_OK", o.get("DOM") == "OK", raw)
        check("E2E_T7_DOCTYPE", o.get("DOM_DOCTYPE") == "html", raw)
        check("E2E_T7_ELEMENTS", o.get("DOM_ELEMENTS") == "6", raw)  # html head title body h1 p
        check("E2E_T7_TEXTS", o.get("DOM_TEXTS") == "3", raw)
        check("E2E_T7_COMMENTS", o.get("DOM_COMMENTS") == "0", raw)
        check("E2E_T7_NODES", o.get("DOM_NODES") == "11", raw)
        check("E2E_T7_DEPTH", o.get("DOM_DEPTH") == "4", raw)
        check("E2E_T7_CLEAN", o.get("PARSE_ERRORS") == "0", raw)

        # T8: chunked framing -> same bytes -> same DOM (framing->DOM chain)
        rc, o, raw = run_client(["--fetch", f"{base}/chunked", "--dom"])
        check("E2E_T8_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
        check("E2E_T8_DOM_OK", o.get("DOM") == "OK", raw)
        check("E2E_T8_SHA", o.get("BODY_SHA256") == PAGE_SHA, raw)
        check("E2E_T8_ELEMENTS", o.get("DOM_ELEMENTS") == "6", raw)

        # T9: --dom-file on a tricky document: exact tree dump equality
        import tempfile

        TRICKY = (
            '<!doctype html><html><body><!--c--><div CLASS="a&amp;b" hidden>'
            "1 &lt; 2<p>x<ul><li>i<li>j</ul></div>"
            '<script>var s="</p>";if(a<b)X()</script>'
            "<title>t</title></body></html>"
        )
        EXPECTED_DUMP = r"""#document
  <!DOCTYPE html>
  <html>
    <head>
    <body>
      <!--c-->
      <div class="a&amp;b" hidden="">
        "1 < 2"
        <p>
          "x"
        <ul>
          <li>
            "i"
          <li>
            "j"
      <script>
        "var s=\"</p>\";if(a<b)X()"
      <title>
        "t"
"""
        with tempfile.TemporaryDirectory() as td:
            src_html = Path(td) / "tricky.html"
            out_dump = Path(td) / "dump.txt"
            src_html.write_text(TRICKY, encoding="utf-8")
            rc, o, raw = run_client(
                [
                    "--dom-file",
                    str(src_html),
                    "--dom-output",
                    str(out_dump),
                ]
            )
            check("E2E_T9_DOM_OK", o.get("DOM") == "OK" and rc == 0, raw)
            check("E2E_T9_DOCTYPE", o.get("DOM_DOCTYPE") == "html", raw)
            check("E2E_T9_ELEMENTS", o.get("DOM_ELEMENTS") == "10", raw)  # html head body div p ul li li script title
            check("E2E_T9_TEXTS", o.get("DOM_TEXTS") == "6", raw)
            check("E2E_T9_COMMENTS", o.get("DOM_COMMENTS") == "1", raw)
            check("E2E_T9_NODES", o.get("DOM_NODES") == "19", raw)
            check("E2E_T9_DEPTH", o.get("DOM_DEPTH") == "6", raw)
            check("E2E_T9_CLEAN", o.get("PARSE_ERRORS") == "0", raw)
            check(
                "E2E_T9_DUMP_EXACT",
                out_dump.exists() and out_dump.read_text(encoding="utf-8") == EXPECTED_DUMP,
                raw,
            )

        # T10: missing file -> honest IO error, exit 5
        rc, o, raw = run_client(["--dom-file", "/nonexistent/path/x.html"])
        check("E2E_T10_DOM_ERROR", o.get("DOM") == "ERROR", raw)
        check("E2E_T10_KIND", o.get("ERROR_KIND") == "IO", raw)
        check("E2E_T10_EXIT5", rc == 5, raw)

        # ---- Stage 4: CSS -> STYLE ----

        EXPECTED_STYLE_DUMP = """STYLE v2
E5 html>body>p
  color: red
  font-size: 12px
  margin: 4px
E6 html>body>p
  border: 1px
  color: blue
  font-size: 20px
  padding: 2px
E8 html>body>div>p
  color: red
  font-size: 12px
  margin: 0
E9 html>body>span
  margin: 0
"""

        # T11: fetch -> DOM -> <style> CSS -> cascade -> STYLE (exact dump)
        import tempfile

        with tempfile.TemporaryDirectory() as td:
            out_dump = Path(td) / "style.txt"
            rc, o, raw = run_client(
                ["--fetch", f"{base}/styled", "--dom", "--style",
                 "--style-output", str(out_dump)]
            )
            check("E2E_T11_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
            check("E2E_T11_SRC", o.get("STYLE_SRC_ELEMENTS") == "1", raw)
            check("E2E_T11_RULES", o.get("STYLE_RULES") == "6", raw)
            check("E2E_T11_DECLS", o.get("STYLE_DECLS") == "8", raw)
            check("E2E_T11_AT_SKIP", o.get("STYLE_AT_RULES_SKIPPED") == "0", raw)
            check("E2E_T11_ERRORS", o.get("STYLE_ERRORS") == "0", raw)
            check("E2E_T11_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "4", raw)
            check("E2E_T11_PROPS", o.get("STYLE_PROPS") == "11", raw)
            check("E2E_T11_STYLE_OK", o.get("STYLE") == "OK", raw)
            check(
                "E2E_T11_DUMP_EXACT",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8") == EXPECTED_STYLE_DUMP,
                raw,
            )

        # T11b: page without CSS -> honest zero counters, still OK
        rc, o, raw = run_client(["--fetch", f"{base}/index.html", "--dom", "--style"])
        check("E2E_T11B_SRC", o.get("STYLE_SRC_ELEMENTS") == "0", raw)
        check("E2E_T11B_RULES", o.get("STYLE_RULES") == "0", raw)
        check("E2E_T11B_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "0", raw)
        check("E2E_T11B_PROPS", o.get("STYLE_PROPS") == "0", raw)
        check("E2E_T11B_OK", o.get("STYLE") == "OK" and rc == 0, raw)

        # T12: chunked framing -> same bytes -> same cascade (framing->STYLE chain)
        rc, o, raw = run_client(["--fetch", f"{base}/styled-chunked", "--style"])
        check("E2E_T12_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
        check("E2E_T12_SRC", o.get("STYLE_SRC_ELEMENTS") == "1", raw)
        check("E2E_T12_RULES", o.get("STYLE_RULES") == "6", raw)
        check("E2E_T12_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "4", raw)
        check("E2E_T12_PROPS", o.get("STYLE_PROPS") == "11", raw)
        check("E2E_T12_STYLE_OK", o.get("STYLE") == "OK", raw)

        # T13: --dom-file + --css-file (user principal) -> exact dump;
        # at-rules are skipped and counted
        CSS_SRC = (
            "/* c */ .a { color: red }\n"
            "div span { color: green }\n"
            "@media screen { p { color: blue } }\n"
            "p { color: purple }\n"
        )
        USER_HTML = (
            '<!doctype html><html><body><p class="a">hi'
            "<div><span>go</span></div></body></html>"
        )
        EXPECTED_USER_DUMP = """STYLE v2
E3 html>body>p
  color: red
E5 html>body>div>span
  color: green
"""
        with tempfile.TemporaryDirectory() as td:
            src_html = Path(td) / "user.html"
            src_css = Path(td) / "user.css"
            out_dump = Path(td) / "user_style.txt"
            src_html.write_text(USER_HTML, encoding="utf-8")
            src_css.write_text(CSS_SRC, encoding="utf-8")
            rc, o, raw = run_client(
                ["--dom-file", str(src_html), "--style", "--css-file", str(src_css),
                 "--style-output", str(out_dump)]
            )
            check("E2E_T13_STYLE_OK", o.get("STYLE") == "OK" and rc == 0, raw)
            check("E2E_T13_SRC", o.get("STYLE_SRC_ELEMENTS") == "0", raw)
            check("E2E_T13_RULES", o.get("STYLE_RULES") == "3", raw)
            check("E2E_T13_DECLS", o.get("STYLE_DECLS") == "3", raw)
            check("E2E_T13_AT_SKIP", o.get("STYLE_AT_RULES_SKIPPED") == "1", raw)
            check("E2E_T13_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "2", raw)
            check("E2E_T13_PROPS", o.get("STYLE_PROPS") == "2", raw)
            check(
                "E2E_T13_DUMP_EXACT",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8") == EXPECTED_USER_DUMP,
                raw,
            )

            # T13b: missing --css-file -> honest IO error, exit 5
            rc, o, raw = run_client(
                ["--dom-file", str(src_html), "--style", "--css-file",
                 str(Path(td) / "missing.css")]
            )
            check("E2E_T13B_STYLE_ERROR", o.get("STYLE") == "ERROR", raw)
            check("E2E_T13B_KIND", o.get("ERROR_KIND") == "IO", raw)
            check("E2E_T13B_EXIT5", rc == 5, raw)

        # ---- Stage 5: inheritance + <link> sheets ----

        EXPECTED_INHERIT_DUMP = """STYLE v2
E4 html>body
  color: green
  font-family: monospace
E5 html>body>div
  ~color: green
  ~font-family: monospace
E6 html>body>div>p
  text-align: center
  ~color: green
  ~font-family: monospace
E7 html>body>div>p>em
  border-width: 3px
  ~color: green
  ~font-family: monospace
  ~text-align: center
E8 html>body>span
  ~color: green
  ~font-family: monospace
"""

        # T14: fetch -> DOM -> cascade -> inheritance -> STYLE (exact dump);
        # head/body/div inherit color+font-family; em also inherits text-align
        with tempfile.TemporaryDirectory() as td:
            out_dump = Path(td) / "inherit.txt"
            rc, o, raw = run_client(
                ["--fetch", f"{base}/inherit", "--dom", "--style",
                 "--style-output", str(out_dump)]
            )
            check("E2E_T14_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
            check("E2E_T14_SRC", o.get("STYLE_SRC_ELEMENTS") == "1", raw)
            check("E2E_T14_LINK", o.get("STYLE_LINK_SHEETS") == "0", raw)
            check("E2E_T14_RULES", o.get("STYLE_RULES") == "3", raw)
            check("E2E_T14_DECLS", o.get("STYLE_DECLS") == "4", raw)
            check("E2E_T14_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "3", raw)
            check("E2E_T14_PROPS", o.get("STYLE_PROPS") == "4", raw)
            check("E2E_T14_INHERITED", o.get("STYLE_INHERITED") == "9", raw)
            check("E2E_T14_INH_EL", o.get("STYLE_INHERITED_ELEMENTS") == "4", raw)
            check("E2E_T14_STYLE_OK", o.get("STYLE") == "OK", raw)
            check(
                "E2E_T14_DUMP_EXACT",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8") == EXPECTED_INHERIT_DUMP,
                raw,
            )

        # T15: <link rel=stylesheet> fetched through the broker-gated chain,
        # parsed and cascaded together with the inline <style> (exact dump)
        EXPECTED_LINK_DUMP = """STYLE v2
E6 html>body>p
  color: green
  margin: 9px
  padding: 1px
"""
        with tempfile.TemporaryDirectory() as td:
            out_dump = Path(td) / "link.txt"
            rc, o, raw = run_client(
                ["--fetch", f"{base}/styled-link", "--style",
                 "--style-output", str(out_dump)]
            )
            check("E2E_T15_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
            check("E2E_T15_SRC", o.get("STYLE_SRC_ELEMENTS") == "1", raw)
            check("E2E_T15_LINK", o.get("STYLE_LINK_SHEETS") == "1", raw)
            check("E2E_T15_FETCHED", o.get("STYLE_LINK_SHEETS_FETCHED") == "1", raw)
            check("E2E_T15_FAILED", o.get("STYLE_LINK_SHEETS_FAILED") == "0", raw)
            check(
                "E2E_T15_URL",
                f"LINK_SHEET_FETCHED={base}/sheet.css" in raw.splitlines(),
                raw,
            )
            check("E2E_T15_RULES", o.get("STYLE_RULES") == "3", raw)
            check("E2E_T15_DECLS", o.get("STYLE_DECLS") == "4", raw)
            check("E2E_T15_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "1", raw)
            check("E2E_T15_PROPS", o.get("STYLE_PROPS") == "3", raw)
            check(
                "E2E_T15_DUMP_EXACT",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8") == EXPECTED_LINK_DUMP,
                raw,
            )

            # T15b: chunked-framed stylesheet -> identical cascade
            rc, o2, raw2 = run_client(
                ["--fetch", f"{base}/styled-link-chunked", "--style",
                 "--style-output", str(Path(td) / "link2.txt")]
            )
            check("E2E_T15B_FETCH_OK", o2.get("FETCH") == "OK" and rc == 0, raw2)
            check("E2E_T15B_FETCHED", o2.get("STYLE_LINK_SHEETS_FETCHED") == "1", raw2)
            check(
                "E2E_T15B_SAME_DUMP",
                (Path(td) / "link2.txt").read_text(encoding="utf-8")
                == EXPECTED_LINK_DUMP,
                raw2,
            )

        # T16: document order across sheets — the <link> sheet is later in
        # the document, so its equal-specificity rule wins
        with tempfile.TemporaryDirectory() as td:
            out_dump = Path(td) / "order.txt"
            rc, o, raw = run_client(
                ["--fetch", f"{base}/link-order", "--style",
                 "--style-output", str(out_dump)]
            )
            check("E2E_T16_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
            check("E2E_T16_RULES", o.get("STYLE_RULES") == "2", raw)
            check(
                "E2E_T16_DUMP",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8")
                == "STYLE v2\nE6 html>body>b\n  color: blue\n",
                raw,
            )

        # T17: broken sheets never abort: 404 / bad URL / unsupported scheme,
        # each reported with its own machine kind; cascade still runs
        rc, o, raw = run_client(["--fetch", f"{base}/link-broken", "--style"])
        check("E2E_T17_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
        check("E2E_T17_LINK", o.get("STYLE_LINK_SHEETS") == "3", raw)
        check("E2E_T17_FETCHED", o.get("STYLE_LINK_SHEETS_FETCHED") == "0", raw)
        check("E2E_T17_FAILED", o.get("STYLE_LINK_SHEETS_FAILED") == "3", raw)
        check("E2E_T17_NO_FETCH", raw.count("LINK_SHEET_FETCHED=") == 0, raw)
        check("E2E_T17_KIND_404", "LINK_SHEET_KIND=HTTP_404" in raw, raw)
        check("E2E_T17_KIND_BADURL", "LINK_SHEET_KIND=BAD_URL" in raw, raw)
        check(
            "E2E_T17_KIND_SCHEME",
            "LINK_SHEET_KIND=UNSUPPORTED_SCHEME" in raw,
            raw,
        )
        check("E2E_T17_RULES", o.get("STYLE_RULES") == "0", raw)
        check("E2E_T17_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "0", raw)
        check("E2E_T17_STYLE_OK", o.get("STYLE") == "OK", raw)

        # T18: relative/parent/absolute hrefs resolve against the page URL
        # (dot-segment removal included); rel matching is case-insensitive
        with tempfile.TemporaryDirectory() as td:
            out_dump = Path(td) / "rel.txt"
            rc, o, raw = run_client(
                ["--fetch", f"{base}/sub/rel.html", "--style",
                 "--style-output", str(out_dump)]
            )
            fetched_urls = [
                l.split("=", 1)[1]
                for l in raw.splitlines()
                if l.startswith("LINK_SHEET_FETCHED=")
            ]
            check("E2E_T18_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
            check("E2E_T18_FETCHED", o.get("STYLE_LINK_SHEETS_FETCHED") == "3", raw)
            check(
                "E2E_T18_URLS",
                fetched_urls
                == [
                    f"{base}/sub/sheet.css",
                    f"{base}/root.css",
                    f"{base}/abs.css",
                ],
                raw,
            )
            check("E2E_T18_RULES", o.get("STYLE_RULES") == "3", raw)
            check(
                "E2E_T18_DUMP",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8")
                == "STYLE v2\nE7 html>body>p\n  color: teal\n  letter-spacing: 1px\n  text-indent: 4px\n",
                raw,
            )

        # T19: the link cap is enforced honestly (MAX_LINK_SHEETS = 8):
        # 10 distinct sheets -> 8 fetched, 2 reported as LIMIT failures
        rc, o, raw = run_client(["--fetch", f"{base}/link-cap", "--style"])
        check("E2E_T19_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
        check("E2E_T19_LINK", o.get("STYLE_LINK_SHEETS") == "10", raw)
        check("E2E_T19_FETCHED", o.get("STYLE_LINK_SHEETS_FETCHED") == "8", raw)
        check("E2E_T19_FAILED", o.get("STYLE_LINK_SHEETS_FAILED") == "2", raw)
        check("E2E_T19_KINDS", raw.count("LINK_SHEET_KIND=LIMIT") == 2, raw)
        check("E2E_T19_RULES", o.get("STYLE_RULES") == "8", raw)
        check("E2E_T19_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "1", raw)
        check("E2E_T19_STYLE_OK", o.get("STYLE") == "OK", raw)

        # ---- Stage 6: block layout ----

        # Frozen dump of /layout (verified against the real binary, then
        # frozen): centering, border-box, display:none, % width, indefinite
        # % height -> auto. Ordinals include title/style (head subtree).
        # Stage 9 (v0.10.0) re-froze it deliberately: dump is LAYOUT v2 —
        # anonymous ::lines boxes carry LN/WORD records; the default strut is
        # 16px font x 1.5 line-height = 24px (the HUMAN_FIRST floor baked in),
        # so html/body grew 40 -> 64 and the formerly empty B8 now holds one
        # 24px line of its <p> text.
        EXPECTED_LAYOUT_DUMP = """LAYOUT v2
B0 html
  rect 0 0 800 64
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 800 64
B4 html>body
  rect 0 0 800 64
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 800 64
B5 html>body>div
  rect 200 0 400 64
  margin 0 200 0 200
  padding 0 0 0 0
  border 0 0 0 0
  content 400 64
B6 html>body>div>div
  rect 200 0 100 40
  margin 0 300 0 0
  padding 5 5 5 5
  border 2 2 2 2
  content 86 26
B# html>body>div>div::lines
  rect 207 7 86 24
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 86 24
  LN 0 24 1 10
  WORD 0 6 a
B8 html>body>div>div
  rect 200 40 200 24
  margin 0 200 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 200 24
B# html>body>div>div::lines
  rect 200 40 200 24
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 200 24
  LN 0 24 1 10
  WORD 0 8 p
"""

# Frozen PAINT v2 dump and PPM sha256 for /painted (verified against the
# real binary output; see paint.rs for the format contract):
# boxes: html propagated canvas, body transparent, #red fill, #blue fill
# (50% blue over red), #frame white fill + 4 solid border strips
# (content-box: border box 63x47 = 1+60+2 x 3+40+4).
# Stage 9 (v0.10.0) re-froze it: T commands paint glyphs from the embedded
# bitmap font and carry honest contrast measurements (WCAG ratio x100 and
# APCA Lc x100 against the effective background under the glyph).
        EXPECTED_PAINT_DUMP = """PAINT v2
CANVAS #e0e0e0
F 0 0 200 100 #ff0000
T 0 15 #000000 #ff0000 525 3995 r
F 0 24 100 50 #0000ff80
T 0 39 #000000 #7f0080 221 1421 b
F 0 100 63 47 #ffffff
F 0 100 63 3 #008000
F 0 143 63 4 #0000ff
F 0 103 1 40 #ffff00
F 61 103 2 40 #00ff00
T 1 118 #000000 #ffffff 2100 10604 f
"""
        EXPECTED_PPM_SHA256 = "7f12ecea89fc22768a39e6969255802abae1a86819fc6ac037f1050bebe8a0fd"
        # Frozen PNG sha256 for /painted (stage 8, verified against the real
        # binary output AND an independent decoder: Python stdlib zlib inflates
        # our stored-block IDAT and the scanlines match the PPM ground truth,
        # see scripts/verify_png_reference.py):
        EXPECTED_PNG_SHA256 = "71e1c188829c701a981d49083d54beb23e7220f8ff206ede1e515cf294e9d6bf"

        # T20: fetch -> DOM -> cascade -> STYLE -> LAYOUT (exact dump):
        # --layout implies --style (the STYLE block always precedes LAYOUT)
        with tempfile.TemporaryDirectory() as td:
            out_dump = Path(td) / "layout.txt"
            rc, o, raw = run_client(
                ["--fetch", f"{base}/layout", "--layout",
                 "--layout-output", str(out_dump)]
            )
            check("E2E_T20_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
            check("E2E_T20_STYLE_OK", o.get("STYLE") == "OK", raw)
            check("E2E_T20_RULES", o.get("STYLE_RULES") == "5", raw)
            check("E2E_T20_DECLS", o.get("STYLE_DECLS") == "11", raw)
            check("E2E_T20_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "5", raw)
            check("E2E_T20_BOXES", o.get("LAYOUT_BOXES") == "7", raw)
            check("E2E_T20_HIDDEN", o.get("LAYOUT_HIDDEN") == "2", raw)
            check("E2E_T20_ERRORS", o.get("LAYOUT_ERRORS") == "0", raw)
            check("E2E_T20_LAYOUT_OK", o.get("LAYOUT") == "OK" and rc == 0, raw)
            check(
                "E2E_T20_DUMP_EXACT",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8") == EXPECTED_LAYOUT_DUMP,
                raw,
            )

            # T21: chunked framing -> same bytes -> same layout dump
            out_dump2 = Path(td) / "layout2.txt"
            rc, o2, raw2 = run_client(
                ["--fetch", f"{base}/layout-chunked", "--layout",
                 "--layout-output", str(out_dump2)]
            )
            check("E2E_T21_FETCH_OK", o2.get("FETCH") == "OK" and rc == 0, raw2)
            check("E2E_T21_BOXES", o2.get("LAYOUT_BOXES") == "7", raw2)
            check(
                "E2E_T21_SAME_DUMP",
                out_dump2.exists()
                and out_dump2.read_text(encoding="utf-8") == EXPECTED_LAYOUT_DUMP,
                raw2,
            )

        # T22: --dom-file + --css-file (user principal): unsupported values
        # are counted (width:red, display:flex) and fall back honestly;
        # the layout still completes
        USER_HTML2 = (
            '<!doctype html><html><body><div class="bad">x'
            '<div id="flex">f</div></div><p>para</p></body></html>'
        )
        CSS_SRC2 = (
            "div { width: 100px }\n"
            ".bad { width: red }\n"
            "#flex { display: flex }\n"
            "p { height: 30px; padding: 2px }\n"
        )
        # Stage 9 (v0.10.0) re-froze: text gives .bad two 24px lines (x + f),
        # p keeps height 30 from CSS but its text still lays one line
        EXPECTED_USER_LAYOUT = """LAYOUT v2
B0 html
  rect 0 0 800 82
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 800 82
B2 html>body
  rect 0 0 800 82
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 800 82
B3 html>body>div
  rect 0 0 800 48
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 800 48
B# html>body>div::lines
  rect 0 0 800 24
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 800 24
  LN 0 24 1 10
  WORD 0 3 x
B4 html>body>div>div
  rect 0 24 100 24
  margin 0 700 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 100 24
B# html>body>div>div::lines
  rect 0 24 100 24
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 100 24
  LN 0 24 1 10
  WORD 0 4 f
B5 html>body>p
  rect 0 48 800 34
  margin 0 0 0 0
  padding 2 2 2 2
  border 0 0 0 0
  content 796 30
B# html>body>p::lines
  rect 2 50 796 24
  margin 0 0 0 0
  padding 0 0 0 0
  border 0 0 0 0
  content 796 24
  LN 0 24 4 40
  WORD 0 5 para
"""
        with tempfile.TemporaryDirectory() as td:
            src_html = Path(td) / "user.html"
            src_css = Path(td) / "user.css"
            out_dump = Path(td) / "user_layout.txt"
            src_html.write_text(USER_HTML2, encoding="utf-8")
            src_css.write_text(CSS_SRC2, encoding="utf-8")
            rc, o, raw = run_client(
                ["--dom-file", str(src_html), "--layout", "--css-file",
                 str(src_css), "--layout-output", str(out_dump)]
            )
            check("E2E_T22_LAYOUT_OK", o.get("LAYOUT") == "OK" and rc == 0, raw)
            check("E2E_T22_BOXES", o.get("LAYOUT_BOXES") == "8", raw)
            check("E2E_T22_HIDDEN", o.get("LAYOUT_HIDDEN") == "1", raw)
            check("E2E_T22_ERRORS", o.get("LAYOUT_ERRORS") == "2", raw)
            check(
                "E2E_T22_DUMP_EXACT",
                out_dump.exists()
                and out_dump.read_text(encoding="utf-8") == EXPECTED_USER_LAYOUT,
                raw,
            )

            # T22b: unwritable dump path -> honest IO error, exit 5
            rc, o, raw = run_client(
                ["--dom-file", str(src_html), "--layout", "--layout-output",
                 str(Path(td) / "no" / "such" / "dir" / "x.txt")]
            )
            check("E2E_T22B_LAYOUT_ERROR", o.get("LAYOUT") == "ERROR", raw)
            check("E2E_T22B_KIND", o.get("ERROR_KIND") == "IO", raw)
            check("E2E_T22B_EXIT5", rc == 5, raw)

            # ---- Stage 7: DISPLAY LIST + PAINT ----

            # T23: fetch -> STYLE -> LAYOUT -> DISPLAY LIST -> PAINT; the PPM
            # artifact and the PAINT v1 dump are frozen byte-for-byte
            with tempfile.TemporaryDirectory() as td:
                ppm = Path(td) / "out.ppm"
                pDump = Path(td) / "paint.txt"
                rc, o, raw = run_client(
                    ["--fetch", f"{base}/painted", "--paint",
                     "--paint-output", str(ppm), "--paint-dump", str(pDump)]
                )
                check("E2E_T23_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
                check("E2E_T23_LAYOUT_OK", o.get("LAYOUT") == "OK", raw)
                check("E2E_T23_CANVAS", o.get("PAINT_CANVAS") == "#e0e0e0", raw)
                check("E2E_T23_BOXES", o.get("PAINT_BOXES") == "6", raw)
                check("E2E_T23_CMDS", o.get("PAINT_CMDS") == "10", raw)
                check("E2E_T23_ERRORS", o.get("PAINT_ERRORS") == "0", raw)
                check("E2E_T23_PAINT_OK", o.get("PAINT") == "OK", raw)
                check(
                    "E2E_T23_DUMP_EXACT",
                    pDump.exists() and pDump.read_text(encoding="utf-8") == EXPECTED_PAINT_DUMP,
                    raw,
                )
                ppm_ok = ppm.exists() and ppm.stat().st_size == 15 + 800 * 600 * 3
                check("E2E_T23_PPM_SIZE", ppm_ok, raw)
                if ppm_ok:
                    data = ppm.read_bytes()
                    dig = hashlib.sha256(data).hexdigest()
                    check("E2E_T23_PPM_SHA", dig == EXPECTED_PPM_SHA256, raw)
                    check("E2E_T23_PPM_HEADER", data[:15] == b"P6\n800 600\n255\n", raw)

                    def pix(x: int, y: int) -> bytes:
                        off = 15 + (y * 800 + x) * 3
                        return data[off:off + 3]

                    check("E2E_T23_CANVAS_PIX", pix(700, 500) == b"\xe0\xe0\xe0", raw)
                    check("E2E_T23_RED_PIX", pix(150, 50) == b"\xff\x00\x00", raw)
                    # 50% blue over red -> (127, 0, 128)
                    check("E2E_T23_BLEND_PIX", pix(50, 25) == bytes([127, 0, 128]), raw)
                    # frame borders: top green, right #00ff00, bottom blue,
                    # left yellow (content-box border box 63x47 at 0,100)
                    check("E2E_T23_BT_PIX", pix(30, 101) == b"\x00\x80\x00", raw)
                    check("E2E_T23_BR_PIX", pix(62, 110) == b"\x00\xff\x00", raw)
                    check("E2E_T23_BB_PIX", pix(30, 145) == b"\x00\x00\xff", raw)
                    check("E2E_T23_BL_PIX", pix(0, 110) == b"\xff\xff\x00", raw)
                    check("E2E_T23_BG_PIX", pix(30, 110) == b"\xff\xff\xff", raw)
                    # stage 9: glyphs rasterize from the embedded bitmap font —
                    # pure-black pixels of 'r' (on red) and 'f' (on the white
                    # frame) prove the text raster reached the canvas
                    check("E2E_T23_TEXT_PIX_R", pix(3, 6) == b"\x00\x00\x00", raw)
                    check("E2E_T23_TEXT_PIX_F", pix(8, 106) == b"\x00\x00\x00", raw)

                # T24: chunked framing -> identical bytes -> identical artifacts
                ppm2 = Path(td) / "out2.ppm"
                pDump2 = Path(td) / "paint2.txt"
                rc, o2, raw2 = run_client(
                    ["--fetch", f"{base}/painted-chunked", "--paint",
                     "--paint-output", str(ppm2), "--paint-dump", str(pDump2)]
                )
                check("E2E_T24_FETCH_OK", o2.get("FETCH") == "OK" and rc == 0, raw2)
                check("E2E_T24_CMDS", o2.get("PAINT_CMDS") == "10", raw2)
                check(
                    "E2E_T24_SAME_DUMP",
                    pDump2.exists()
                    and pDump2.read_text(encoding="utf-8") == EXPECTED_PAINT_DUMP,
                    raw2,
                )
                check(
                    "E2E_T24_SAME_PPM",
                    ppm2.exists() and hashlib.sha256(ppm2.read_bytes()).hexdigest() == EXPECTED_PPM_SHA256,
                    raw2,
                )

            # T25: --dom-file: unsupported paint values are counted honestly
            # (garbage background-color, dashed border) and fall back; painting
            # of the rest of the page still completes
            with tempfile.TemporaryDirectory() as td:
                src_html = Path(td) / "u.html"
                src_css = Path(td) / "u.css"
                src_html.write_text(
                    '<!doctype html><html><body><div class="bad">x</div>'
                    '<div id="ok">y</div></body></html>',
                    encoding="utf-8",
                )
                src_css.write_text(
                    ".bad { background-color: 12px; border-top-style: dashed;"
                    " border-top-width: 2px; border-top-color: red }\n"
                    "#ok { width: 10px; height: 10px; background-color: teal }\n",
                    encoding="utf-8",
                )
                ppm3 = Path(td) / "out3.ppm"
                rc, o3, raw3 = run_client(
                    ["--dom-file", str(src_html), "--css-file", str(src_css),
                     "--paint", "--paint-output", str(ppm3)]
                )
                check("E2E_T25_PAINT_OK", o3.get("PAINT") == "OK" and rc == 0, raw3)
                check("E2E_T25_ERRORS", o3.get("PAINT_ERRORS") == "2", raw3)
                check("E2E_T25_CMDS", o3.get("PAINT_CMDS") == "3", raw3)
                check("E2E_T25_CANVAS", o3.get("PAINT_CANVAS") == "#ffffff", raw3)
                if ppm3.exists():
                    d3 = ppm3.read_bytes()
                    # stage 9: text gives .bad a 24px line box -> #ok is pushed
                    # down to y=24 (was y=0); the teal fill moved with it
                    off = 15 + (29 * 800 + 5) * 3
                    check("E2E_T25_TEAL_PIX", d3[off:off + 3] == b"\x00\x80\x80", raw3)
                    # glyph 'y' rasterizes dark below the 10px box (baseline
                    # 24+15=39, descender overflow) on the white canvas
                    off2 = 15 + (39 * 800 + 5) * 3
                    check("E2E_T25_TEXT_PIX", d3[off2:off2 + 3] == b"\x01\x01\x01", raw3)

                # T25b: unwritable PPM path -> honest IO error, exit 5
                rc, o4, raw4 = run_client(
                    ["--dom-file", str(src_html), "--css-file", str(src_css),
                     "--paint", "--paint-output",
                     str(Path(td) / "no" / "such" / "dir" / "x.ppm")]
                )
                check("E2E_T25B_PAINT_ERROR", o4.get("PAINT") == "ERROR", raw4)
                check("E2E_T25B_KIND", o4.get("ERROR_KIND") == "IO", raw4)
                check("E2E_T25B_EXIT5", rc == 5, raw4)

            # ---- Stage 8: PNG output (own zlib-stored writer) ----

            # T26: fetch -> ... -> PAINT -> PNG. The binary artifact is checked
            # structurally (signature, chunk sequence, chunk CRCs, IHDR fields)
            # AND semantically by an EXTERNAL decoder: Python stdlib zlib must
            # inflate our stored-block stream to 600 scanlines (filter 0) whose
            # pixels match the PPM ground truth of T23. Not circular: the
            # inflater is the OS one, not our writer.
            with tempfile.TemporaryDirectory() as td:
                pngf = Path(td) / "out.png"
                rc, o, raw = run_client(
                    ["--fetch", f"{base}/painted", "--paint",
                     "--png-output", str(pngf)]
                )
                check("E2E_T26_FETCH_OK", o.get("FETCH") == "OK" and rc == 0, raw)
                check("E2E_T26_PAINT_OK", o.get("PAINT") == "OK", raw)
                check("E2E_T26_PNG_SAVED", o.get("PAINT_PNG_SAVED") == str(pngf), raw)
                png_ok = pngf.exists() and pngf.stat().st_size == 1_920_813
                check("E2E_T26_PNG_SIZE", png_ok, raw)
                if png_ok:
                    data = pngf.read_bytes()
                    dig = hashlib.sha256(data).hexdigest()
                    check("E2E_T26_PNG_SHA", dig == EXPECTED_PNG_SHA256, raw)
                    check("E2E_T26_PNG_SIG", data[:8] == b"\x89PNG\r\n\x1a\n", raw)

                    def chunks(d: bytes):
                        off = 8
                        while off < len(d):
                            ln = struct.unpack(">I", d[off:off + 4])[0]
                            typ = d[off + 4:off + 8]
                            payload = d[off + 8:off + 8 + ln]
                            crc = struct.unpack(">I", d[off + 8 + ln:off + 12 + ln])[0]
                            good = (zlib.crc32(typ + payload) & 0xFFFFFFFF) == crc
                            yield typ, payload, good
                            off += 12 + ln

                    ks = list(chunks(data))
                    check(
                        "E2E_T26_CHUNK_SEQ",
                        [t.decode() for t, _, _ in ks] == ["IHDR", "IDAT", "IEND"],
                        raw,
                    )
                    check("E2E_T26_CHUNK_CRC", all(g for _, _, g in ks), raw)
                    ihdr = ks[0][1]
                    check(
                        "E2E_T26_IHDR",
                        ihdr == struct.pack(">IIBBBBB", 800, 600, 8, 6, 0, 0, 0),
                        raw,
                    )
                    rawpx = zlib.decompress(ks[1][1])
                    check("E2E_T26_INFLATE_SIZE", len(rawpx) == 600 * (1 + 800 * 4), raw)

                    def pix4(x: int, y: int) -> bytes:
                        o_ = y * (1 + 800 * 4) + 1 + x * 4
                        return rawpx[o_:o_ + 4]

                    check("E2E_T26_FILTERS",
                          all(rawpx[y * 3201] == 0 for y in range(600)), raw)
                    check("E2E_T26_CANVAS_PIX", pix4(700, 500) == b"\xe0\xe0\xe0\xff", raw)
                    check("E2E_T26_RED_PIX", pix4(150, 50) == b"\xff\x00\x00\xff", raw)
                    # 50% blue over red -> (127, 0, 128), opaque
                    check("E2E_T26_BLEND_PIX", pix4(50, 25) == bytes([127, 0, 128, 255]), raw)
                    # frame content + borders, same probes as T23 but RGBA
                    check("E2E_T26_BT_PIX", pix4(30, 101) == b"\x00\x80\x00\xff", raw)
                    check("E2E_T26_BR_PIX", pix4(62, 110) == b"\x00\xff\x00\xff", raw)
                    check("E2E_T26_BB_PIX", pix4(30, 145) == b"\x00\x00\xff\xff", raw)
                    check("E2E_T26_BL_PIX", pix4(0, 110) == b"\xff\xff\x00\xff", raw)
                    check("E2E_T26_BG_PIX", pix4(30, 110) == b"\xff\xff\xff\xff", raw)

                # T26b: chunked framing -> identical PNG bytes
                pngf2 = Path(td) / "out2.png"
                rc, o2, raw2 = run_client(
                    ["--fetch", f"{base}/painted-chunked", "--paint",
                     "--png-output", str(pngf2)]
                )
                check("E2E_T26B_FETCH_OK", o2.get("FETCH") == "OK" and rc == 0, raw2)
                check(
                    "E2E_T26B_SAME_PNG",
                    pngf2.exists()
                    and hashlib.sha256(pngf2.read_bytes()).hexdigest() == EXPECTED_PNG_SHA256,
                    raw2,
                )

            # T26c: --dom-file (user principal): PNG alongside PPM; unwritable
            # PNG path -> honest IO error, exit 5
            with tempfile.TemporaryDirectory() as td:
                src_html = Path(td) / "u.html"
                src_css = Path(td) / "u.css"
                src_html.write_text(
                    '<!doctype html><html><body><div id="ok">y</div></body></html>',
                    encoding="utf-8",
                )
                src_css.write_text(
                    "#ok { width: 10px; height: 10px; background-color: teal }\n",
                    encoding="utf-8",
                )
                ppm4 = Path(td) / "o.ppm"
                png4 = Path(td) / "o.png"
                rc, o3, raw3 = run_client(
                    ["--dom-file", str(src_html), "--css-file", str(src_css),
                     "--paint", "--paint-output", str(ppm4), "--png-output", str(png4)]
                )
                check("E2E_T26C_BOTH_OK", o3.get("PAINT") == "OK" and rc == 0, raw3)
                check("E2E_T26C_PPM_SAVED", ppm4.exists(), raw3)
                check("E2E_T26C_PNG_SAVED", png4.exists()
                      and png4.read_bytes()[:8] == b"\x89PNG\r\n\x1a\n", raw3)
                rc, o4, raw4 = run_client(
                    ["--dom-file", str(src_html), "--css-file", str(src_css),
                     "--paint", "--png-output",
                     str(Path(td) / "no" / "such" / "dir" / "x.png")]
                )
                check("E2E_T26C_PNG_ERROR", o4.get("PAINT") == "ERROR", raw4)
                check("E2E_T26C_KIND", o4.get("ERROR_KIND") == "IO", raw4)
                check("E2E_T26C_EXIT5", rc == 5, raw4)

        # ---- T27: GOLOGRAMMA v1 — real-world autonomous stress fixture ----
        # A 406 KB self-contained page (718 elements, depth 17, one 322 KB
        # <style> with @media/@keyframes/@font-face, custom properties, flex,
        # grid) through --dom-file -> STYLE -> LAYOUT -> PAINT -> PNG. The
        # engine must digest it WITHOUT a crash and report honest counters.
        # Stage 9 (v0.10.0) re-froze the baseline deliberately: text now lays
        # 477 lines and paints 3264 glyph runs; the page's colors are still
        # var(--custom-props) (honestly unresolved -> default backgrounds),
        # so most text lands black-on-white. The page VIOLATES the
        # HUMAN_FIRST health norms and the engine says so honestly: 61 lines
        # with line-height < 1.5, 50 lines longer than 75 chars, 7 text pairs
        # below the WCAG 2.2 / APCA Lc 75 floor. Counters move deliberately,
        # never silently. Fixture sha256 pins the input bytes.
        fix = ROOT / "tests" / "fixtures" / "gologramma_v1.html"
        check(
            "E2E_T27_FIXTURE_SHA",
            fix.is_file()
            and hashlib.sha256(fix.read_bytes()).hexdigest()
            == "2a6d10fad0241051525a428beff850613c84286c94a0acccb24472bd39301f11",
        )
        import tempfile

        with tempfile.TemporaryDirectory() as td:
            pngf = Path(td) / "gologramma.png"
            rc, o, raw = run_client(
                ["--dom-file", str(fix), "--layout", "--paint",
                 "--png-output", str(pngf)]
            )
            check("E2E_T27_EXIT0", rc == 0, raw)
            check("E2E_T27_DOM_OK",
                  o.get("DOM") == "OK" and o.get("PARSE_ERRORS") == "0", raw)
            check("E2E_T27_ELEMENTS", o.get("DOM_ELEMENTS") == "718", raw)
            check("E2E_T27_STYLE_OK", o.get("STYLE") == "OK", raw)
            check("E2E_T27_STYLE_RULES", o.get("STYLE_RULES") == "252", raw)
            check("E2E_T27_STYLE_DECLS", o.get("STYLE_DECLS") == "1105", raw)
            check("E2E_T27_AT_RULES", o.get("STYLE_AT_RULES_SKIPPED") == "31", raw)
            check("E2E_T27_STYLE_ERRORS", o.get("STYLE_ERRORS") == "38", raw)
            check("E2E_T27_STYLED", o.get("STYLE_ELEMENTS_STYLED") == "662", raw)
            # stage 10 re-freeze (honest correction): winners with var() used
            # to leak RAW "var(--x)" text into inherited values (inflating the
            # count to 2159 with values nobody could paint); unresolved var()
            # declarations now drop at computed-value time -> the page's real
            # inherited flow is 1815. GOLOGRAMMA defines 0 custom props and
            # references vars 541 times: STYLE_VAR_UNRESOLVED=541 is the page
            # honestly confessing it references what it never defined.
            check("E2E_T27_INHERITED", o.get("STYLE_INHERITED") == "1815", raw)
            check("E2E_T27_VAR_UNRESOLVED",
                  o.get("STYLE_VAR_UNRESOLVED") == "541", raw)
            check("E2E_T27_LAYOUT_OK", o.get("LAYOUT") == "OK", raw)
            check("E2E_T27_BOXES", o.get("LAYOUT_BOXES") == "675", raw)
            check("E2E_T27_HIDDEN", o.get("LAYOUT_HIDDEN") == "18", raw)
            check("E2E_T27_LAYOUT_ERRORS", o.get("LAYOUT_ERRORS") == "411", raw)
            check("E2E_T27_LINES", o.get("LAYOUT_LINES") == "477", raw)
            check("E2E_T27_TEXT_CHARS", o.get("LAYOUT_TEXT_CHARS") == "20922", raw)
            check("E2E_T27_MISSING_GLYPHS", o.get("LAYOUT_MISSING_GLYPHS") == "3", raw)
            check("E2E_T27_LINE_CHARS_MAX", o.get("LAYOUT_LINE_CHARS_MAX") == "80", raw)
            check("E2E_T27_PAINT_OK", o.get("PAINT") == "OK", raw)
            check("E2E_T27_CANVAS", o.get("PAINT_CANVAS") == "#ffffff", raw)
            check("E2E_T27_TEXT_PAINTED",
                  o.get("PAINT_BOXES") == "270" and o.get("PAINT_CMDS") == "3264"
                  and o.get("PAINT_TEXT_RUNS") == "3264"
                  and o.get("PAINT_TEXT_CHARS") == "18135", raw)
            # HUMAN_FIRST health norms are MEASURED on the real page — the
            # engine reports the page's violations honestly, not silently
            check("E2E_T27_HEALTH_LH",
                  o.get("HEALTH_LINE_HEIGHT_VIOLATIONS") == "61", raw)
            check("E2E_T27_HEALTH_LL",
                  o.get("HEALTH_LINE_LENGTH_VIOLATIONS") == "50", raw)
            check("E2E_T27_HEALTH_WCAG",
                  o.get("HEALTH_CONTRAST_VIOLATIONS") == "7", raw)
            check("E2E_T27_HEALTH_APCA",
                  o.get("HEALTH_APCA_VIOLATIONS") == "7", raw)
            png_ok = pngf.exists() and pngf.stat().st_size == 1_920_813
            check("E2E_T27_PNG_SIZE", png_ok, raw)
            if png_ok:
                check(
                    "E2E_T27_PNG_SHA",
                    hashlib.sha256(pngf.read_bytes()).hexdigest()
                    == "0ada362165f47cb7afd8674d55bd70b614c2222d7ab727fd3b5f1029ba9ed622",
                    raw,
                )

        # ---- T28: text fixture — wrap, struts, unicode, missing glyph, ----
        # ---- contrast norms measured honestly                        ----
        # tests/fixtures/text_v1.html: word wrap in a 120px box (baselines
        # 24px apart = the 1.5 default strut), line-height 8px (cramped ->
        # HEALTH counts 5 violations), line-height 3 (48px), 32px font,
        # Cyrillic white-on-black (WCAG 21:1, APCA 10788 - the WoB branch),
        # dim #cccccc on white (WCAG 1.61:1, APCA 2729 - honestly below the
        # Lc 75 floor), <br> strut, and one missing emoji (honestly counted,
        # no tofu invented). Regression anchor: the APCA WoB bug (subtraction
        # swapped) reported Lc 0 for every light-on-dark pair.
        fix9 = ROOT / "tests" / "fixtures" / "text_v1.html"
        check(
            "E2E_T28_FIXTURE_SHA",
            fix9.is_file()
            and hashlib.sha256(fix9.read_bytes()).hexdigest()
            == "8dbb9152f614b14206866af687bf5cad8e68039463c4755e9e776d5a39b8d6f5",
        )
        with tempfile.TemporaryDirectory() as td:
            png9 = Path(td) / "text.png"
            rc, o, raw = run_client(
                ["--dom-file", str(fix9), "--layout", "--paint",
                 "--png-output", str(png9)]
            )
            check("E2E_T28_EXIT0", rc == 0, raw)
            check("E2E_T28_LINES", o.get("LAYOUT_LINES") == "21", raw)
            check("E2E_T28_BOXES", o.get("LAYOUT_BOXES") == "18", raw)
            check("E2E_T28_TEXT_CHARS", o.get("LAYOUT_TEXT_CHARS") == "180", raw)
            check("E2E_T28_BREAKS", o.get("LAYOUT_BREAKS") == "1", raw)
            check("E2E_T28_MISSING_GLYPHS", o.get("LAYOUT_MISSING_GLYPHS") == "1", raw)
            check("E2E_T28_HEALTH_LH",
                  o.get("HEALTH_LINE_HEIGHT_VIOLATIONS") == "5", raw)
            check("E2E_T28_TEXT_RUNS", o.get("PAINT_TEXT_RUNS") == "37", raw)
            check("E2E_T28_TEXT_CMDS",
                  o.get("PAINT_CMDS") == "38" and o.get("PAINT_BOXES") == "9", raw)
            check("E2E_T28_WCAG_MIN", o.get("PAINT_WCAG_MIN_X100") == "161", raw)
            check("E2E_T28_APCA_MIN",
                  o.get("PAINT_APCA_MIN_LC_X100") == "2729", raw)
            check("E2E_T28_HEALTH_CONTRAST",
                  o.get("HEALTH_CONTRAST_VIOLATIONS") == "3", raw)
            check("E2E_T28_HEALTH_APCA",
                  o.get("HEALTH_APCA_VIOLATIONS") == "3", raw)
            png9_ok = png9.exists() and png9.stat().st_size == 1_920_813
            check("E2E_T28_PNG_SIZE", png9_ok, raw)
            if png9_ok:
                check(
                    "E2E_T28_PNG_SHA",
                    hashlib.sha256(png9.read_bytes()).hexdigest()
                    == "7d229e14ca31f65ded179ba30fccdd3554266b3cb3ce71a1bd0381a888c3cefb",
                    raw,
                )

        # ---- T29: var() + custom properties (stage 10) ----
        # tests/fixtures/vars_v1.html: define-on-html/use-on-children via
        # inheritance of RESOLVED values, fallback for a missing name, an
        # unresolved var that drops its declaration (inherited value keeps
        # flowing), case-sensitive custom names (--Tok vs --tok), a var()
        # reference cycle (--a <-> --b invalidates both), white text on a
        # var() background, and var/plain mixed declarations. Counters are
        # the honest part: 3 vars defined, 9 refs, 1 fallback, 3 unresolved
        # (2 misses + 1 use of a cycle member), 2 cycles.
        fix10 = ROOT / "tests" / "fixtures" / "vars_v1.html"
        check(
            "E2E_T29_FIXTURE_SHA",
            fix10.is_file()
            and hashlib.sha256(fix10.read_bytes()).hexdigest()
            == "a9ea7172c09f02feba834b190c5309b29a7c742e9e9df9ecd07d1ab7beb8f3bb",
        )
        with tempfile.TemporaryDirectory() as td:
            png10 = Path(td) / "vars.png"
            rc, o, raw = run_client(
                ["--dom-file", str(fix10), "--style", "--layout", "--paint",
                 "--png", "--png-output", str(png10)]
            )
            check("E2E_T29_EXIT0", rc == 0, raw)
            check("E2E_T29_VARS_DEFINED",
                  o.get("STYLE_VARS_DEFINED") == "3", raw)
            check("E2E_T29_VAR_REFS", o.get("STYLE_VAR_REFS") == "9", raw)
            check("E2E_T29_VAR_FALLBACK",
                  o.get("STYLE_VAR_FALLBACK_USED") == "1", raw)
            check("E2E_T29_VAR_UNRESOLVED",
                  o.get("STYLE_VAR_UNRESOLVED") == "3", raw)
            check("E2E_T29_VAR_CYCLES",
                  o.get("STYLE_VAR_CYCLES") == "2", raw)
            check("E2E_T29_PROPS", o.get("STYLE_PROPS") == "13", raw)
            check("E2E_T29_LINES", o.get("LAYOUT_LINES") == "10", raw)
            check("E2E_T29_PAINT_CHARS",
                  o.get("PAINT_TEXT_CHARS") == "222", raw)
            check("E2E_T29_HEALTH_LH",
                  o.get("HEALTH_LINE_HEIGHT_VIOLATIONS") == "0", raw)
            check("E2E_T29_HEALTH_CONTRAST",
                  o.get("HEALTH_CONTRAST_VIOLATIONS") == "0", raw)
            check("E2E_T29_WCAG_MIN",
                  o.get("PAINT_WCAG_MIN_X100") == "716", raw)
            check("E2E_T29_APCA_MIN",
                  o.get("PAINT_APCA_MIN_LC_X100") == "8433", raw)
            png10_ok = png10.exists() and png10.stat().st_size == 1_920_813
            check("E2E_T29_PNG_SIZE", png10_ok, raw)
            if png10_ok:
                check(
                    "E2E_T29_PNG_SHA",
                    hashlib.sha256(png10.read_bytes()).hexdigest()
                    == "b5de6abdd59a41359698bdc02bd09b659869619b11d2e873959adf0aa1fc3cd7",
                    raw,
                )

    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()

    failed = [r for r in results if not r[1]]
    for name, ok, _ in results:
        print(f"{name}=PASS" if ok else f"{name}=FAIL")
    if failed:
        print(f"E2E_TEST=FAIL ({len(failed)} failed)")
        return 1
    print("E2E_TEST=PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
