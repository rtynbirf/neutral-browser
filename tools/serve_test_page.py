#!/usr/bin/env python3
"""Local HTTP/1.1 test server for neutral-browser e2e verification.

Endpoints:
  / and /index.html -> 200, Content-Length framing, known PAGE bytes
  /chunked          -> 200, Transfer-Encoding: chunked, same PAGE bytes in 2 chunks
  /styled           -> 200, page with a <style> element (stage 4)
  /styled-chunked   -> 200, chunked framing of the styled page
  /inherit          -> 200, page exercising CSS inheritance (stage 5)
  /styled-link      -> 200, page with <link rel=stylesheet> + <style> (stage 5)
  /styled-link-chunked -> same page, sheet URL uses chunked framing
  /sheet.css        -> 200, stylesheet body for /styled-link
  /sheet-chunked    -> 200, same stylesheet body, chunked framing
  /late.css         -> 200, stylesheet fetched AFTER the inline <style>
  /link-order       -> 200, <style> then <link>: the later sheet wins
  /link-broken      -> 200, three broken links (404 / bad URL / https)
  /link-cap         -> 200, ten distinct stylesheet links (cap = 8)
  /capN.css (1..10) -> 200, distinct stylesheets for the cap test
  /sub/rel.html     -> 200, relative + ../ + absolute link hrefs
  /sub/sheet.css    -> 200, stylesheet for /sub/rel.html
  /root.css, /abs.css -> 200, stylesheets for /sub/rel.html
  /layout           -> 200, page exercising block layout (stage 6)
  /layout-chunked   -> 200, chunked framing of the layout page
  /painted          -> 200, page exercising display list + paint (stage 7)
  /painted-chunked  -> 200, chunked framing of the painted page
  anything else     -> 404, short plain-text body

No third-party modules. No telemetry. Binds 127.0.0.1 only.
"""
import http.server
import socketserver
import sys

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18080

PAGE = (
    b"<!doctype html><html><head><title>neutral-browser stage2 e2e</title></head>"
    b"<body><h1>STAGE2 E2E OK</h1><p>broker-gated fetch</p></body></html>"
)

STYLED = (
    b'<!doctype html><html><head><title>t</title><style>'
    b'p { color: red; font-size: 12px }'
    b'.x { color: green }'
    b'#y { border: 1px; color: blue !important }'
    b'p.x { font-size: 20px }'
    b'div p, span { margin: 0 }'
    b'p[x="1"] { padding: 2px }'
    b'</style></head><body>'
    b'<p style="margin: 4px">one</p>'
    b'<p class="x" id="y" x="1">two</p>'
    b'<div><p>three</p></div>'
    b'<span>s</span>'
    b'</body></html>'
)

INHERIT = (
    b'<!doctype html><html><head><title>i</title><style>'
    b'body { color: green; font-family: monospace }'
    b'div > p { text-align: center }'
    b'p em { border-width: 3px }'
    b'</style></head><body>'
    b'<div><p>para<em>em</em></p></div>'
    b'<span>out</span>'
    b'</body></html>'
)

STYLED_LINK = (
    b'<!doctype html><html><head><title>t</title>'
    b'<link rel="stylesheet" href="/sheet.css">'
    b'<style>p { margin: 9px }</style></head>'
    b'<body><p class="c">x</p></body></html>'
)

STYLED_LINK_CHUNKED = STYLED_LINK.replace(b'href="/sheet.css"', b'href="/sheet-chunked"')

LINK_ORDER = (
    b'<!doctype html><html><head><title>t</title>'
    b'<style>.z { color: red }</style>'
    b'<link rel="stylesheet" href="/late.css"></head>'
    b'<body><b class="z">x</b></body></html>'
)

LINK_BROKEN = (
    b'<!doctype html><html><head><title>t</title>'
    b'<link rel="stylesheet" href="/missing.css">'
    b'<link rel="stylesheet" href="bad href.css">'
    b'<link rel="stylesheet" href="https://127.0.0.1/x.css">'
    b'</head><body><p>x</p></body></html>'
)

LINK_CAP = (
    b'<!doctype html><html><head><title>t</title>'
    + b"".join(
        b'<link rel="stylesheet" href="/cap%d.css">' % i for i in range(1, 11)
    )
    + b'</head><body><p>x</p></body></html>'
)

REL_PAGE = (
    b'<!doctype html><html><head><title>t</title>'
    b'<link rel="STYLESHEET" href="sheet.css">'
    b'<link rel="stylesheet" href="../root.css">'
    b'<link rel="stylesheet" href="/abs.css"></head>'
    b'<body><p>x</p></body></html>'
)

LAYOUT = (
    b'<!doctype html><html><head><title>l</title><style>'
    b'body { margin: 0 }'
    b'#wrap { width: 400px; margin: 0 auto }'
    b'#a { width: 100px; height: 40px; padding: 5px; border-width: 2px;'
    b' box-sizing: border-box }'
    b'.none { display: none }'
    b'#pc { width: 50%; height: 25% }'
    b'</style></head><body>'
    b'<div id="wrap"><div id="a">a</div><div class="none">h</div><div id="pc">p</div></div>'
    b'</body></html>'
)

PAINTED = (
    b'<!doctype html><html><head><title>p</title><style>'
    b'html { background-color: #e0e0e0 }'
    b'#red { width: 200px; height: 100px; background-color: red }'
    b'#blue { width: 100px; height: 50px; background-color: rgba(0,0,255,0.5) }'
    b'#frame { width: 60px; height: 40px; background-color: white;'
    b' border-top-width: 3px; border-right-width: 2px;'
    b' border-bottom-width: 4px; border-left-width: 1px;'
    b' border-top-style: solid; border-right-style: solid;'
    b' border-bottom-style: solid; border-left-style: solid;'
    b' border-top-color: green; border-right-color: #0f0;'
    b' border-bottom-color: rgb(0,0,255); border-left-color: yellow }'
    b'</style></head><body>'
    b'<div id="red">r<div id="blue">b</div></div>'
    b'<div id="frame">f</div>'
    b'</body></html>'
)

SHEET_CSS = b"p { color: red }\n.c { color: green; padding: 1px }\n"
LATE_CSS = b".z { color: blue }\n"
ROOT_CSS = b"p { color: teal }\n"
ABS_CSS = b"p { letter-spacing: 1px }\n"
SUB_SHEET_CSS = b"p { text-indent: 4px }\n"
CAP_CSS = b"p { color: red }\n"


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send_fixed(self, code: int, body: bytes, ctype: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        if self.path in ("/", "/index.html"):
            self._send_fixed(200, PAGE, "text/html; charset=utf-8")
        elif self.path == "/styled":
            self._send_fixed(200, STYLED, "text/html; charset=utf-8")
        elif self.path == "/styled-chunked":
            self._send_chunked(STYLED, "text/html; charset=utf-8")
        elif self.path == "/chunked":
            self._send_chunked(PAGE, "text/html; charset=utf-8")
        elif self.path == "/inherit":
            self._send_fixed(200, INHERIT, "text/html; charset=utf-8")
        elif self.path == "/styled-link":
            self._send_fixed(200, STYLED_LINK, "text/html; charset=utf-8")
        elif self.path == "/styled-link-chunked":
            self._send_fixed(200, STYLED_LINK_CHUNKED, "text/html; charset=utf-8")
        elif self.path == "/sheet.css":
            self._send_fixed(200, SHEET_CSS, "text/css")
        elif self.path == "/sheet-chunked":
            self._send_chunked(SHEET_CSS, "text/css")
        elif self.path == "/late.css":
            self._send_fixed(200, LATE_CSS, "text/css")
        elif self.path == "/link-order":
            self._send_fixed(200, LINK_ORDER, "text/html; charset=utf-8")
        elif self.path == "/link-broken":
            self._send_fixed(200, LINK_BROKEN, "text/html; charset=utf-8")
        elif self.path == "/link-cap":
            self._send_fixed(200, LINK_CAP, "text/html; charset=utf-8")
        elif self.path.startswith("/cap") and self.path.endswith(".css"):
            self._send_fixed(200, CAP_CSS, "text/css")
        elif self.path == "/sub/rel.html":
            self._send_fixed(200, REL_PAGE, "text/html; charset=utf-8")
        elif self.path == "/layout":
            self._send_fixed(200, LAYOUT, "text/html; charset=utf-8")
        elif self.path == "/layout-chunked":
            self._send_chunked(LAYOUT, "text/html; charset=utf-8")
        elif self.path == "/painted":
            self._send_fixed(200, PAINTED, "text/html; charset=utf-8")
        elif self.path == "/painted-chunked":
            self._send_chunked(PAINTED, "text/html; charset=utf-8")
        elif self.path == "/sub/sheet.css":
            self._send_fixed(200, SUB_SHEET_CSS, "text/css")
        elif self.path == "/root.css":
            self._send_fixed(200, ROOT_CSS, "text/css")
        elif self.path == "/abs.css":
            self._send_fixed(200, ABS_CSS, "text/css")
        else:
            self._send_fixed(404, b"not found", "text/plain")

    def _send_chunked(self, body: bytes, ctype: str) -> None:
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        half = len(body) // 2
        for part in (body[:half], body[half:]):
            self.wfile.write(f"{len(part):X}\r\n".encode("ascii") + part + b"\r\n")
        self.wfile.write(b"0\r\n\r\n")

    def log_message(self, fmt, *args):  # silent: test output must stay clean
        pass


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    with Server(("127.0.0.1", PORT), Handler) as httpd:
        print(f"SERVED_READY={PORT}", flush=True)
        httpd.serve_forever()
