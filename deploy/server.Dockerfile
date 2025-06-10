# Multi-stage build for komika-server (apps/server).
# Build context is the REPO ROOT:
#   docker build -f deploy/server.Dockerfile -t komika-server .
#
# The crate is self-contained (no path deps outside apps/server) and uses
# rustls (no OpenSSL), so the runtime image needs only libc + CA certs.

# ---- builder ----------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder
WORKDIR /build

# Copy only the server crate (its Cargo.toml has no workspace parent).
COPY apps/server/ ./apps/server/

WORKDIR /build/apps/server
# `panic = "abort"` + LTO are set in the crate's [profile.release].
RUN cargo build --release --locked --bin komika-server \
    && strip target/release/komika-server || true

# ---- runtime ----------------------------------------------------------------
# Distroless-style slim base kept simple; only CA certs are pulled in for
# outbound HTTPS to Suwayomi and the backup bucket via reqwest+rustls.
FROM debian:bookworm-slim AS runtime

# Litestream provides continuous SQLite backup (WAL → S3-compatible / R2) + restore-on-boot.
ARG LITESTREAM_VERSION=0.3.13

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    # Install the Litestream static binary for THIS image's architecture. Resolve
    # it from dpkg (emits amd64/arm64, matching Litestream's asset names) instead
    # of relying on TARGETARCH, which is only injected under BuildKit/buildx and
    # silently defaulted to amd64 under the legacy builder → wrong-arch binary.
    && arch="$(dpkg --print-architecture)" \
    && curl -fsSL "https://github.com/benbjohnson/litestream/releases/download/v${LITESTREAM_VERSION}/litestream-v${LITESTREAM_VERSION}-linux-${arch}.tar.gz" \
       | tar -xz -C /usr/local/bin litestream \
    && rm -rf /var/lib/apt/lists/* \
    # Non-root runtime user; owns the data dir where the SQLite file lives.
    && useradd --system --create-home --uid 10001 komika \
    && mkdir -p /data \
    && chown komika:komika /data

COPY --from=builder /build/apps/server/target/release/komika-server /usr/local/bin/komika-server
# Vendored extension icons (~11 MB), served at /ext-icons/{pkgName}.png. Baked in
# rather than fetched at runtime because Keiyoushi emptied the icon directory we
# used to link (see apps/server/src/ext_icon.rs); refresh with
# `node scripts/fetch-ext-icons.mjs`. Read-only to the runtime user.
COPY assets/ext-icons/ /srv/ext-icons/
COPY deploy/litestream.yml /etc/litestream.yml
COPY deploy/server-entrypoint.sh /usr/local/bin/komika-entrypoint.sh
RUN chmod +x /usr/local/bin/komika-entrypoint.sh

USER komika
WORKDIR /data

# Defaults; override via compose / deploy env. DB persists under /data (user
# avatars are stored as BLOBs in the DB, so they persist + back up with it).
ENV PORT=8080 \
    DATABASE_URL=sqlite:///data/komika.sqlite3 \
    SUWAYOMI_URL=http://suwayomi:4567 \
    RUST_LOG=komika_server=info,tower_http=warn

EXPOSE 8080

# `/health` returns 200 with a static body once the server is listening.
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:8080/health || exit 1

# Entrypoint runs the server under Litestream when configured, else runs it plain.
ENTRYPOINT ["/usr/local/bin/komika-entrypoint.sh"]
