"""Write router.yaml for the gost egress router: only the domains in a list leave
through the upstream proxy, everything else goes direct.
Usage: python3 gen-router.py DOMAINS_FILE > router.yaml
Reads PROXY_UPSTREAM (http://user:pass@host:port) from the environment. The output
holds the proxy password: keep it mode 600 and out of git.
"""
import json
import os
import sys
from urllib.parse import urlparse

up = urlparse(os.environ["PROXY_UPSTREAM"])
domains = sorted({l.strip().lower() for l in open(sys.argv[1]) if l.strip() and not l.startswith("#")})
# gost: ".example.com" matches example.com and every subdomain.
matchers = [d if d.startswith(".") else "." + d.removeprefix("www.") for d in domains]

auth = ""
if up.username:
    auth = f"""
              auth:
                username: {json.dumps(up.username)}
                password: {json.dumps(up.password or "")}"""

print(f"""services:
  - name: socks
    addr: ":1080"
    handler:
      type: socks5
      chain: egress
    listener:
      type: tcp
  - name: http
    addr: ":8080"
    handler:
      type: http
      chain: egress
    listener:
      type: tcp
chains:
  - name: egress
    hops:
      - name: upstream
        # whitelist: the hop (the proxy) is used ONLY for matched hosts;
        # every other host skips it and goes direct.
        bypass: proxied-sites
        nodes:
          - name: upstream
            addr: {json.dumps(f"{up.hostname}:{up.port}")}
            connector:
              type: {json.dumps(up.scheme or "http")}{auth}
            dialer:
              type: tcp
bypasses:
  - name: proxied-sites
    whitelist: true
    matchers: {json.dumps(matchers)}
""")
