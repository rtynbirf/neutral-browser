#!/usr/bin/env python3
"""End-to-end machine verification for neutral-browser stage 2.

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
