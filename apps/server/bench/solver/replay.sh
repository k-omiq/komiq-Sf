#!/usr/bin/env bash
# Replay a solved page the way the engine would: a plain HTTP client with the
# solver's User-Agent and cookies. Prints "status wall" (wall: none|challenge).
# Usage: replay.sh URL USER_AGENT COOKIE_HEADER
set -uo pipefail
url=$1 ua=$2 cookie=$3
tmp=$(mktemp -d)
status=$(curl -s -m 20 -L --max-redirs 5 ${HOST_PROXY:+--socks5-hostname "$HOST_PROXY"} \
  -A "$ua" -H 'Accept: text/html' \
  -H 'Accept-Language: en-US,en;q=0.9' ${cookie:+-H "Cookie: $cookie"} \
  -D "$tmp/h" -o "$tmp/b" -w '%{http_code}' "$url" 2>/dev/null)
touch "$tmp/h" "$tmp/b"
wall=none
if grep -qi '^cf-mitigated:' "$tmp/h" || grep -qiE 'challenges\.cloudflare\.com|cf-chl-|_cf_chl_opt' "$tmp/b"; then
  wall=challenge
fi
echo "${status:-000} $wall"
rm -rf "$tmp"
