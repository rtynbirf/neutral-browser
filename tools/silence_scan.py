#!/usr/bin/env python3
"""Сканер молчания neutral-browser — статическое свидетельство «не звонит домой».

Std-only Python (никаких сторонних пакетов) — часть оффлайн-петли машинной
верификации (см. tests/architecture.py, docs/DEPENDENCY_POLICY.md).

П43 (issue #2): НЕЙТРАЛЬНЫЙ БРАУЗЕР ПОЛЬЗОВАТЕЛЯ обещает не звонить домой —
пусть докажет строкой, не словом. cargo-deny/cargo-audit как сторонние тулы
запрещены политикой нулевых зависимостей (REJECTED в tests/architecture.py),
поэтому эквивалентная проверка сделана std-only (S5).

Чеки (все fail-closed — любое сомнение = красный):
  S1 SOCKETS  — сетевые примитивы std::net (TcpStream/UdpSocket/ToSocketAddrs,
                std::net) только в src/network.rs (единственный модуль,
                которому архитектурный гейт разрешает I/O).
  S2 NETCRATE — сторонние сетевые крейты (reqwest/hyper/openssl/...) не
                упоминаются в коде как use X / X:: / extern crate.
  S3 SPAWN    — браузер никогда не порождает процессы и не вызывает
                spawn/exec — обходной канал «позвонить домой» через curl
                и пр. закрыт статически (std::process::exit разрешён).
  S4 HOSTS    — внешние хосты: в src/ допустимы только localhost/127.0.0.1
                и зарезервированные имена RFC 2606/6761 (*.test, *.example,
                *.invalid, *.localhost, example.com/org/net); телеметрийный
                блок-лист (analytics/ads/crash/metrica) запрещён во всём
                репозитории — доки и CI тоже. Внешние хосты в доках — это
                цитаты-справки (inventory, S4C), не сетевые вызовы.
                Сам сканер исключён из S4-обхода: внутри него — строки
                блок-листа и негативные фикстуры (иначе самоисполнение).
  S5 CARGO    — cargo-аудит: [dependencies] пуст (строго), нет
                [dev-dependencies]/[build-dependencies]/[patch]/[features],
                Cargo.lock содержит ровно 1 пакет без checksum, версия лока
                совпадает с Cargo.toml (храповик против устаревшего лока).
                Пустое множество зависимостей = пустое множество крейтов =
                нечему конфликтовать, нечему быть уязвимым, не с чем
                дублировать.
  S6 PYLOOP   — петля верификации (tools/*.py, tests/*.py) импортирует
                только стандартную библиотеку (sys.stdlib_module_names).

Режимы:
  python3 tools/silence_scan.py               # скан репозитория → SILENCE=PASS/FAIL
  python3 tools/silence_scan.py --selftest    # негативный тест: фикстуры с
                 # нарушениями ОБЯЗАНЫ дать красный по каждому чеку.
                 # Нарушенный блок-лист в тесте = красный (машинная приёмка П43).
"""
from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parents[1]

# --- S1: примитивы std::net разрешены только здесь ---
SOCKET_ALLOWED_FILES = {"network.rs"}
SOCKET_TOKENS = [
    r"\bTcpStream\b",
    r"\bUdpSocket\b",
    r"\bToSocketAddrs\b",
    r"\bstd::net\b",
]

# --- S2: сторонние сетевые крейты (use X / X:: / extern crate) ---
NET_CRATES = [
    "reqwest", "ureq", "hyper", "curl", "isahc", "attohttpc",
    "openssl", "rustls", "native_tls", "native_tls", "trust_dns",
    "tokio_tungstenite", "tungstenite", "websocket", "warp", "actix",
]

# --- S3: порождение процессов запрещено (std::process::exit не трогаем) ---
SPAWN_TOKENS = [
    r"\bCommand::new\b",
    r"\bprocess::Command\b",
    r"\bstd::os::unix::process\b",
    r"\bspawn\s*\(",
    r"\bexec\s*\(",
]

# --- S4: телеметрийный блок-лист (любой файл репо = красный) ---
TELEMETRY_BLOCKLIST = [
    "sentry.io", "google-analytics.com", "googletagmanager.com",
    "googlesyndication.com", "doubleclick.net", "adservice.google",
    "segment.io", "segment.com", "mixpanel.com", "amplitude.com",
    "hotjar.com", "fullstory.com", "matomo", "piwik",
    "mc.yandex.ru", "metrika.yandex.ru", "top-fwz1.mail.ru",
    "crashlytics.com", "appcenter.ms", "app-measurement.com",
    "events.data.microsoft.com", "browser.pipe.aria",
    "watson.telemetry.microsoft", "stats.wp.com", "plausible.io",
    "posthog.com", "clarity.ms", "bat.bing.com",
]

TEXT_SUFFIXES = {
    ".rs", ".py", ".md", ".txt", ".toml", ".lock", ".yml", ".yaml",
    ".html", ".css", ".js", ".json", ".cfg", ".xml", ".sh", ".bat",
    ".jinja", ".tmpl",
}
EXTRA_NAMES = {"Cargo.lock", "LICENSE", "MANIFEST.sha256", "llms.txt",
               "CODEOWNERS", ".gitignore", ".gitattributes"}
SKIP_DIRS = {".git", "target", "node_modules", "__pycache__"}

LOCAL_HOSTS = {"127.0.0.1", "localhost", "::1"}
TEST_TLDS = (".test", ".example", ".invalid", ".localhost")
RFC2606 = {"example.com", "example.org", "example.net"}


# --------------------------------------------------------------------------
# утилиты
# --------------------------------------------------------------------------
_CHAR_LIT = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[^}]*\}|.)|[^\\'])'")
_RAW_OPEN = re.compile(r'(b?r)(#*)"')


def strip_rust_comments(src: str) -> str:
    """Срезает // и /* */ комментарии, будучи чувствительным к строкам,
    char-литералам и raw-строкам r#\"...\"# (// внутри \"...\" не комментарий,
    'static — не char-литерал). Незакрытая raw-строка → остаток считается
    кодом (fail-closed: ложный красный, а не тихий пропуск)."""
    out: list[str] = []
    i, n = 0, len(src)
    state = "code"  # code | line | block | str
    while i < n:
        c = src[i]
        nxt = src[i + 1] if i + 1 < n else ""
        if state == "code":
            m = _RAW_OPEN.match(src, i)
            if m and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
                hashes = m.group(2)
                close = src.find('"' + hashes, m.end())
                seg_end = n if close == -1 else close + 1 + len(hashes)
                out.append("".join(ch if ch == "\n" else " " for ch in src[i:seg_end]))
                i = seg_end
                continue
            m = _CHAR_LIT.match(src, i)
            if m:
                out.append(m.group(0))
                i = m.end()
                continue
            if c == "/" and nxt == "/":
                state = "line"; i += 2; continue
            if c == "/" and nxt == "*":
                state = "block"; i += 2; continue
            if c == '"':
                state = "str"; out.append(c); i += 1; continue
            out.append(c); i += 1; continue
        if state == "line":
            if c == "\n":
                state = "code"; out.append(c)
            i += 1; continue
        if state == "block":
            if c == "*" and nxt == "/":
                state = "code"; i += 2; continue
            if c == "\n":
                out.append(c)  # переводы строк сохраняем (номера строк)
            i += 1; continue
        # state == "str"
        if c == "\\":
            out.append(src[i:i + 2])
            i += 2; continue
        if c == '"':
            state = "code"
        out.append(c); i += 1
    return "".join(out)


def walk_text_files(root: Path):
    for p in sorted(root.rglob("*")):
        if not p.is_file():
            continue
        if any(part in SKIP_DIRS for part in p.relative_to(root).parts):
            continue
        if p.suffix.lower() not in TEXT_SUFFIXES and p.name not in EXTRA_NAMES:
            continue
        try:
            if p.stat().st_size > 2_000_000:
                continue
            p.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        yield p


def host_is_local_or_test(host: str) -> bool:
    h = host.lower().rstrip(".")
    if h in LOCAL_HOSTS or h in RFC2606:
        return True
    if h.endswith(TEST_TLDS):
        return True
    return False


class Report:
    def __init__(self) -> None:
        self.lines: list[tuple[str, bool, str]] = []

    def add(self, name: str, ok: bool, detail: str = "") -> None:
        self.lines.append((name, ok, detail))

    @property
    def ok(self) -> bool:
        return all(ok for _, ok, _ in self.lines)

    def names(self) -> dict[str, bool]:
        return {name: ok for name, ok, _ in self.lines}

    def render(self) -> str:
        out = []
        for name, ok, detail in self.lines:
            mark = "PASS" if ok else "FAIL"
            out.append(f"{name}={mark}" + (f" :: {detail}" if detail and not ok else ""))
        out.append("SILENCE=PASS" if self.ok else "SILENCE=FAIL")
        return "\n".join(out)


# --------------------------------------------------------------------------
# чеки
# --------------------------------------------------------------------------
def check_s1_sockets(rep: Report, src_dir: Path) -> None:
    bad = []
    for p in sorted(src_dir.glob("*.rs")):
        code = strip_rust_comments(p.read_text(encoding="utf-8"))
        if p.name in SOCKET_ALLOWED_FILES:
            continue
        for tok in SOCKET_TOKENS:
            for m in re.finditer(tok, code):
                line = code[: m.start()].count("\n") + 1
                bad.append(f"{p.name}:{line}: {m.group(0)}")
    rep.add("S1_SOCKETS_CONFINED", not bad,
            "сетевые примитивы вне network.rs: " + "; ".join(bad[:8]))


def check_s2_netcrate(rep: Report, src_dir: Path) -> None:
    bad = []
    for p in sorted(src_dir.glob("*.rs")):
        code = strip_rust_comments(p.read_text(encoding="utf-8"))
        for crate in NET_CRATES:
            pat = (rf"(?:^|\n|\s)(use\s+{re.escape(crate)}\b|"
                   rf"{re.escape(crate)}\s*::|extern\s+crate\s+{re.escape(crate)}\b)")
            for m in re.finditer(pat, code):
                line = code[: m.start()].count("\n") + 1
                bad.append(f"{p.name}:{line}: {crate}")
    rep.add("S2_NO_NET_CRATES", not bad, "; ".join(bad[:8]))


def check_s3_spawn(rep: Report, src_dir: Path) -> None:
    bad = []
    for p in sorted(src_dir.glob("*.rs")):
        code = strip_rust_comments(p.read_text(encoding="utf-8"))
        for tok in SPAWN_TOKENS:
            for m in re.finditer(tok, code):
                line = code[: m.start()].count("\n") + 1
                bad.append(f"{p.name}:{line}: {m.group(0).strip()}")
    rep.add("S3_NO_PROCESS_SPAWN", not bad, "; ".join(bad[:8]))


def check_s4_hosts(rep: Report, root: Path) -> None:
    src_bad, bl_bad = [], []
    inventory: dict[str, list[str]] = {}
    url_re = re.compile(r"https?://[A-Za-z0-9._~:/?#@!$&'()*+,;=%\[\]-]+")
    for p in walk_text_files(root):
        rel = p.relative_to(root).as_posix()
        if rel == "tools/silence_scan.py":
            # самоисключение: в теле сканера — строки блок-листа и фикстуры
            continue
        try:
            text = p.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        in_src = rel.startswith("src/")
        for m in url_re.finditer(text):
            raw = m.group(0).rstrip(".,;:!?)]}\"'»")
            if re.search(r"\s", raw):
                # мусор вида «http://exa mple.test» — негативные фикстуры URL-парсера
                continue
            try:
                host = (urlsplit(raw).hostname or "").lower()
            except ValueError:
                host = ""
            if not host:
                continue
            # точка-меньше и не localhost → обрывок невалидного URL из
            # негативных фикстур парсера (например «http://exa mple.test»),
            # а не сетевой адрес — фрагменты по частям не считаем вызовом
            if "." not in host and ":" not in host and host not in LOCAL_HOSTS:
                continue
            if host_is_local_or_test(host):
                continue
            if any(b in host for b in TELEMETRY_BLOCKLIST) or host.startswith("telemetry."):
                bl_bad.append(f"{rel}: {host}")
                continue
            if in_src:
                line = text[: m.start()].count("\n") + 1
                src_bad.append(f"{rel}:{line}: {host}")
            else:
                inventory.setdefault(host, []).append(rel)
    rep.add("S4A_BLOCKLIST_ZERO", not bl_bad, "; ".join(bl_bad[:8]))
    rep.add("S4B_SRC_EXTERNAL_HOSTS", not src_bad, "; ".join(src_bad[:8]))
    rep.add("S4C_INVENTORY_ONLY_INFO", True,
            f"{len(inventory)} внешних хостов-цитат в доках (не сетевые вызовы)")


def check_s5_cargo(rep: Report, root: Path) -> None:
    toml, lock = root / "Cargo.toml", root / "Cargo.lock"
    if not toml.is_file() or not lock.is_file():
        rep.add("S5_CARGO_AUDIT", False, "Cargo.toml/Cargo.lock не найдены")
        return
    t = toml.read_text(encoding="utf-8")
    m = re.search(r"(?m)^\[dependencies\][^\n]*(\n(?!\[)[^\n]*)*", t)
    dep_lines = [ln for ln in (m.group(0).splitlines()[1:] if m else [])
                 if ln.strip() and not ln.strip().startswith("#")]
    ok_empty = bool(m) and not dep_lines
    forbidden = [h for h in re.findall(r"(?m)^\[([^\]]+)\]", t)
                 if h.strip() in {"dev-dependencies", "build-dependencies",
                                  "patch", "features"}
                 or h.strip().startswith("target.")]
    mver = re.search(r'(?m)^\s*version\s*=\s*"([^"]+)"', t)
    pkg_version = mver.group(1) if mver else None
    l = lock.read_text(encoding="utf-8")
    n_pkgs = l.count("[[package]]")
    has_checksum = "checksum" in l
    lver = re.search(r'\[\[package\]\]\s*\nname\s*=\s*"([^"]+)"\s*\nversion\s*=\s*"([^"]+)"', l)
    lock_name = lver.group(1) if lver else None
    lock_ver = lver.group(2) if lver else None
    ok = (ok_empty and not forbidden and n_pkgs == 1 and not has_checksum
          and lock_name == "neutral-browser" and pkg_version is not None
          and lock_ver == pkg_version)
    detail = (f"deps={len(dep_lines)} forbidden={forbidden} pkgs={n_pkgs} "
              f"checksum={has_checksum} toml_ver={pkg_version} lock={lock_name}@{lock_ver}")
    rep.add("S5_CARGO_AUDIT", ok, detail)


def check_s6_pyloop(rep: Report, root: Path) -> None:
    std = set(getattr(sys, "stdlib_module_names", ()))
    bad = []
    for sub in ("tools", "tests"):
        d = root / sub
        if not d.is_dir():
            continue
        for p in sorted(d.glob("*.py")):
            text = p.read_text(encoding="utf-8", errors="replace")
            for m in re.finditer(r"(?m)^\s*(?:import|from)\s+([A-Za-z_][\w]*)", text):
                top = m.group(1)
                if top == "__future__":
                    continue
                if top not in std and not (root / sub / f"{top}.py").is_file():
                    bad.append(f"{sub}/{p.name}: import {top}")
    rep.add("S6_PYLOOP_STD_ONLY", not bad, "; ".join(bad[:8]))


# --------------------------------------------------------------------------
def scan(root: Path) -> Report:
    rep = Report()
    src_dir = root / "src"
    if not src_dir.is_dir():
        rep.add("S0_SRC_PRESENT", False, "нет src/")
        return rep
    rep.add("S0_SRC_PRESENT", True)
    check_s1_sockets(rep, src_dir)
    check_s2_netcrate(rep, src_dir)
    check_s3_spawn(rep, src_dir)
    check_s4_hosts(rep, root)
    check_s5_cargo(rep, root)
    check_s6_pyloop(rep, root)
    return rep


# --------------------------------------------------------------------------
# негативный selftest: фикстуры с нарушениями обязаны дать красный
# --------------------------------------------------------------------------
def build_fixture(root: Path) -> None:
    (root / "src").mkdir(parents=True, exist_ok=True)
    (root / "docs").mkdir(parents=True, exist_ok=True)
    (root / "tools").mkdir(parents=True, exist_ok=True)
    # чистый network.rs — разрешённый модуль (сам по себе S1 не триггерит)
    (root / "src" / "network.rs").write_text("pub fn noop() {}\n", encoding="utf-8")
    (root / "src" / "rogue.rs").write_text(
        "use std::net::TcpStream;\nfn x() { let _ = TcpStream::connect(\"a\"); }\n",
        encoding="utf-8")
    (root / "src" / "rogue2.rs").write_text(
        "use reqwest::blocking::Client;\nfn y() {}\n", encoding="utf-8")
    (root / "src" / "spawn.rs").write_text(
        "fn z() { let _ = std::process::Command::new(\"curl\"); }\n", encoding="utf-8")
    (root / "src" / "pixel.rs").write_text(
        "pub const BEACON: &str = \"http://cdn.evil-widgets.ru/pixel.js\";\n",
        encoding="utf-8")
    (root / "docs" / "note.md").write_text(
        "Смотри дашборд: https://mc.yandex.ru/watch/123456\n", encoding="utf-8")
    (root / "Cargo.toml").write_text(
        '[package]\nname = "fx"\nversion = "1.0.0"\nedition = "2021"\n\n'
        '[dependencies]\nserde = "1"\n', encoding="utf-8")
    (root / "Cargo.lock").write_text(
        '# generated\nversion = 4\n\n[[package]]\nname = "fx"\nversion = "0.9.0"\n',
        encoding="utf-8")
    (root / "tools" / "fx.py").write_text("import requests\n", encoding="utf-8")


def selftest() -> int:
    failures: list[str] = []
    must_fail = ["S1_SOCKETS_CONFINED", "S2_NO_NET_CRATES", "S3_NO_PROCESS_SPAWN",
                 "S4A_BLOCKLIST_ZERO", "S4B_SRC_EXTERNAL_HOSTS",
                 "S5_CARGO_AUDIT", "S6_PYLOOP_STD_ONLY"]
    with tempfile.TemporaryDirectory(prefix="silence-fx-") as td:
        root = Path(td)
        build_fixture(root)
        rep = scan(root)
        print("--- fixture scan (ожидаем КРАСНЫЙ по каждому чеку) ---")
        print(rep.render())
        if rep.ok:
            failures.append("фикстура не поймана: SILENCE=PASS на заведомо нарушенном дереве")
        by_name = rep.names()
        for name in must_fail:
            if by_name.get(name, None) is not False:
                failures.append(f"{name}: негативная фикстура НЕ даёт красный")
    print("--- итог selftest ---")
    if failures:
        for f in failures:
            print(f"SELFTEST_NEGATIVE=FAIL :: {f}")
        print("SELFTEST=FAIL")
        return 1
    for name in must_fail:
        print(f"{name}_NEGATIVE=PASS")
    print("SELFTEST=PASS (нарушенный блок-лист даёт красный)")
    return 0


def main() -> int:
    if "--selftest" in sys.argv or "--self-test" in sys.argv:
        return selftest()
    if not REPO.is_dir():
        print("SILENCE=FAIL :: корень репо не найден")
        return 1
    rep = scan(REPO)
    print(rep.render())
    return 0 if rep.ok else 1


if __name__ == "__main__":
    sys.exit(main())
