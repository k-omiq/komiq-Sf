#!/usr/bin/env python3
"""
Bootstrap a running Komika stack into a working public deployment:

  1. Point Suwayomi at the Keiyoushi extension repo and install the MangaDex source.
  2. Enable the FlareSolverr Cloudflare-challenge solver in Suwayomi.
  3. Create the Komika admin account.
  4. Seed the library with a starter set of series so the catalog is populated and
     the adaptive scanner immediately has things to auto-update.

Idempotent — safe to re-run. Best-effort per step (a failure is logged, not fatal),
except the two waits. Reads config from deploy/.env (or environment / defaults).
Stdlib only (urllib), no pip deps.
"""
import json
import os
import sys
import time
import urllib.request
import urllib.error

HERE = os.path.dirname(os.path.abspath(__file__))


def load_env():
    env = dict(os.environ)
    dotenv = os.path.join(HERE, ".env")
    if os.path.exists(dotenv):
        for line in open(dotenv):
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            env.setdefault(k.strip(), v.strip())  # real env wins over .env
    return env


ENV = load_env()
SUWA_PORT = ENV.get("SUWAYOMI_PORT", "4567")
SRV_PORT = ENV.get("SERVER_PORT", "8080")
ADMIN_USER = ENV.get("KOMIKA_ADMIN_USERS", "admin").split(",")[0].strip() or "admin"
DEFAULT_ADMIN_PW = "change-this-admin-pw"
ADMIN_PW = ENV.get("KOMIKA_ADMIN_PASSWORD", DEFAULT_ADMIN_PW)
ADMIN_EMAIL = ENV.get("KOMIKA_ADMIN_EMAIL", "admin@example.com")
SEED = [s.strip() for s in ENV.get("SEED_SERIES", "").split(",") if s.strip()]

SUWA = f"http://localhost:{SUWA_PORT}/api/graphql"
SRV = f"http://localhost:{SRV_PORT}/graphql"
KEIYOUSHI = "https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.min.json"
MANGADEX_PKG = "eu.kanade.tachiyomi.extension.all.mangadex"
FLARESOLVERR_URL = "http://flaresolverr:8191"

C = {"cy": "\033[36m", "gn": "\033[32m", "yl": "\033[33m", "rd": "\033[31m", "z": "\033[0m"}
def log(m): print(f"{C['cy']}▸ {m}{C['z']}")
def ok(m): print(f"{C['gn']}  ✓ {m}{C['z']}")
def warn(m): print(f"{C['yl']}  ! {m}{C['z']}")


def gql(endpoint, query, variables=None):
    body = json.dumps({"query": query, "variables": variables or {}}).encode()
    req = urllib.request.Request(endpoint, data=body, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        data = json.loads(r.read().decode())
    if data.get("errors"):
        raise RuntimeError("; ".join(e.get("message", "?") for e in data["errors"]))
    return data.get("data") or {}


def wait_for(url, name, tries=80, delay=3):
    log(f"waiting for {name} …")
    for _ in range(tries):
        try:
            urllib.request.urlopen(url, timeout=5)
            ok(f"{name} is up")
            return True
        except Exception:
            time.sleep(delay)
    print(f"{C['rd']}  ✗ {name} never became ready{C['z']}")
    return False


# ---- 1. Suwayomi: repo + MangaDex source --------------------------------------

def configure_suwayomi():
    log("adding Keiyoushi extension repo")
    try:
        gql(SUWA, "mutation($u:String!){ addExtensionStore(input:{indexUrl:$u}){ clientMutationId } }", {"u": KEIYOUSHI})
        ok("repo registered")
    except Exception as e:
        warn(f"repo add: {e} (probably already present)")

    log("refreshing extension list")
    try:
        gql(SUWA, "mutation { fetchExtensions(input:{}){ clientMutationId } }")
    except Exception as e:
        warn(f"fetchExtensions: {e}")

    # Install MangaDex (idempotent — installing an already-installed extension is
    # a no-op). Then verify by name (the reliable filter).
    log("installing MangaDex source")
    try:
        gql(SUWA, "mutation($id:String!){ updateExtension(input:{id:$id, patch:{install:true}}){ extension { isInstalled } } }", {"id": MANGADEX_PKG})
    except Exception as e:
        warn(f"install MangaDex: {e}")
    try:
        d = gql(SUWA, 'query { extensions(condition:{name:"MangaDex"}){ nodes { pkgName isInstalled } } }')
        node = next((n for n in d["extensions"]["nodes"] if n["pkgName"] == MANGADEX_PKG), None)
        if node and node["isInstalled"]:
            ok("MangaDex source ready")
        else:
            warn("MangaDex not installed — check repo/network and re-run")
    except Exception as e:
        warn(f"verify MangaDex: {e}")


# ---- 2. Suwayomi: FlareSolverr Cloudflare bypass ------------------------------

def enable_flaresolverr():
    log("enabling FlareSolverr (Cloudflare bypass) in Suwayomi")
    q = ("mutation($u:String!){ setSettings(input:{settings:{"
         "flareSolverrEnabled:true, flareSolverrUrl:$u, flareSolverrAsResponseFallback:true"
         "}}){ clientMutationId } }")
    try:
        gql(SUWA, q, {"u": FLARESOLVERR_URL})
        ok(f"FlareSolverr enabled → {FLARESOLVERR_URL}")
    except Exception as e:
        warn(f"enable FlareSolverr: {e}")


# ---- 3. Komika admin account --------------------------------------------------

def create_admin():
    # The server provisions the admin account itself at startup, from
    # KOMIKA_ADMIN_USERS + KOMIKA_ADMIN_PASSWORD (the names are reserved from open
    # registration, so this cannot be done via the register mutation). By the time
    # bootstrap runs, the account already exists — nothing to do here.
    log(f"admin account '{ADMIN_USER}' is provisioned by the server at startup")
    ok("admin provisioning is server-side (KOMIKA_ADMIN_PASSWORD)")


# ---- 4. Seed the library ------------------------------------------------------

def resolve_mangadex_source():
    # After install, find an English MangaDex source id (retry a few times — the
    # source list can lag the extension install).
    for _ in range(10):
        try:
            d = gql(SUWA, "query { sources { nodes { id name lang } } }")
            srcs = d["sources"]["nodes"]
            md = [s for s in srcs if "mangadex" in s["name"].lower()]
            if md:
                en = next((s for s in md if s["lang"] == "en"), None)
                return (en or md[0])["id"]
        except Exception:
            pass
        time.sleep(3)
    return None


def seed_library():
    if not SEED:
        log("no SEED_SERIES set — starting with an empty library")
        return
    log(f"seeding library with {len(SEED)} series")
    source = resolve_mangadex_source()
    if not source:
        warn("could not resolve a MangaDex source — skipping seed")
        return
    search = ("mutation($s:LongString!,$q:String!){ fetchSourceManga(input:{source:$s,type:SEARCH,page:1,query:$q}){ "
              "mangas { id title } } }")
    added = 0
    for title in SEED:
        try:
            d = gql(SUWA, search, {"s": source, "q": title})
            mangas = d["fetchSourceManga"]["mangas"]
            if not mangas:
                warn(f"no match for '{title}'")
                continue
            m = mangas[0]
            gql(SUWA, "mutation($id:Int!){ updateManga(input:{id:$id, patch:{inLibrary:true}}){ manga { id } } }", {"id": m["id"]})
            ok(f"added '{m['title']}' (#{m['id']})")
            added += 1
        except Exception as e:
            warn(f"seed '{title}': {e}")
    ok(f"seeded {added}/{len(SEED)} series — the scanner will keep them updated")


def main():
    if ADMIN_PW == DEFAULT_ADMIN_PW:
        print(
            f"{C['rd']}✗ KOMIKA_ADMIN_PASSWORD is still the committed placeholder "
            f"('{DEFAULT_ADMIN_PW}'). Set a real password in deploy/.env before "
            f"bootstrapping.{C['z']}",
            file=sys.stderr,
        )
        sys.exit(1)
    if not wait_for(f"http://localhost:{SUWA_PORT}/", "Suwayomi"):
        sys.exit(1)
    if not wait_for(f"http://localhost:{SRV_PORT}/health", "Komika server"):
        sys.exit(1)
    configure_suwayomi()
    enable_flaresolverr()
    create_admin()
    seed_library()
    print(f"\n{C['gn']}Bootstrap complete.{C['z']}")


if __name__ == "__main__":
    main()
