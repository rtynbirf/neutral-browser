#!/usr/bin/env python3
"""Artifact FREEZE manifest for neutral-browser.

Harvested from the ФУНДАМЕНТ.md review (see session worklog, Task 55): the
author's machine-verifiable core is `FREEZE = hash-of-bytes` plus an
append-only commitment log. The author's fundamental error — inventing
CommitTime / claiming in-system anchors prove history — is NOT repeated
here: this manifest makes NO claims about when or by whom a freeze
happened. It only states one thing, verifiable right now:

    the bytes of every frozen artifact equal the hash of its LATEST freeze
    line, and the log is append-only (enforced by git history + the gate).

Commands:
    freeze <tag> <file> [<file>...]   append lines: <tag> <sha256> <path>
    verify                            every frozen path's CURRENT bytes must
                                      equal the LATEST hash recorded for it;
                                      print per-path verdict, exit 1 on any
                                      CHANGED/MISSING.

Honest semantics:
  - a deliberate stage change to a frozen file = re-freeze with the new
    stage tag (the git diff shows the conscious move, never silence);
  - the log is append-only by construction: freeze() never rewrites or
    deletes lines, it only appends.
"""
import hashlib
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "MANIFEST.sha256"


def sha256_file(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def freeze(tag: str, files: list[str]) -> int:
    if MANIFEST.exists():
        for line in MANIFEST.read_text(encoding="utf-8").splitlines():
            if line and len(line.split()) != 3:
                print(f"MANIFEST_CORRUPT line does not have 3 fields: {line!r}")
                return 1
    lines = []
    for f in files:
        p = Path(f) if Path(f).is_absolute() else ROOT / f
        if not p.is_file():
            print(f"FREEZE_ERROR missing: {f}")
            return 1
        lines.append(f"{tag} {sha256_file(p)} {Path(f).as_posix()}")
    with MANIFEST.open("a", encoding="utf-8") as fh:
        for ln in lines:
            fh.write(ln + "\n")
            print(f"MANIFEST_FROZEN {ln}")
    return 0


def verify() -> int:
    if not MANIFEST.exists():
        print("MANIFEST_VERIFY=FAIL manifest missing")
        return 1
    latest: dict[str, str] = {}
    total = 0
    for line in MANIFEST.read_text(encoding="utf-8").splitlines():
        if not line:
            continue
        parts = line.split()
        if len(parts) != 3:
            print(f"MANIFEST_VERIFY=FAIL corrupt line {line!r}")
            return 1
        tag, digest, rel = parts
        total += 1
        latest[rel] = tag  # last occurrence wins (append-only history)
        latest[rel + "::hash"] = digest
    bad = 0
    for rel, tag in sorted(latest.items()):
        if rel.endswith("::hash"):
            continue
        want = latest[rel + "::hash"]
        p = ROOT / rel
        if not p.is_file():
            print(f"MANIFEST_MISSING {rel}")
            bad += 1
        else:
            have = sha256_file(p)
            verdict = "FROZEN" if have == want else "CHANGED"
            print(f"MANIFEST_{verdict} {rel} ({tag})")
            if verdict != "FROZEN":
                bad += 1
    n_frozen = len([k for k in latest if not k.endswith("::hash")])
    print(f"MANIFEST_VERIFY={'PASS' if bad == 0 else 'FAIL'} "
          f"({total} lines, {n_frozen} artifacts)")
    return 1 if bad else 0


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    if args[0] == "freeze" and len(args) >= 3:
        return freeze(args[1], args[2:])
    if args[0] == "verify" and len(args) == 1:
        return verify()
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
