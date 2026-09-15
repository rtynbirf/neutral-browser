#!/usr/bin/env python3
"""End-to-end machine verification for neutral-browser (stages 2-5).

Starts a local HTTP test server, drives the REAL compiled binary and asserts
its machine output line by line. Verdict per case (E2E_Tn_*=PASS/FAIL) and a
final E2E_TEST=PASS / E2E_TEST=FAIL.

Usage: python3 tools/e2e.py [path-to-binary]
Build first: cargo build --locked
"""
import hashlib
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "debug" / "neutral-browser"

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
