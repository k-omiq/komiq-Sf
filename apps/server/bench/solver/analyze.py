"""Summarise out/results.jsonl: per solver, how many sites it cleared in the browser,
how many earned cf_clearance, and how many replays passed from the JVM (what
Suwayomi would see). Usage: python3 analyze.py results.jsonl
"""
import json
import sys
from collections import defaultdict

rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip().startswith("{")]
by = defaultdict(list)
for r in rows:
    by[f"{r.get('solver')}/{r.get('route', 'direct')}"].append(r)


def passed(replay):
    return bool(replay) and replay.split()[0] == "200" and replay.endswith("none")


print(f"{'solver/route':22} {'sites':>5} {'page ok':>8} {'cf_clear':>8} {'jvm ok':>7} {'curl ok':>8} {'p50 ms':>7}")
for solver, rs in sorted(by.items()):
    page = sum(1 for r in rs if r.get("ok") and not r.get("afterPresent"))
    clear = sum(1 for r in rs if r.get("cfClearance"))
    jvm = sum(1 for r in rs if passed(r.get("replayJvm")))
    curl = sum(1 for r in rs if passed(r.get("replayCurl")))
    ms = sorted(r.get("ms", 0) for r in rs)
    print(f"{solver:22} {len(rs):5} {page:8} {clear:8} {jvm:7} {curl:8} {ms[len(ms) // 2]:7}")

print("\nper site (jvm replay: P pass, F fail, - no answer)")
sites = sorted({r["url"] for r in rows})
names = sorted(by)
print("  ".join(f"{n:22}" for n in names), " site")
for u in sites:
    cells = []
    for n in names:
        r = next((x for x in by[n] if x["url"] == u), None)
        if r is None:
            cells.append("-")
            continue
        mark = "P" if passed(r.get("replayJvm")) else "F"
        extra = ""
        if r.get("solve"):
            extra = f" {r['solve'].get('stage')}:{r['solve'].get('status')}"
        elif not r.get("ok"):
            extra = " err"
        cells.append(mark + extra)
    print("  ".join(f"{c:22}" for c in cells), " " + u)
