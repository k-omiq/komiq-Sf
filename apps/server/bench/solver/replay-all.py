"""Read one solver JSON line on stdin, replay its User-Agent and cookies with curl
(host) and the JVM HTTP client (jvm container), and print the line with the replay
results added. The cookie values are dropped, so results files hold no clearance.
"""
import json
import os
import subprocess
import sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        r = json.loads(line)
    except ValueError:
        print(json.dumps({"ok": False, "error": "bad solver output", "raw": line[:300]}))
        continue
    header = r.pop("cookieHeader", "") or ""
    r["route"] = os.environ.get("ROUTE", "direct")
    ua = r.get("ua")
    if ua and r.get("url"):
        args = [r["url"], ua, header]
        curl = subprocess.run(
            ["bash", "replay.sh", *args], stdin=subprocess.DEVNULL, capture_output=True, text=True
        )
        jvm_env = []
        if os.environ.get("JVM_PROXY"):
            ph, pp = os.environ["JVM_PROXY"].rsplit(":", 1)
            jvm_env = ["-e", f"PROXY_HOST={ph}", "-e", f"PROXY_PORT={pp}"]
        jvm = subprocess.run(
            ["docker", "compose", "exec", "-T", *jvm_env, "jvm", "java", "Replay.java", *args],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=60,
        )
        r["replayCurl"] = curl.stdout.strip()
        r["replayJvm"] = jvm.stdout.strip().splitlines()[-1] if jvm.stdout.strip() else "000 error"
    print(json.dumps(r))
