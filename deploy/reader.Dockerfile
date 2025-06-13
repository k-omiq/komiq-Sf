# Multi-stage build for the reader SPA (apps/reader), served by nginx.
# Build context is the REPO ROOT:
#   docker build -f deploy/reader.Dockerfile -t komika-reader .
#
# This is the STATIC / self-host path. The default `pnpm build` now targets
# Cloudflare edge SSR (adapter-cloudflare); this image forces the static SPA
# build with KOMIKA_TARGET=tauri (adapter-static), which emits a fully static
# SPA to apps/reader/build with an index.html fallback so any tiny static server
# works. nginx is used here for the SPA rewrite + long-cache immutable assets.
# For the edge-SSR web deploy, use apps/reader/wrangler.toml instead.
#
# Build-time public config is baked in (Vite inlines PUBLIC_* at build time).
# Point the reader at the backend by passing build args, e.g.:
#   --build-arg PUBLIC_KOMIKA_API=https://api.example.com/graphql

# ---- builder ----------------------------------------------------------------
FROM node:22-slim AS builder
WORKDIR /repo

# Enable the pinned pnpm via corepack.
RUN corepack enable && corepack prepare pnpm@11.9.0 --activate

# Copy the whole workspace: the reader depends on packages/* via workspace:*,
# so a partial copy would break resolution. Lockfile is at the root.
COPY . .

RUN pnpm install --frozen-lockfile

# Reader backend wiring (see apps/server/README.md "Point the reader at it").
# Defaults keep the local mock fallback; override for a real deploy.
ARG PUBLIC_KOMIKA_BACKEND=off
ARG PUBLIC_KOMIKA_BACKEND_KIND=komika
ARG PUBLIC_KOMIKA_API=http://localhost:8080/graphql
ARG PUBLIC_KOMIKA_IMG_MODE=direct
# Base URL of the deployed Cloudflare image Worker (used when IMG_MODE=proxy).
ARG PUBLIC_KOMIKA_IMG_WORKER=http://localhost:8787
# Donation link. EMPTY BY DEFAULT, unlike the committed `apps/reader/.env`: that file
# carries komiq's own Ko-fi URL, and Vite loads it in every mode, so without this ARG a
# self-hosted image would ship a header button and a Support-page section soliciting
# donations to komiq. Empty hides every donate link (see apps/reader/src/lib/config.ts).
ARG PUBLIC_KOMIKA_DONATE_URL=
ENV PUBLIC_KOMIKA_BACKEND=$PUBLIC_KOMIKA_BACKEND \
    PUBLIC_KOMIKA_BACKEND_KIND=$PUBLIC_KOMIKA_BACKEND_KIND \
    PUBLIC_KOMIKA_API=$PUBLIC_KOMIKA_API \
    PUBLIC_KOMIKA_IMG_MODE=$PUBLIC_KOMIKA_IMG_MODE \
    PUBLIC_KOMIKA_IMG_WORKER=$PUBLIC_KOMIKA_IMG_WORKER \
    PUBLIC_KOMIKA_DONATE_URL=$PUBLIC_KOMIKA_DONATE_URL

# Force the static SPA target (adapter-static → apps/reader/build). Without this,
# the default build targets Cloudflare and would not emit `build/`.
RUN KOMIKA_TARGET=tauri pnpm --filter @komika/reader build

# ---- runtime ----------------------------------------------------------------
FROM nginx:1.27-alpine AS runtime

# SPA config: history-fallback to index.html, cache immutable hashed assets, CSP.
# Installed as a template: the stock nginx entrypoint runs envsubst on
# /etc/nginx/templates/*.template at boot. NGINX_ENVSUBST_FILTER restricts
# substitution to CSP_* vars so nginx runtime vars ($uri, $host, …) are left
# alone. CSP origins default to empty (same-origin only) — override per deploy
# (see docker-compose.yml / deploy README).
ENV NGINX_ENVSUBST_FILTER="^CSP_" \
    CSP_IMG_ORIGIN="" \
    CSP_CONNECT_ORIGIN=""
COPY deploy/nginx.conf /etc/nginx/templates/default.conf.template
COPY --from=builder /repo/apps/reader/build /usr/share/nginx/html

EXPOSE 80

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget -qO- http://localhost/ >/dev/null 2>&1 || exit 1
