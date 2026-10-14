# Cloudflare WARP in local proxy mode, for the engine's outgoing requests.
# Optional: started by COMPOSE_PROFILES=warp together with WARP_ENABLED=true
# (see .env.example). Some sites block datacenter IP ranges (a VPS); their
# requests succeed when they leave through WARP instead.
FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl gnupg socat \
 && curl -fsSL https://pkg.cloudflareclient.com/pubkey.gpg \
    | gpg --dearmor -o /usr/share/keyrings/cloudflare-warp-archive-keyring.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/cloudflare-warp-archive-keyring.gpg] https://pkg.cloudflareclient.com/ bookworm main" \
    > /etc/apt/sources.list.d/cloudflare-client.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends cloudflare-warp \
 && rm -rf /var/lib/apt/lists/*
COPY entrypoint.sh /usr/local/bin/warp-proxy
RUN chmod 755 /usr/local/bin/warp-proxy
EXPOSE 1080
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 \
  CMD curl -fsS -m 8 --socks5-hostname 127.0.0.1:40000 https://www.cloudflare.com/cdn-cgi/trace | grep -q '^warp=on'
ENTRYPOINT ["/usr/local/bin/warp-proxy"]
