# komiq self-host image: the server, the reader at `/`, the admin console at
# `/admin`, and `cloudflared` for the optional Cloudflare Tunnel, in one image.
# The source engine (Suwayomi) runs next to it from its stock image; see
# docker-compose.yml in this folder and docs/self-host.md.
#
# Build context is the REPO ROOT:
#   docker build -f deploy/selfhost/Dockerfile -t komiq-selfhost:local .
#
# This file is separate from deploy/server.Dockerfile on purpose: the hosted
# komiq.cc stack builds from that one, and nothing here may change it.

# ---- web apps ----------------------------------------------------------------
# Both apps are built as "embedded" static SPAs: they talk to `/graphql` on
# whatever address the server is reached at, so one image works on localhost, a
# LAN address and a domain alike.
FROM node:22-slim AS web
WORKDIR /repo
RUN corepack enable && corepack prepare pnpm@11.9.0 --activate
COPY . .
RUN pnpm install --frozen-lockfile --filter "@komika/reader..." --filter "@komika/admin..."
# `$env/static/public` needs every PUBLIC_* the apps import to exist (the .env
# files are not in the build context). Embedded builds ignore the values.
ENV PUBLIC_KOMIKA_BACKEND=on \
    PUBLIC_KOMIKA_BACKEND_KIND=komika \
    PUBLIC_KOMIKA_API= \
    PUBLIC_KOMIKA_IMG_MODE=direct \
    PUBLIC_KOMIKA_IMG_WORKER= \
    PUBLIC_KOMIKA_MEDIA_HOSTS= \
    PUBLIC_SUWAYOMI_URL=
RUN KOMIKA_TARGET=embedded pnpm --filter @komika/reader build \
    && ADMIN_TARGET=embedded ADMIN_BASE_PATH=/admin pnpm --filter @komika/admin build \
    && mkdir -p /web \
    && cp -r apps/reader/build /web/reader \
    && cp -r apps/admin/build /web/admin

# ---- server ------------------------------------------------------------------
# Same crate and release profile as the hosted image. The build takes a while
# (link-time optimisation) and wants about 4 GB of memory.
FROM rust:1-slim-bookworm AS server
WORKDIR /build
COPY .cargo/config.toml ./.cargo/config.toml
COPY apps/server/ ./apps/server/
WORKDIR /build/apps/server
# Match server.Dockerfile's build path and command so both images can reuse the
# same verified release-builder layer.
RUN cargo build --release --locked --bin komika-server \
    && strip target/release/komika-server

# ---- cloudflared ---------------------------------------------------------------
# Pinned release, verified by checksum. The server starts it as a child process
# when Cloudflare Tunnel setup runs (found on PATH; override with CLOUDFLARED_BIN).
# To update: download both assets of the new release, `sha256sum` them, and
# replace the version and the two sums below. Change the same pin in
# apps/server/src/cloudflare/fetch_connector.rs with it: the server downloads
# that release by itself where no cloudflared is installed (the desktop app).
FROM debian:bookworm-slim AS cloudflared
ARG CLOUDFLARED_VERSION=2026.9.3
ARG CLOUDFLARED_SHA256_AMD64=77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2
ARG CLOUDFLARED_SHA256_ARM64=aaeb2d7d0da3614634c7e03ab13487a1522c2e79165ed2929cfe23d5e95b326d
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    # dpkg names the architecture the same way the release assets do (amd64,
    # arm64), and unlike TARGETARCH it is also right under the legacy builder.
    && arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
         amd64) sum="$CLOUDFLARED_SHA256_AMD64" ;; \
         arm64) sum="$CLOUDFLARED_SHA256_ARM64" ;; \
         *) echo "cloudflared: no pinned checksum for $arch" >&2; exit 1 ;; \
       esac \
    && curl -fsSL -o /cloudflared \
       "https://github.com/cloudflare/cloudflared/releases/download/${CLOUDFLARED_VERSION}/cloudflared-linux-${arch}" \
    && echo "$sum  /cloudflared" | sha256sum -c - \
    && chmod 0755 /cloudflared

# ---- runtime -----------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
# ca-certificates for outbound HTTPS (sources, Cloudflare API); curl for the
# health check.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 komika \
    && mkdir -p /data \
    && chown komika:komika /data

COPY --from=server /build/apps/server/target/release/komika-server /usr/local/bin/komika-server
COPY --from=cloudflared /cloudflared /usr/local/bin/cloudflared
COPY --from=web /web /srv/web

USER komika
WORKDIR /data
VOLUME /data

# Container defaults. Listening on every interface is right INSIDE the container:
# which host interfaces reach it is decided by the port publish in compose.
# SUWAYOMI_PUBLIC_URL=/ makes source cover and page URLs relative
# (`/api/v1/manga/...`), so the browser loads them from whatever address it used
# to open komiq and the server proxies them from the engine. No image Worker.
# CORS_ORIGINS lets the native reader apps call this server; the web apps are
# same-origin and do not need it.
ENV BIND_ADDR=0.0.0.0 \
    PORT=8080 \
    COVERS_DATABASE_URL=sqlite:///data/covers.sqlite3 \
    KOMIKA_WEB_DIR=/srv/web \
    SUWAYOMI_URL=http://suwayomi:4567 \
    SUWAYOMI_PUBLIC_URL=/ \
    CORS_ORIGINS=tauri://localhost,http://tauri.localhost,https://tauri.localhost \
    RUST_LOG=komika_server=info,tower_http=warn

EXPOSE 8080

# Compose supplies DATABASE_URL; no SQLite main database fallback lives here.
# Readiness proves the database answers.
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=5 \
    CMD curl -fsS http://localhost:8080/health/ready || exit 1

ENTRYPOINT ["/usr/local/bin/komika-server"]
