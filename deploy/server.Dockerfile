# Multi-stage build for komika-server (apps/server).
# Build context is the REPO ROOT:
#   docker build -f deploy/server.Dockerfile -t komika-server .
#
# The crate is self-contained (no path deps outside apps/server) and uses
# rustls (no OpenSSL), so the runtime image needs only libc + CA certs.

# ---- builder ----------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

# Keep bundled SQLite build flags identical to local and CI builds.
COPY .cargo/config.toml ./.cargo/config.toml
# The server crate has no workspace parent.
COPY apps/server/ ./apps/server/

WORKDIR /build/apps/server
# `panic = "unwind"` + LTO are set in the crate's [profile.release].
RUN cargo build --release --locked --bin komika-server \
    && strip target/release/komika-server

# ---- runtime ----------------------------------------------------------------
# Distroless-style slim base kept simple; only CA certs are pulled in for
# outbound HTTPS to upstream sources via reqwest+rustls.
FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    # Non-root runtime user; owns the covers cache and legacy import directory.
    && useradd --system --create-home --uid 10001 komika \
    && mkdir -p /data \
    && chown komika:komika /data

COPY --from=builder /build/apps/server/target/release/komika-server /usr/local/bin/komika-server
# Vendored extension icons (~11 MB), served at /ext-icons/{pkgName}.png. Baked in
# rather than fetched at runtime because Keiyoushi emptied the icon directory we
# used to link (see apps/server/src/ext_icon.rs); refresh with
# `node scripts/fetch-ext-icons.mjs`. Read-only to the runtime user.
COPY assets/ext-icons/ /srv/ext-icons/
COPY deploy/server-entrypoint.sh /usr/local/bin/komika-entrypoint.sh
RUN chmod +x /usr/local/bin/komika-entrypoint.sh

USER komika
WORKDIR /data

# Defaults; Compose supplies the main database password. Covers persist under
# /data; main data and avatars live in PostgreSQL.
ENV PORT=8080 \
    DATABASE_URL=postgres://komika@postgres:5432/komika \
    COVERS_DATABASE_URL=sqlite:///data/covers.sqlite3 \
    SUWAYOMI_URL=http://suwayomi:4567 \
    RUST_LOG=komika_server=info,tower_http=warn

EXPOSE 8080

# `/health` returns 200 with a static body once the server is listening.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:8080/health || exit 1

# Entrypoint executes the server; PostgreSQL owns backup and restore.
ENTRYPOINT ["/usr/local/bin/komika-entrypoint.sh"]
