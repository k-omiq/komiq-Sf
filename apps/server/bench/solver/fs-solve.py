"""Solve one URL with a FlareSolverr-v1 endpoint and print the clearance it earned
as one JSON line; replay-all.py then checks it from plain HTTP clients.
Usage: python3 fs-solve.py SOLVER_NAME ENDPOINT URL
"""
import json
import os
import sys
import time
import urllib.request
from urllib.parse import urlparse

name, endpoint, url = sys.argv[1:4]
out = {"solver": name, "url": url}
started = time.time()
try:
    payload = {"cmd": "request.get", "url": url, "maxTimeout": 60000}
    if os.environ.get("PROXY"):
        payload["proxy"] = {"url": os.environ["PROXY"]}
    body = json.dumps(payload).encode()
    req = urllib.request.Request(endpoint + "/v1", body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=90) as r:
        resp = json.load(r)
except Exception as e:  # FlareSolverr answers 500 with a JSON body on a failed solve
    resp = {"status": "error", "message": str(e)}
    if hasattr(e, "read"):
        try:
            resp = json.loads(e.read())
        except Exception:
            pass

out["ok"] = resp.get("status") == "ok"
if not out["ok"]:
    out["error"] = str(resp.get("message", ""))[:300]
sol = resp.get("solution") or {}
host = urlparse(url).hostname.removeprefix("www.")
cookies = [c for c in sol.get("cookies", []) if c.get("domain", "").lstrip(".").endswith(host)]
out["status"] = sol.get("status")
out["cfClearance"] = any(c["name"] == "cf_clearance" for c in cookies)
out["cookieNames"] = sorted({c["name"] for c in cookies})
html = sol.get("response") or ""
lo = html.lower()
out["title"] = lo.split("<title", 1)[1].split(">", 1)[1].split("<", 1)[0][:80] if "<title" in lo else ""
out["afterPresent"] = "challenges.cloudflare.com" in lo or "_cf_chl_opt" in lo
ua = sol.get("userAgent")
out["ua"] = ua
out["cookieHeader"] = "; ".join(f"{c['name']}={c['value']}" for c in cookies)
out["ms"] = int((time.time() - started) * 1000)
print(json.dumps(out))
