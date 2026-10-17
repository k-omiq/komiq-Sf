# `bench/` — load testing komika-server

`loadtest.mjs` is a zero-dependency load generator for the GraphQL API. It exists to
replace guesses about capacity with numbers, and to act as the **acceptance test for
performance work**: any "this made it faster" claim should be a before/after run of this
harness, not an argument.

Baseline results: [`results/2026-07-27-baseline.md`](results/2026-07-27-baseline.md).

## Capacity runs (`capacity.mjs`)

`capacity.mjs` is the second tool. `loadtest.mjs` sends anonymous reads only. `capacity.mjs`
adds the series page, text search, signed-in writes, a whole signed-in reader and images
(`--scenario read | write | session | image-warm | image-cold | mixed | release-burst`),
with the operation text of
`packages/api/src/operations.ts`. It registers its own test accounts, so run it only
against a test stack, never against production.

`host/` holds what the 2026-10-13 runs used on the test host: `measure.sh` (CPU per
container, memory, WAL, archiver and top statements for one run; `--json` for the
suite) and the two Compose overrides
(production settings and the lean profile).

```bash
node capacity.mjs --scenario read --vus 1,2,4,8,16,32,48,64 --duration 20 --warmup 6 --ops
node capacity.mjs --scenario write --vus 1,2,4,8,16,32,64 --accounts 64 --state state.json --ops
node capacity.mjs --scenario session --users 1000,2000,4000 --accounts 4000 --state state.json --ops
```

The state file holds the test password and the session tokens. Keep it out of the
repository and do not print it. Each virtual user sends its own `X-Forwarded-For`, so
the test server must trust the address the tool connects from (`TRUSTED_PROXY_CIDRS`)
and must have sign-up open.

## The M2 suite (one command, one JSON)

`--suite` runs a list of steps in order and writes one JSON file. Each step is one
workload class, so results never mix. `suites/m2-baseline.json` is the baseline: metadata
reads, sequential and random progress writes, writes and sessions on 20 hot works, a
plain session, warm images, cold fixture images, mixed and a release burst.

```bash
ssh -N -L 19091:127.0.0.1:9091 testhost &        # the server's loopback /metrics
node capacity.mjs --suite suites/m2-baseline.json --target http://testhost:8080 \
  --metrics-url http://127.0.0.1:19091/metrics \
  --host-cmd 'ssh testhost ~/measure.sh --json {label} {seconds}' \
  --fixture-images 'http://fixtures.test:8099/img/{n}.webp?run={run}' \
  --image-digest sha256:... --settings-file server.env --seed m2-1 \
  --state /s/state.json --json m2-1.json
```

Add `--duration 5 --warmup 1` for a short smoke run of the same suite. A suite step
sets its own scenario and load (`vus`, `rate` or `users`). Run-wide options go on the
command line or in the suite's `defaults`.

| Flag                                       | Meaning                                                                                                                                                |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `--scenario`                               | `read`, `write`, `session`, `image-warm`, `image-cold`, `mixed`, `release-burst`                                                                       |
| `--seed <s>`                               | Same seed, same pool and the same path for every virtual user. Recorded.                                                                               |
| `--metrics-url <url>`                      | Scraped when the measured window opens and closes, and every `--metrics-interval` s                                                                    |
| `--host-cmd '<cmd>'`                       | Run once per load step; `{label}` and `{seconds}` are filled in; JSON stdout is kept. The window opens when it prints `READY` (`--host-ready-timeout`) |
| `--image-digest`, `--settings-file`        | The server image and its effective environment, recorded                                                                                               |
| `--progress-pattern sequential\|random`    | `write`: page n+1 of one chapter, or a random chapter and page                                                                                         |
| `--hot-works N`                            | Every virtual user works on the same N works                                                                                                           |
| `--fixture-images <file or URL with {n}>`  | Fixture images: page stand-ins, then cold addresses. Needed by `image-cold`, `mixed` and `release-burst`                                               |
| `--real-source-sample N`                   | Real-source image fetches allowed in the whole run, at most 200. Default 0                                                                             |
| `--image-chapters`, `--image-urls`         | The bounded set of page lists and image addresses for image steps                                                                                      |
| `--img-base`, `--own-hosts`                | Where foreign images are proxied; hosts that count as the server itself                                                                                |
| `--burst-works`, `--bursts`, `--burst-gap` | `release-burst` shape                                                                                                                                  |

`host/measure.sh --json <label> <seconds>` prints one JSON object: CPU cores per
container, host busy cores, memory per container, WAL bytes, FPI and records,
`pg_stat_archiver` before and after with the archived and failed deltas, database
size, commits per second and the top statements. It prints `READY` on stderr when its
baseline is taken, and reads the end counters at the window edge. When statistics are
reset inside the window, the value is null and `reset` is true.

### Image policy

The server keeps no page images. The page route asks the engine, and `/img` fetches
upstream on every request. So image load fetches only two kinds of address:

- store covers: `/covers/<work>.webp`, with or without `?w=`, served from the cover store;
- fixture addresses from `--fixture-images`.

Everything else is real-source: engine pages and thumbnails, `/img?src=...` and any
address with a nested source in its query. Such an address is fetched only with
`--real-source-sample N`. N counts every real-source fetch of the run, the warm pass
included, and N is at most 200. When N is used up, those addresses are not fetched
again, and `realSource` records when. An address with a production host anywhere in it
is never fetched.

Harvested chapter pages map to fixture addresses (page stand-ins), so `mixed` and
`release-burst` read real chapter lists with fixture bytes. `image-cold` uses the
fixture addresses after the stand-ins, each once. Without `--fixture-images` these
steps are skipped and listed in `omitted`. A cover not yet in the cover store is made
once from MangaDex on its first request, in the warm pass.

The JSON has this shape:

- `manifest`: run id, tool hash, git HEAD, dirty-file count and diff hash, Node,
  generator host and CPUs, run-wide options, image digest, settings (secrets redacted),
  seed, start and end times.
- `accounts`, `pool` (works found and sampled, `excluded` with the reason, dataset
  totals, work ids, hot order, digest), `images` (page lists, warm pass, classes),
  `fixtures` (origins, stand-ins, cold addresses used), `realSource` (cap and use).
- `omitted`: steps that did not run, and why.
- `steps[]`: name, scenario, every effective option, write pattern, hot works, cache
  state, and `loads[]`. Each load has requests, errors and p50/p95/p99 per class
  (`read`, `write`, `image`) and per operation, image bytes and cache headers,
  requests per work, `metrics` (reset-aware counter increases, gauge
  first/last/min/max, hit ratios, byte counters, refusing lanes), `afterBoundary`
  (requests in flight when the window closed), `host` and `costInputs`.

A GraphQL answer counts as good only when it is 2xx and a JSON object with `data` or
`errors`, and has no errors. A 3xx, or an empty or HTML 200, is an error. No request is
sent after the window closes.

Two runs are comparable when the manifest, `pool.digest` and the settings hash match.
`cacheState` and the `images` warm pass say what was warm before a step.

The tool refuses komiq.cc, any `*.komiq.cc` and the configured production addresses as
target, metrics, image proxy or fixture origin. It checks the text and the DNS answer,
and it catches other spellings of an address, such as one number, hex, or IPv4 inside
IPv6. It also refuses a `--host-cmd` whose text names production. It does not parse that
command, so the operator owns what it does. Passwords and tokens in the command are
replaced before anything is recorded. There is no override.

The production addresses are not in the repository. The tool reads them from two places
and joins them:

- `KOMIKA_PRODUCTION_IPS`: IPv4 addresses separated by commas or spaces.
- `deploy/production-ips.local` (git-ignored): one IPv4 address per line, `#` starts a
  comment.

`deploy/watch.sh` reads the same two. An entry that is not an IPv4 address stops the
tool before it does anything. When neither names an address, the tool still runs and
prints one warning that only the komiq.cc names are refused.

Tests: `node --test capacity.test.mjs` (a local mock server, a few seconds).

### What a run does not prove

- A request count is not a number of people. Do not turn it into daily active users or
  a concurrency figure. `users` in a session step are simulated readers. How many real
  people one equals is an assumption (open question 1 of the 10k plan).
- Image steps measure store covers and fixtures, not MangaDex or a source site. A
  real-source sample is at most 200 fetches, and `realSource` records it.
- The metrics are what the server exposes. When a counter is missing, the derived hit
  ratio or lane is missing too. The tool does not guess it.
- A mock-server test proves the tool works. It says nothing about the server.

## Why hand-rolled

There is no `k6`, `wrk`, `vegeta`, `ab`, `hey` or `autocannon` on this host, and `pnpm`
is broken on this ARM64 box, so installing one is not a one-liner. Node 22 ships
everything needed — `node:http` with a keep-alive Agent saturates 4 vCPU of axum fine —
and writing it by hand means the harness speaks the **real GraphQL operations** instead
of a synthetic GET.

## Requirements

Node >= 22.13. The system Node is 18, so put the right one on PATH first:

```bash
export PATH=$HOME/.local/node/bin:$PATH
cd apps/server/bench
```

## Usage

```bash
node loadtest.mjs --list                                                  # scenarios
node loadtest.mjs --scenario discovery --vus 1,2,4,8,16,32 --duration 8   # find saturation
node loadtest.mjs --scenario mixed --rate 60 --duration 30                # latency at a fixed rate
node loadtest.mjs --scenario mixed --vus 1,2,4,8,16,32 --json out.json    # keep the raw data
```

| Flag                | Default                 | Meaning                                                              |
| ------------------- | ----------------------- | -------------------------------------------------------------------- |
| `--target <url>`    | `http://127.0.0.1:8080` | Base URL                                                             |
| `--scenario <name>` | `health`                | See table below                                                      |
| `--vus a,b,c`       | `1,2,4,8,16,32,64`      | Closed-loop ramp steps                                               |
| `--rate <rps>`      | —                       | Open-loop arrival rate (mutually exclusive with `--vus`)             |
| `--duration <sec>`  | `10`                    | Measured window per step                                             |
| `--warmup <sec>`    | `3`                     | Discarded window per step                                            |
| `--max-p95 <ms>`    | `750`                   | Abort the ramp above this p95                                        |
| `--think <sec>`     | `5`                     | Seconds between requests per real user, for the rps→users conversion |
| `--queries <file>`  | —                       | JSON overriding the GraphQL operation text                           |
| `--json <file>`     | —                       | Write full results as JSON                                           |

## Scenarios

| Name        | What it exercises                                                                                                                    |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `health`    | `GET /health` — no DB. The **framework floor**: the ceiling tokio + axum + the kernel impose, which every other number sits under.   |
| `ready`     | `GET /health/ready` — runs `SELECT 1`, isolating pool-acquire cost from query cost.                                                  |
| `discovery` | The home feed. Most-requested read in the product, and the current bottleneck.                                                       |
| `search`    | Browse rows against the materialized `browse_catalogue`. Page is randomized because deep `OFFSET` paging is where it gets expensive. |
| `facets`    | `genreFacets` — a full catalogue aggregate with no in-process cache.                                                                 |
| `home`      | `discovery` + `updates`, i.e. roughly one home page view.                                                                            |
| `mixed`     | Weighted blend (discovery 5, search 3, updates 2, facets 1) — the headline number.                                                   |

## Two load shapes, two questions

**Closed-loop (`--vus`)** — N virtual users, each firing the next request as soon as the
previous returns. Finds **saturation**: raise VUs until rps stops rising and latency
climbs linearly. That knee is real capacity.

**Open-loop (`--rate`)** — fires on a fixed schedule regardless of how fast the server
answers. Finds **latency under load**, and is the only shape that exposes _coordinated
omission_: when the server stalls, a closed loop politely stops sending and hides the
stall, while an open loop keeps pressure on so queue delay shows up.

**Always confirm a closed-loop capacity figure with an open-loop run at ~80% of it**
before believing it.

## Reading the output

```
load           rps     ok/s      p50      p90      p95      p99      max      err   users~
16vu          54.7     54.7    287.3    331.9    368.8    407.5    443.8        0      274
```

- `ok/s` excludes HTTP errors, network errors **and GraphQL errors**. A GraphQL error is
  HTTP 200 with an `errors` key, so the harness sniffs the body — otherwise a broken
  query looks like excellent throughput. If `err` is large and `ok/s` is ~0, your query
  is invalid, not fast.
- `users~` = `ok/s × --think`. This is the only step converting a server metric into a
  product metric, and it is **only as good as the think-time assumption**. 5 s is a
  reasonable browsing default; derive a real one from access logs before betting on it.
- **Saturation looks like**: rps flat across VU steps while p50 rises roughly linearly.
  Once you see that, more concurrency is buying pure queue delay.

## Safety on this box

This repo _is_ the live VPS, and `api.komiq.cc` serves real traffic from the same
container. Three things make the harness safe to run against it:

1. **Read-only scenarios.** No mutations. Nothing is written.
2. **`--max-p95` abort.** The ramp stops climbing as soon as p95 crosses the threshold —
   it backs off before users notice, rather than after.
3. **Error-rate abort.** Stops if >5% of requests fail.

It still competes for CPU with production traffic while running. Keep `--duration`
modest, and prefer off-peak.

## Scope limits — what a clean run does NOT prove

- **The write path is untested.** `RecordView` (3 writes + 1 read per chapter open,
  serialized by SQLite's single writer when that baseline was recorded) and the rate limiter's process-global mutex are
  deliberately not exercised. A clean `mixed` result says nothing about them.
- **The default target bypasses the edge.** `127.0.0.1:8080` measures _origin_ capacity —
  the thing code changes move. It excludes the cloudflared tunnel and Cloudflare's
  cache. Point `--target` at `https://api.komiq.cc` to measure the whole delivery path;
  expect lower numbers, because the tunnel becomes the limit.
- **`mixed` weights are an estimate**, not measured traffic. Refine from access logs.
- **Anonymous only by default.** No auth token is sent, so viewer-library batch reads
  are not exercised. `capacity.mjs` (`--scenario write | session`) adds authenticated
  writes.

## Keeping it honest

The operations in `loadtest.mjs` are copied from `packages/api/src/operations.ts`. If
those change, **update the harness** — a load test against queries nobody sends measures
nothing.

`SeriesFields` is reproduced in full on purpose. Trimming it to `id title` would make the
server look ~5× faster than it is in production. (Ironically, on the 2026-07-27 baseline
the base resolver dominated so heavily that trimming barely mattered — but that is a
finding, not something to assume.)

Note the comment on the `search` operation: the harness currently uses the **deployed**
(narrower) argument surface, which lacks `sort`/`hasChapters`/`types`/`status`/
`contentRating`. Widen it once the browse-catalogue server change ships.

## Adding a scenario

Add an entry to `SCENARIOS`: a weighted list of `[weight, () => request]` pairs, where a
request is `{method, path, headers?, body?}`. Use the `GQL(query, variables)` helper for
GraphQL. Weights only matter for blends; single-op scenarios use `[[1, ...]]`.

## Local contention comparison

The main database has been PostgreSQL since 2026-10-13. The SQLite-era writer
harnesses (`sqlite_contention.rs`, `sqlite_projection.rs`) were removed on 2026-10-14:
the test `Database` is PostgreSQL, so they could no longer open a SQLite copy. Their
recorded results stay in `results/sqlite-contention-results.json`, and the files remain
in git history. The PostgreSQL equivalent is
the ignored test `postgres_writer_overlap_benchmark` in `postgres_contention.rs`. It
needs `KOMIKA_PG_BENCH_URL` (an imported scratch database with no other client),
`KOMIKA_PG_BENCH_OUTPUT` (the JSON result path) and `KOMIKA_PG_BENCH_SERVER_LOG`. Never
point it at the production database.
