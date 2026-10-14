#!/bin/sh
# Start WARP in proxy mode and share its SOCKS5 port with the Compose network.
# WARP's own proxy listens on 127.0.0.1:40000 only; socat publishes it on 1080.
# The device registration lives in /var/lib/cloudflare-warp (a volume), so it
# is made once and reused on every restart.
set -eu

warp-svc >/var/log/warp-svc.log 2>&1 &

tries=0
until warp-cli --accept-tos status >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ "$tries" -ge 30 ]; then
        echo "warp-svc did not start; see /var/log/warp-svc.log" >&2
        exit 1
    fi
    sleep 1
done

if ! warp-cli --accept-tos registration show >/dev/null 2>&1; then
    warp-cli --accept-tos registration new
fi
warp-cli --accept-tos mode proxy
warp-cli --accept-tos proxy port 40000
warp-cli --accept-tos connect

exec socat TCP-LISTEN:1080,fork,reuseaddr TCP:127.0.0.1:40000
