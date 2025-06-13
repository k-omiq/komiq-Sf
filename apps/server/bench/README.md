# `bench/` — load testing komika-server

`loadtest.mjs` is a zero-dependency load generator for the GraphQL API. It exists to
replace guesses about capacity with numbers, and to act as the **acceptance test for
performance work**: any "this made it faster" claim should be a before/after run of this
harness, not an argument.

Baseline results: [`results/2026-07-27-baseline.md`](results/2026-07-27-baseline.md).

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

| Flag | Default | Meaning |
|---|---|---|
| `--target <url>` | `http://127.0.0.1:8080` | Base URL |
| `--scenario <name>` | `health` | See table below |
| `--vus a,b,c` | `1,2,4,8,16,32,64` | Closed-loop ramp steps |
| `--rate <rps>` | — | Open-loop arrival rate (mutually exclusive with `--vus`) |
| `--duration <sec>` | `10` | Measured window per step |
| `--warmup <sec>` | `3` | Discarded window per step |
| `--max-p95 <ms>` | `750` | Abort the ramp above this p95 |
| `--think <sec>` | `5` | Seconds between requests per real user, for the rps→users conversion |
| `--queries <file>` | — | JSON overriding the GraphQL operation text |
| `--json <file>` | — | Write full results as JSON |

## Scenarios

| Name | What it exercises |
|---|---|
| `health` | `GET /health` — no DB. The **framework floor**: the ceiling tokio + axum + the kernel impose, which every other number sits under. |
| `ready` | `GET /health/ready` — runs `SELECT 1`, isolating pool-acquire cost from query cost. |
| `discovery` | The home feed. Most-requested read in the product, and the current bottleneck. |
| `search` | Browse rows against the materialized `browse_catalogue`. Page is randomized because deep `OFFSET` paging is where it gets expensive. |
| `facets` | `genreFacets` — a full catalogue aggregate with no in-process cache. |
| `home` | `discovery` + `updates`, i.e. roughly one home page view. |
| `mixed` | Weighted blend (discovery 5, search 3, updates 2, facets 1) — the headline number. |

## Two load shapes, two questions

**Closed-loop (`--vus`)** — N virtual users, each firing the next request as soon as the
previous returns. Finds **saturation**: raise VUs until rps stops rising and latency
climbs linearly. That knee is real capacity.

**Open-loop (`--rate`)** — fires on a fixed schedule regardless of how fast the server
answers. Finds **latency under load**, and is the only shape that exposes *coordinated
omission*: when the server stalls, a closed loop politely stops sending and hides the
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

This repo *is* the live VPS, and `api.komiq.cc` serves real traffic from the same
container. Three things make the harness safe to run against it:

1. **Read-only scenarios.** No mutations. Nothing is written.
2. **`--max-p95` abort.** The ramp stops climbing as soon as p95 crosses the threshold —
   it backs off before users notice, rather than after.
3. **Error-rate abort.** Stops if >5% of requests fail.

It still competes for CPU with production traffic while running. Keep `--duration`
modest, and prefer off-peak.

## Scope limits — what a clean run does NOT prove

- **The write path is untested.** `RecordView` (3 writes + 1 read per chapter open,
  serialized by SQLite's single writer) and the rate limiter's process-global mutex are
  deliberately not exercised. A clean `mixed` result says nothing about them.
- **The default target bypasses the edge.** `127.0.0.1:8080` measures *origin* capacity —
  the thing code changes move. It excludes the cloudflared tunnel and Cloudflare's
  cache. Point `--target` at `https://api.komiq.cc` to measure the whole delivery path;
  expect lower numbers, because the tunnel becomes the limit.
- **`mixed` weights are an estimate**, not measured traffic. Refine from access logs.
- **Anonymous only.** No auth token is sent, so the per-viewer library N+1
  (`viewer_library_row`, one query per series for signed-in users) never fires. Real
  logged-in traffic is heavier than these numbers suggest.

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
