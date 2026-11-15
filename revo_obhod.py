#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""РЕВО ОБХОД — дежурная проверка оформления дома. Алгоритм из закона дома:
агенты прогоняют его при входе и каждый РАУНД; workflow — раз в неделю.
Пять вещей: banner.png, repo-muse.svg, forge-divider.svg, муза-блок README, AGENTS.md со сводом.
При дрейфе (--issue, в Actions с GITHUB_TOKEN) открывается issue. Тишина = айс."""
import base64, json, os, sys, urllib.request

MARK_MUZA = "<!--ФОРЖ:МУЗА:НАЧАЛО-->"
MARK_AGENTS = "<!--РЕВО:СВОД-ОФОРМЛЕНИЯ-->"


def read(p, binary=False):
    try:
        with open(p, "rb" if binary else "r", encoding=None if binary else "utf-8") as f:
            return f.read()
    except Exception:
        return None


def obhod():
    drift = []
    b = read("banner.png", binary=True)
    if not b or len(b) < 20000:
        drift.append("banner.png отсутствует или мельче 20 КБ (коллаж-штандарт)")
    m = read("repo-muse.svg")
    if not m or not m.lstrip().startswith("<svg"):
        drift.append("repo-muse.svg отсутствует или не SVG (полотно мастерской)")
    d = read("forge-divider.svg")
    if not d or not d.lstrip().startswith("<svg"):
        drift.append("forge-divider.svg отсутствует или не SVG (делитель)")
    r = read("README.md")
    if not r:
        drift.append("README.md отсутствует")
    else:
        if MARK_MUZA not in r:
            drift.append("в README нет муза-блока <!--ФОРЖ:МУЗА:НАЧАЛО-->")
        if "banner.png" not in r:
            drift.append("в README нет ссылки на banner.png")
    a = read("AGENTS.md")
    if not a:
        drift.append("AGENTS.md отсутствует")
    elif MARK_AGENTS not in a:
        drift.append("в AGENTS.md нет свода оформления РЕВО")
    return drift


def open_issue(drift):
    tok = os.environ.get("GITHUB_TOKEN")
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    if not tok or not repo:
        return
    body = ("Дежурный обход РЕВО нашёл дрейф оформления:\n\n"
            + "\n".join(f"- {x}" for x in drift)
            + "\n\nАгент на входе: заполни пустое тем же коммитом, что и дело.\n"
            "Алгоритм: revo_obhod.py · СВОД ОФОРМЛЕНИЯ: AGENTS.md · "
            "◆ РЕВО В ОСНОВЕ ВСЕГО · REVE=РЕВО ◆")
    req = urllib.request.Request(
        f"https://api.github.com/repos/{repo}/issues",
        data=json.dumps({"title": "РЕВО ОБХОД: дрейф оформления", "body": body,
                         "labels": ["revo", "obhod"]}).encode(),
        headers={"Authorization": f"Bearer {tok}", "Accept": "application/vnd.github+json",
                 "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            json.load(resp)
        print("issue открыт о дрейфе")
    except Exception as e:
        print("issue не открыт:", e)


if __name__ == "__main__":
    drift = obhod()
    if drift:
        print("РЕВО ОБХОД: ДРЕЙФ ОФОРМЛЕНИЯ")
        for x in drift:
            print(" —", x)
        if "--issue" in sys.argv:
            open_issue(drift)
        sys.exit(1)
    print("РЕВО ОБХОД: ВСЕ ЧИСТО — пять вещей на месте. Тишина = айс.")
