#!/usr/bin/env python3
"""Сертификат молчания — RUNTIME-детектор внеполосных запросов (П43, волна ×6).

Статический сканер (tools/silence_scan.py) доказывает отсутствие каналов
«домой» в ИСХОДНИКАХ (уровень 0). Этот инструмент доказывает поведение
СОБРАННОГО бинарника (уровень 1.5): запускает реальный движок под
детектором сетевых попыток и утверждает — ноль внеполосных (не
разрешённых приёмкой) сетевых обращений. Любая попытка дотянуться до
адреса вне явного разрешённого списка = красный, fail-closed.

Детектор (выбор по окружению, честно печатается маркером DETECTOR=):
  strace    — ptrace-трасса syscall'ов connect/sendto/sendmsg: видит
              ЛЮБЫЕ попытки, включая сырые syscall'ы мимо libc. Приоритет.
  ldpreload — LD_PRELOAD-шим (интерпозиция connect/sendto/sendmsg на
              уровне libc, компилируется из встроенного C-исходника
              через cc): ловит все вызовы через libc. Фолбэк без ptrace.
  none      — сертификат НЕ выдаётся (SILENCE_E2E=NOT_ISSUED, exit 2):
              fail-closed — без детектора свидетельство молчания ложно.

Дополнительно (best-effort, честно маркерами): строгий сетевой namespace
`unshare` (только loopback) — снаружи физически невозможно открыть
внешнее соединение; внутри namespace прогон повторяет приёмку.

Негативная приёмка (обязательна, машинная):
  1. LEAK_PROBE  — «нарушитель» (отдельный бинарник, который разрешает
     себе известный внешний адрес RFC 5737 TEST-NET-1) обязан быть
     ПОЙМАН детектором (красный). Если детектор пропустил — сертификат
     не выдаётся: детектор порочен.
  2. CLEAN_PROBE — тот же нарушитель, но на loopback: обязан быть
     ЧИСТЫМ (детектор не «красит всё подряд» — двухсторонний контроль).
  3. BLOCKED_ADDR — реальный браузер под детектором с ПУСТЫМ
     разрешённым списком тянется к известному адресу — попытка обязана
     быть поймана (красный). Это и есть «режим, который разрешает
     известный адрес, — тест ловит его как нарушение».

Режимы:
  python3 tools/silence_e2e.py [--certificate PATH [--require-netns]]
  python3 tools/silence_e2e.py --selftest
  python3 tools/silence_e2e.py --watch --allow 127.0.0.1:8080 -- CMD...

Машинные маркеры: последняя строка прогона —
  SILENCE_E2E=PASS (exit 0) | SILENCE_E2E=FAIL (exit 1) |
  SILENCE_E2E=NOT_ISSUED (exit 2, окружение не позволяет сертифицировать)

Std-only Python (часть оффлайн-петли; см. tests/architecture.py, S6).
"""
from __future__ import annotations

import fcntl
import hashlib
import ipaddress
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
TOOL = Path(__file__).resolve()

# RFC 5737 TEST-NET-1: документационный адрес. Безопасно для попыток
# соединения: реального хоста за ним по построению нет. Используется как
# «известный адрес» в негативных приёмках (детектор обязан его ловить).
KNOWN_ADDR = "192.0.2.1"

# Страница зелёного прогона: ровно один fetch на loopback, известный sha256.
PAGE = (
    b"<!doctype html><html><head><title>silence-certificate-e2e</title></head>"
    b"<body><h1>SILENCE CERTIFICATE</h1><p>loopback-only fetch</p></body></html>"
)
PAGE_SHA = hashlib.sha256(PAGE).hexdigest()

# --- синтетические строки strace для юнит-приёмки парсера ----------------
STRACE_SAMPLES = [
    # (строка, ожидаемое: event/family/addr/port | "OTHER" | "UNPARSED")
    ('connect(3, {sa_family=AF_INET, sin_port=htons(8080), '
     'sin_addr=inet_addr("127.0.0.1")}, 16) = 0', ("connect", 2, "127.0.0.1", 8080)),
    ('1234  connect(5, {sa_family=AF_INET, sin_port=htons(53), '
     'sin_addr=8.8.8.8}, 16) = -1 ENETUNREACH Нет маршрута',
     ("connect", 2, "8.8.8.8", 53)),
    ('connect(4, {sa_family=AF_INET6, sin6_port=htons(443), '
     'sin6_addr=inet_pton(AF_INET6, "2001:db8::1")}, 28) = 0',
     ("connect", 10, "2001:db8::1", 443)),
    ('sendto(6, "GET / HTTP/1.1", 15, 0, {sa_family=AF_INET, '
     'sin_port=htons(80), sin_addr=inet_addr("192.0.2.1")}, 16) = 15',
     ("sendto", 2, "192.0.2.1", 80)),
    ('sendmsg(7, {msg_name={sa_family=AF_INET, sin_port=htons(53), '
     'sin_addr=inet_addr("9.9.9.9")}, msg_iov=[{iov_base="q", iov_len=1}]}, 0) = 1',
     ("sendmsg", 2, "9.9.9.9", 53)),
    ('connect(3, {sa_family=AF_UNIX, sun_path="/tmp/x.sock"}, 110) = 0', "OTHER"),
    ('1234  connect(3, {sa_family=AF_NETLINK, nl_pid=0, nl_groups=00000000}, 12) = 0',
     "OTHER"),
    ('connect(3, {sa_family=AF_INET, <unfinished ...>}', "UNPARSED"),
]

# --- встроенный LD_PRELOAD-шим (интерпозиция libc) ------------------------
SHIM_C = r"""
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

static FILE *logf(void) {
    const char *p = getenv("SIL_LOG");
    if (!p) return NULL;
    static FILE *f;
    if (!f) { f = fopen(p, "a"); if (f) setvbuf(f, NULL, _IOLBF, 0); }
    return f;
}

static void emit(const char *ev, int fd, const struct sockaddr *sa, socklen_t len) {
    FILE *f = logf();
    if (!f || !sa || len < sizeof(sa_family_t)) return;
    char out[512];
    out[0] = 0;
    if (sa->sa_family == AF_INET && len >= sizeof(struct sockaddr_in)) {
        char ip[INET_ADDRSTRLEN] = {0};
        const struct sockaddr_in *a = (const struct sockaddr_in *)sa;
        inet_ntop(AF_INET, &a->sin_addr, ip, sizeof ip);
        snprintf(out, sizeof out,
                 "{\"event\":\"%s\",\"fd\":%d,\"family\":2,\"addr\":\"%s\",\"port\":%d}",
                 ev, fd, ip, ntohs(a->sin_port));
    } else if (sa->sa_family == AF_INET6 && len >= sizeof(struct sockaddr_in6)) {
        char ip[INET6_ADDRSTRLEN] = {0};
        const struct sockaddr_in6 *a = (const struct sockaddr_in6 *)sa;
        inet_ntop(AF_INET6, &a->sin6_addr, ip, sizeof ip);
        snprintf(out, sizeof out,
                 "{\"event\":\"%s\",\"fd\":%d,\"family\":10,\"addr\":\"%s\",\"port\":%d,\"scope\":%u}",
                 ev, fd, ip, ntohs(a->sin6_port), (unsigned)a->sin6_scope_id);
    } else {
        snprintf(out, sizeof out, "{\"event\":\"%s\",\"fd\":%d,\"family\":%d}",
                 ev, fd, (int)sa->sa_family);
    }
    if (out[0]) { fputs(out, f); fputc('\n', f); }
}

typedef int (*connect_fn)(int, const struct sockaddr *, socklen_t);
int connect(int fd, const struct sockaddr *sa, socklen_t len) {
    emit("connect", fd, sa, len);
    static connect_fn real;
    if (!real) real = (connect_fn)dlsym(RTLD_NEXT, "connect");
    return real(fd, sa, len);
}

typedef ssize_t (*sendto_fn)(int, const void *, size_t, int,
                             const struct sockaddr *, socklen_t);
ssize_t sendto(int fd, const void *buf, size_t n, int flags,
               const struct sockaddr *sa, socklen_t len) {
    emit("sendto", fd, sa, len);
    static sendto_fn real;
    if (!real) real = (sendto_fn)dlsym(RTLD_NEXT, "sendto");
    return real(fd, buf, n, flags, sa, len);
}

typedef ssize_t (*sendmsg_fn)(int, const struct msghdr *, int);
ssize_t sendmsg(int fd, const struct msghdr *msg, int flags) {
    if (msg && msg->msg_name)
        emit("sendmsg", fd, (const struct sockaddr *)msg->msg_name, msg->msg_namelen);
    static sendmsg_fn real;
    if (!real) real = (sendmsg_fn)dlsym(RTLD_NEXT, "sendmsg");
    return real(fd, msg, flags);
}
"""

# --- негативный «нарушитель»: отдельный бинарник, который стучится -------
RUST_PROBE_EXT = (
    "use std::net::{SocketAddr, TcpStream};\n"
    "use std::time::Duration;\n"
    "fn main() {\n"
    "    let a: SocketAddr = \"192.0.2.1:80\".parse().expect(\"addr\");\n"
    "    let _ = TcpStream::connect_timeout(&a, Duration::from_millis(600));\n"
    "    println!(\"PROBE_EXTERNAL_DONE\");\n"
    "}\n"
)
RUST_PROBE_CLEAN = (
    "use std::net::{SocketAddr, TcpStream};\n"
    "use std::time::Duration;\n"
    "fn main() {\n"
    "    let a: SocketAddr = \"127.0.0.1:PORT_PLACEHOLDER\".parse().expect(\"addr\");\n"
    "    let _ = TcpStream::connect_timeout(&a, Duration::from_millis(600));\n"
    "    println!(\"PROBE_CLEAN_DONE\");\n"
    "}\n"
)
PY_PROBE_EXT = (
    "import socket\n"
    "s = socket.socket()\n"
    "s.settimeout(0.7)\n"
    "try:\n"
    "    s.connect(('192.0.2.1', 80))\n"
    "    print('PROBE_EXTERNAL_CONNECTED')\n"
    "except Exception as e:\n"
    "    print('PROBE_EXTERNAL_REFUSED', type(e).__name__)\n"
)
PY_PROBE_CLEAN = (
    "import socket\n"
    "s = socket.socket()\n"
    "s.settimeout(0.7)\n"
    "try:\n"
    "    s.connect(('127.0.0.1', PORT_PLACEHOLDER))\n"
    "    print('PROBE_CLEAN_CONNECTED')\n"
    "except Exception as e:\n"
    "    print('PROBE_CLEAN_REFUSED', type(e).__name__)\n"
)

_PROXY_RE = re.compile(r"^(?i:(?:http|https|all|ftp|socks)_?proxy|no_?proxy)$")


# ---------------------------------------------------------------------------
# локальный loopback-сервер (доверенная часть тестовой среды)
# ---------------------------------------------------------------------------
class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _fixed(self, code: int, body: bytes, ctype: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        if self.path in ("/", "/index.html"):
            self._fixed(200, PAGE, "text/html; charset=utf-8")
        else:
            self._fixed(404, b"not found", "text/plain")

    def log_message(self, fmt, *args):  # тишина: машинный вывод теста чистый
        pass


class Server(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True


def start_server() -> tuple[Server, int]:
    srv = Server(("127.0.0.1", 0), Handler)
    port = srv.server_address[1]
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    return srv, port


def stop_server(srv: Server) -> None:
    srv.shutdown()
    srv.server_close()


# ---------------------------------------------------------------------------
# детектор: strace / LD_PRELOAD-шим
# ---------------------------------------------------------------------------
def detect_tier() -> str:
    if shutil.which("strace"):
        return "strace"
    if shutil.which("cc") or shutil.which("gcc"):
        return "ldpreload"
    return "none"


def build_shim(tmp: Path) -> Path | None:
    cc = shutil.which("cc") or shutil.which("gcc")
    if not cc:
        return None
    src = tmp / "silence_shim.c"
    lib = tmp / "libsilence_shim.so"
    src.write_text(SHIM_C, encoding="utf-8")
    r = subprocess.run(
        [cc, "-shared", "-fPIC", "-O0", "-o", str(lib), str(src), "-ldl"],
        capture_output=True, text=True, timeout=120,
    )
    if r.returncode != 0 or not lib.is_file():
        print(f"SHIM_BUILD=FAIL :: {r.stderr.strip()[:200]}")
        return None
    return lib


def harden_env(base: dict | None = None) -> dict:
    env = dict(base if base is not None else os.environ)
    for k in list(env.keys()):
        if _PROXY_RE.match(k):
            env.pop(k, None)
    return env


def _norm_addr(family: int, raw: str):
    """Нормализация адреса: v4-mapped IPv6 -> IPv4; возврат (family, addr)."""
    if family == 10:
        try:
            ip6 = ipaddress.IPv6Address(raw)
        except ValueError:
            return family, raw
        mapped = ip6.ipv4_mapped
        if mapped is not None:
            return 2, str(mapped)
    return family, raw


def classify_event(ev: dict, allow: set) -> str:
    """ALLOW — разрешено приёмкой; VIOLATION — внеполосная попытка;
    OTHER — не IP-сеть (AF_UNIX/AF_NETLINK, kernel-внутреннее)."""
    fam = ev.get("family")
    addr, port = ev.get("addr"), ev.get("port")
    if fam in (1, 16):  # AF_UNIX, AF_NETLINK
        return "OTHER"
    if fam not in (2, 10) or not addr:
        return "UNPARSED"
    fam, addr = _norm_addr(fam, addr)
    try:
        ip = ipaddress.ip_address(addr)
    except ValueError:
        return "UNPARSED"
    if fam == 10:
        # IPv6: разрешён только ::1
        return "ALLOW" if ip == ipaddress.IPv6Address("::1") else "VIOLATION"
    # IPv4: разрешено ТОЛЬКО то, что явно в приёмке (fail-closed: даже
    # loopback без явного разрешения — нарушение; неизвестный порт не
    # матчится со списком и потому красный, а не тихий пропуск)
    return "ALLOW" if (str(ip), port) in allow else "VIOLATION"


def classify_all(events: list, allow: set) -> dict:
    verdict = {"ALLOW": 0, "VIOLATION": 0, "OTHER": 0, "UNPARSED": 0, "list": []}
    for ev in events:
        v = classify_event(ev, allow)
        verdict[v] += 1
        if v in ("VIOLATION", "UNPARSED"):
            verdict["list"].append(ev)
    return verdict


def _parse_strace_line(line: str) -> dict | str:
    """dict-event | "OTHER" | "UNPARSED" | None (строка не о событии)."""
    body = re.sub(r"^\[pid\s+\d+\]\s*", "", line.rstrip("\n"))
    body = re.sub(r"^\d+\s+", "", body)
    m = re.match(r"^(connect|sendto|sendmsg)\(", body)
    resumed = "resumed" in body
    if not m:
        return "UNPARSED" if resumed else None
    syscall = m.group(1)
    fam_m = re.search(r"sa_family=AF_(\w+)", body)
    if not fam_m:
        # sendto/sendmsg с NULL-адресом идут в УЖЕ подключённый сокет —
        # его connect() классифицирован отдельно (fail-open тут нет:
        # цель известна с момента connect). Остальное без семейства —
        # неразобранная попытка = нарушение.
        if syscall in ("sendto", "sendmsg") and ("NULL" in body):
            return None
        return "UNPARSED"
    famname = fam_m.group(1)
    if famname == "UNIX" or famname == "NETLINK":
        return "OTHER"
    if famname == "INET":
        a = re.search(r"sin_addr=(?:inet_addr\(\")?(\d+\.\d+\.\d+\.\d+)", body)
        p = re.search(r"sin_port=htons\((\d+)\)", body)
        if not a:
            return "UNPARSED"
        return {"event": syscall, "family": 2, "addr": a.group(1),
                "port": int(p.group(1)) if p else None}
    if famname == "INET6":
        a = re.search(r'inet_pton\(AF_INET6,\s*"([^"]+)"', body)
        if not a:
            a = re.search(r"sin6_addr=([0-9A-Fa-f:]+)", body)
        p = re.search(r"sin6_port=htons\((\d+)\)", body)
        if not a:
            return "UNPARSED"
        return {"event": syscall, "family": 10, "addr": a.group(1),
                "port": int(p.group(1)) if p else None}
    # семейство не IP-сеть (PACKET и пр.) — не внеполосный сетевой канал
    return "OTHER"


def parse_strace_log(path: Path) -> tuple[list, list]:
    events, unparsed = [], []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.strip():
            continue
        r = _parse_strace_line(line)
        if r is None:
            continue
        if r == "UNPARSED":
            unparsed.append(line.strip()[:200])
        elif r == "OTHER":
            events.append({"event": "?", "family": 1, "addr": None, "port": None})
        else:
            events.append(r)
    return events, unparsed


def parse_shim_log(path: Path) -> tuple[list, list]:
    events, unparsed = [], []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.strip():
            continue
        try:
            ev = json.loads(line)
            if not isinstance(ev, dict) or "family" not in ev:
                raise ValueError("not an event")
            events.append(ev)
        except ValueError:
            unparsed.append(line.strip()[:200])
    return events, unparsed


class Watched:
    """Запуск команды под детектором + классификация сетевых событий."""

    def __init__(self, tier: str, tmp: Path) -> None:
        self.tier = tier
        self.tmp = tmp
        self.lib = build_shim(tmp) if tier == "ldpreload" else None

    def run(self, cmd: list, allow: set, timeout: float = 90.0):
        log = self.tmp / "watch.log"
        if log.exists():
            log.unlink()
        log.touch()
        if self.tier == "strace":
            wrapped = ["strace", "-f", "-q", "-s", "128",
                       "-e", "trace=connect,sendto,sendmsg",
                       "-o", str(log), "--"] + cmd
            env = harden_env()
        elif self.tier == "ldpreload" and self.lib:
            wrapped = cmd
            env = harden_env()
            env["LD_PRELOAD"] = str(self.lib)
            env["SIL_LOG"] = str(log)
        else:
            raise RuntimeError("detector tier unavailable")
        try:
            r = subprocess.run(wrapped, capture_output=True, text=True,
                               timeout=timeout, env=env)
            rc, out, err = r.returncode, r.stdout, r.stderr
        except subprocess.TimeoutExpired as e:
            def _d(x):
                if x is None:
                    return ""
                return x.decode(errors="replace") if isinstance(x, bytes) else x
            rc, out, err = -1, _d(e.stdout), _d(e.stderr)
        if self.tier == "strace":
            events, unparsed = parse_strace_log(log)
        else:
            events, unparsed = parse_shim_log(log)
        for u in unparsed:
            events.append({"event": "unparsed", "family": 2, "addr": None, "port": None})
        verdict = classify_all(events, allow)
        verdict["unparsed_raw"] = unparsed
        return rc, out, err, verdict


# ---------------------------------------------------------------------------
# «нарушитель» — негативный зонд
# ---------------------------------------------------------------------------
def make_probe(tmp: Path, port: int) -> tuple[str, list, list]:
    """(язык, команда-нарушитель, команда-loopback). rustc доступен —
    бинарник на Rust (тот же профиль связки, что у движка); иначе
    std-only Python-зонд."""
    rustc = shutil.which("rustc")
    if rustc:
        ext_src, clean_src = tmp / "probe_ext.rs", tmp / "probe_clean.rs"
        ext_bin, clean_bin = tmp / "probe_ext", tmp / "probe_clean"
        ext_src.write_text(RUST_PROBE_EXT, encoding="utf-8")
        clean_src.write_text(RUST_PROBE_CLEAN.replace("PORT_PLACEHOLDER", str(port)),
                             encoding="utf-8")
        ok = True
        for src, dst in ((ext_src, ext_bin), (clean_src, clean_bin)):
            r = subprocess.run([rustc, "-O", "-o", str(dst), str(src)],
                               capture_output=True, text=True, timeout=120)
            if r.returncode != 0:
                print(f"PROBE_BUILD=FAIL :: {r.stderr.strip()[:200]}")
                ok = False
        if ok:
            return "rust", [str(ext_bin)], [str(clean_bin)]
    ext = tmp / "probe_ext.py"
    clean = tmp / "probe_clean.py"
    ext.write_text(PY_PROBE_EXT, encoding="utf-8")
    clean.write_text(PY_PROBE_CLEAN.replace("PORT_PLACEHOLDER", str(port)), encoding="utf-8")
    return "python", [sys.executable, str(ext)], [sys.executable, str(clean)]


# ---------------------------------------------------------------------------
# сетевой namespace: строго loopback (best-effort, честные маркеры)
# ---------------------------------------------------------------------------
IFF_UP, IFF_RUNNING = 0x1, 0x40
SIOCGIFFLAGS, SIOCSIFFLAGS = 0x8913, 0x8914


def lo_up() -> bool:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        ifreq = struct.pack("16sH", b"lo", 0)
        res = fcntl.ioctl(s.fileno(), SIOCGIFFLAGS, ifreq)
        flags = struct.unpack("16sH", res)[1]
        if flags & IFF_UP:
            return True
        ifreq = struct.pack("16sH", b"lo", flags | IFF_UP | IFF_RUNNING)
        fcntl.ioctl(s.fileno(), SIOCSIFFLAGS, ifreq)
        return True
    except OSError:
        return False
    finally:
        s.close()


def netns_prefix() -> list | None:
    if shutil.which("unshare"):
        r = subprocess.run(["unshare", "--user", "--map-root-user", "--net", "true"],
                           capture_output=True, timeout=30)
        if r.returncode == 0:
            return ["unshare", "--user", "--map-root-user", "--net"]
    if shutil.which("sudo"):
        r = subprocess.run(["sudo", "-n", "unshare", "--net", "true"],
                           capture_output=True, timeout=30)
        if r.returncode == 0:
            return ["sudo", "-n", "unshare", "--net"]
    return None


# ---------------------------------------------------------------------------
# приёмки
# ---------------------------------------------------------------------------
def require_marker(out: str, *markers: str) -> bool:
    return all(m in out for m in markers)


def phase_loopback(w: Watched, bin_path: Path, allow: set, port: int) -> tuple[bool, str]:
    url = f"http://127.0.0.1:{port}/"
    cmd = [str(bin_path), "--fetch", url, "--dom", "--timeout-ms", "4000"]
    rc, out, err, v = w.run(cmd, allow)
    ok = (rc == 0
          and require_marker(out, "FETCH=OK", "HTTP_STATUS=200",
                             f"BODY_SHA256={PAGE_SHA}", "BROKER_DECISION=ALLOW")
          and v["VIOLATION"] == 0 and v["UNPARSED"] == 0
          and v["ALLOW"] >= 1)
    detail = (f"rc={rc} allow={v['ALLOW']} other={v['OTHER']} "
              f"violations={v['VIOLATION']} unparsed={v['UNPARSED']}")
    if not ok and v["list"]:
        detail += " :: " + "; ".join(
            f"{e.get('event')}->{e.get('addr')}:{e.get('port')}" for e in v["list"][:6])
    print(f"CONNECT_LOOPBACK={v['ALLOW']}")
    print(f"LOOPBACK_VIOLATIONS={v['VIOLATION'] + v['UNPARSED']}")
    print(f"LOOPBACK_FETCH={'PASS' if ok else 'FAIL'} :: {detail}")
    return ok, out


def phase_blocked_addr(w: Watched, bin_path: Path) -> tuple[bool, str]:
    """Негатив приёмки: реальный движок с ПУСТЫМ разрешённым списком
    тянется к известному адресу — попытка ОБЯЗАНА быть поймана."""
    cmd = [str(bin_path), "--fetch", f"http://{KNOWN_ADDR}:80/t.html",
           "--timeout-ms", "1500"]
    rc, out, err, v = w.run(cmd, allow=set())
    caught = (v["VIOLATION"] + v["UNPARSED"]) >= 1 and any(
        e.get("addr") == KNOWN_ADDR for e in v["list"])
    browser_ok = (rc in (4, 5) and ("FETCH=ERROR" in out or "FETCH=DENIED" in out))
    ok = caught and browser_ok
    detail = (f"rc={rc} caught={caught} violations={v['VIOLATION']} "
              f"unparsed={v['UNPARSED']} browser_refused={browser_ok}")
    print(f"BLOCKED_ADDR_RC={rc}")
    print(f"BLOCKED_ADDR={'CAUGHT' if caught else 'FAIL'} :: {detail}")
    return ok, out


def phase_probe_ext(w: Watched, probe_cmd: list) -> bool:
    rc, out, err, v = w.run(probe_cmd, allow=set(), timeout=60)
    caught = (v["VIOLATION"] + v["UNPARSED"]) >= 1
    print(f"LEAK_PROBE={'CAUGHT' if caught else 'FAIL'} :: "
          f"rc={rc} violations={v['VIOLATION']} unparsed={v['UNPARSED']}")
    return caught


def phase_probe_clean(w: Watched, probe_cmd: list, allow: set) -> bool:
    rc, out, err, v = w.run(probe_cmd, allow=allow, timeout=60)
    ok = rc == 0 and v["VIOLATION"] == 0 and v["UNPARSED"] == 0
    print(f"CLEAN_PROBE={'PASS' if ok else 'FAIL'} :: rc={rc} "
          f"violations={v['VIOLATION']}")
    return ok


# ---------------------------------------------------------------------------
# режимы запуска
# ---------------------------------------------------------------------------
def certificate(bin_path: Path, require_netns: bool) -> int:
    print(f"CERT_BIN={'PASS' if bin_path.is_file() else 'FAIL'} :: {bin_path}")
    if not bin_path.is_file():
        print("SILENCE_E2E=FAIL :: бинарник не найден (сначала cargo build --locked)")
        return 1
    tier = detect_tier()
    print(f"DETECTOR={tier}")
    if tier == "none":
        print("SILENCE_E2E=NOT_ISSUED :: нет ни strace, ни cc — "
              "сертификат не выдаётся (fail-closed), приёмка переносится в CI")
        return 2
    failures: list[str] = []
    with tempfile.TemporaryDirectory(prefix="silence-e2e-") as td:
        tmp = Path(td)
        w = Watched(tier, tmp)
        srv, port = start_server()
        try:
            allow = {("127.0.0.1", port)}
            # 1. двухсторонняя приёмка детектора
            lang, probe_ext, probe_clean = make_probe(tmp, port)
            print(f"PROBE_LANG={lang}")
            if not phase_probe_ext(w, probe_ext):
                failures.append("LEAK_PROBE: нарушитель не пойман — детектор порочен")
            if not phase_probe_clean(w, probe_clean, allow):
                failures.append("CLEAN_PROBE: чистый loopback признан нарушением")
            # 2. зелёный прогон: ровно один loopback fetch, ноль внеполосных
            ok, _ = phase_loopback(w, bin_path, allow, port)
            if not ok:
                failures.append("LOOPBACK_FETCH: зелёный прогон не зелёный или есть внеполосные попытки")
            # 3. негатив: браузер тянется к известному адресу с пустым списком
            ok, _ = phase_blocked_addr(w, bin_path)
            if not ok:
                failures.append("BLOCKED_ADDR: попытка к известному адресу НЕ поймана")
            # 4. строгий netns (best-effort, честно)
            pref = netns_prefix()
            print(f"NETNS={'ON' if pref else 'OFF'}")
            if pref:
                for tag, expect in (("NETNS_GREEN", "INNER_GREEN=PASS"),
                                    ("NETNS_BLOCKED", "INNER_BLOCKED=CAUGHT")):
                    r = subprocess.run(
                        pref + [sys.executable, str(TOOL), "--inner", "--bin",
                                str(bin_path), "--detector", tier,
                                "--check", tag.split("_")[1].lower()],
                        capture_output=True, text=True, timeout=300)
                    print(r.stdout.strip())
                    if expect not in r.stdout or r.returncode != 0:
                        failures.append(f"{tag}: строгий netns прогон не прошёл")
            elif require_netns:
                failures.append("NETNS: недоступен, а приёмка требует строгий netns")
            else:
                print("NETNS_INNER=SKIPPED :: netns недоступен (детектор остаётся гейтом)")
        finally:
            stop_server(srv)
    if failures:
        for f in failures:
            print(f"SILENCE_E2E_NEG=FAIL :: {f}")
        print("SILENCE_E2E=FAIL")
        return 1
    print("SILENCE_E2E=PASS (ноль внеполосных попыток; негатив пойман)")
    return 0


def inner_run(bin_path: Path, tier: str, check: str) -> int:
    """Внутри netns: поднять lo, повторить приёмку. Используется только
    как дочерний процесс certificate()."""
    if not lo_up():
        print(f"INNER_{check.upper()}=FAIL :: не поднять lo")
        return 1
    if tier == "none":
        print(f"INNER_{check.upper()}=FAIL :: нет детектора")
        return 1
    with tempfile.TemporaryDirectory(prefix="silence-inner-") as td:
        tmp = Path(td)
        w = Watched(tier, tmp)
        srv, port = start_server()
        try:
            allow = {("127.0.0.1", port)}
            if check == "green":
                ok, _ = phase_loopback(w, bin_path, allow, port)
                print(f"INNER_GREEN={'PASS' if ok else 'FAIL'}")
                return 0 if ok else 1
            ok, _ = phase_blocked_addr(w, bin_path)
            print(f"INNER_BLOCKED={'CAUGHT' if ok else 'FAIL'}")
            return 0 if ok else 1
        finally:
            stop_server(srv)


def selftest() -> int:
    failures: list[str] = []
    print(f"DETECTOR={detect_tier()}")
    # 1. парсер strace
    for line, expect in STRACE_SAMPLES:
        r = _parse_strace_line(line)
        if expect == "OTHER":
            if r != "OTHER":
                failures.append(f"parser: {line[:60]} -> {r!r} != OTHER")
        elif expect == "UNPARSED":
            if r != "UNPARSED":
                failures.append(f"parser: {line[:60]} -> {r!r} != UNPARSED")
        else:
            if not isinstance(r, dict):
                failures.append(f"parser: {line[:60]} -> {r!r} != event")
                continue
            exp_event, exp_fam, exp_addr, exp_port = expect
            fam, addr = _norm_addr(r["family"], r["addr"])
            if (r["event"], fam, addr, r["port"]) != expect:
                failures.append(f"parser: {line[:60]} -> "
                                f"{(r['event'], fam, addr, r['port'])} != {expect}")
    print("PARSER=" + ("PASS" if not failures else "FAIL"))
    # 2. классификатор
    cls_cases = [
        ({"event": "connect", "family": 2, "addr": "127.0.0.1", "port": 9},
         {("127.0.0.1", 9)}, "ALLOW"),
        ({"event": "connect", "family": 2, "addr": "127.0.0.1", "port": 9}, set(), "VIOLATION"),
        ({"event": "connect", "family": 2, "addr": "127.1.2.3", "port": 1},
         {("127.1.2.3", 1)}, "ALLOW"),
        ({"event": "connect", "family": 10, "addr": "::1", "port": 5}, set(), "ALLOW"),
        ({"event": "connect", "family": 10, "addr": "fe80::1", "port": 5}, set(), "VIOLATION"),
        ({"event": "connect", "family": 10, "addr": "::ffff:192.0.2.9", "port": 5},
         set(), "VIOLATION"),
        ({"event": "connect", "family": 1, "addr": None, "port": None}, set(), "OTHER"),
        ({"event": "connect", "family": 16, "addr": None, "port": None}, set(), "OTHER"),
        ({"event": "connect", "family": 2, "addr": None, "port": None}, set(), "UNPARSED"),
    ]
    for ev, allow, expect in cls_cases:
        got = classify_event(ev, allow)
        if got != expect:
            failures.append(f"classifier: {ev} -> {got} != {expect}")
    print("CLASSIFIER=" + ("PASS" if not any(f.startswith('classifier') for f in failures) else "FAIL"))
    # 3. живой детектор (двухсторонний негатив + shim/strace живьём)
    tier = detect_tier()
    if tier == "none" and os.environ.get("SIL_E2E_ALLOW_NO_DETECTOR") != "1":
        print("DETECTOR_SELFTEST=SKIPPED :: нет ни strace, ни cc (fail-closed)")
        failures.append("нет детектора в окружении")
    else:
        with tempfile.TemporaryDirectory(prefix="silence-selftest-") as td:
            tmp = Path(td)
            if tier == "ldpreload":
                if not build_shim(tmp):
                    failures.append("шим не собрался")
                    print("SHIM_BUILD=FAIL")
                else:
                    print("SHIM_BUILD=PASS")
            srv, port = start_server()
            try:
                w = Watched(tier, tmp)
                lang, probe_ext, probe_clean = make_probe(tmp, port)
                print(f"PROBE_LANG={lang}")
                if not phase_probe_ext(w, probe_ext):
                    failures.append("LEAK_PROBE: нарушитель не пойман")
                if not phase_probe_clean(w, probe_clean, {("127.0.0.1", port)}):
                    failures.append("CLEAN_PROBE: loopback признан нарушением")
            finally:
                stop_server(srv)
    print("DETECTOR_SELFTEST=" + ("PASS" if not failures else "FAIL"))
    if failures:
        for f in failures:
            print(f"SELFTEST_NEG=FAIL :: {f}")
        print("SELFTEST=FAIL")
        return 1
    print("SELFTEST=PASS (детектор ловит нарушителя и пропускает loopback)")
    return 0


def watch(allow: set, cmd: list) -> int:
    if not cmd:
        print("WATCH=FAIL :: нет команды после --")
        return 2
    tier = detect_tier()
    print(f"DETECTOR={tier}")
    if tier == "none":
        print("WATCH=NOT_ISSUED")
        return 2
    with tempfile.TemporaryDirectory(prefix="silence-watch-") as td:
        w = Watched(tier, Path(td))
        rc, out, err, v = w.run(cmd, allow)
        print(out, end="")
        print(f"WATCH_RC={rc}")
        print(f"WATCH_ALLOWED={v['ALLOW']} WATCH_OTHER={v['OTHER']} "
              f"WATCH_VIOLATIONS={v['VIOLATION'] + v['UNPARSED']}")
        for e in v["list"][:10]:
            print(f"VIOLATION :: {e.get('event')} -> {e.get('addr')}:{e.get('port')}")
        print("WATCH=" + ("PASS" if v["VIOLATION"] == 0 and v["UNPARSED"] == 0 else "FAIL"))
        return 0 if (v["VIOLATION"] == 0 and v["UNPARSED"] == 0) else 1


def parse_allow(pairs: list[str]) -> set:
    out = set()
    for p in pairs:
        host, _, port = p.rpartition(":")
        out.add((host, int(port) if port else None))
    return out


def main() -> int:
    argv = sys.argv[1:]
    if "--selftest" in argv or "--self-test" in argv:
        return selftest()
    if "--inner" in argv:
        def val(name: str) -> str | None:
            i = argv.index(name)
            return argv[i + 1] if i + 1 < len(argv) else None
        b, tier, check = val("--bin"), val("--detector") or detect_tier(), val("--check") or "green"
        if not b:
            print("INNER_USAGE_ERROR")
            return 2
        return inner_run(Path(b), tier, check)
    if "--watch" in argv:
        i = argv.index("--watch")
        allows, j = [], i + 1
        while j < len(argv) and argv[j] == "--allow":
            allows.append(argv[j + 1])
            j += 2
        if j >= len(argv) or argv[j] != "--":
            print("WATCH_USAGE_ERROR :: ожидается -- CMD...")
            return 2
        return watch(parse_allow(allows), argv[j + 1:])
    bin_path = Path("./target/debug/neutral-browser")
    if "--certificate" in argv:
        i = argv.index("--certificate")
        if i + 1 < len(argv) and not argv[i + 1].startswith("--"):
            bin_path = Path(argv[i + 1])
    return certificate(bin_path, "--require-netns" in argv)


if __name__ == "__main__":
    sys.exit(main())
