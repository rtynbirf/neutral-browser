#!/usr/bin/env python3
"""Local HTTP/1.1 test server for neutral-browser stage 2 e2e verification.

Endpoints:
  / and /index.html -> 200, Content-Length framing, known PAGE bytes
  /chunked          -> 200, Transfer-Encoding: chunked, same PAGE bytes in 2 chunks
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
        elif self.path == "/chunked":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            half = len(PAGE) // 2
            for part in (PAGE[:half], PAGE[half:]):
                self.wfile.write(f"{len(part):X}\r\n".encode("ascii") + part + b"\r\n")
            self.wfile.write(b"0\r\n\r\n")
        else:
            self._send_fixed(404, b"not found", "text/plain")

    def log_message(self, fmt, *args):  # silent: test output must stay clean
        pass


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    with Server(("127.0.0.1", PORT), Handler) as httpd:
        print(f"SERVED_READY={PORT}", flush=True)
        httpd.serve_forever()
