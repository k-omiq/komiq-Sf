#!/usr/bin/env node
// Zero-dependency load generator for komika-server.
//
// WHY a hand-rolled generator instead of k6/wrk/vegeta: none of them are installed
// on this host, `pnpm` is broken on this ARM64 box, and pulling a binary in just to
// answer "how many users can we take?" is more moving parts than the measurement.
// Node 22 (~/.local/node/bin) ships everything needed: `node:http` with a keep-alive
// Agent is enough to saturate 4 vCPU of axum, and doing it by hand means the harness
// speaks our actual GraphQL operations rather than a synthetic GET.
//
// WHAT IT MEASURES. Two loop shapes, because they answer different questions:
//
//   closed-loop (--vus)   N virtual users, each firing its next request the instant
//                         the previous one returns. This finds SATURATION: push VUs
//                         up until RPS stops rising and latency starts climbing
//                         linearly. That knee is the box's real capacity.
//
//   open-loop  (--rate)   A fixed arrival rate, independent of how fast the server
//                         answers. This finds LATENCY UNDER LOAD and is the only
//                         shape that exposes coordinated omission: if the server
//                         stalls, a closed loop politely stops sending (hiding the
//                         stall), while an open loop keeps the pressure on and the
//                         queue delay shows up in the numbers. Always confirm a
//                         closed-loop capacity figure with an open-loop run at ~80%
//                         of it before believing the capacity figure.
//
// SAFETY. This box serves live traffic (api.komiq.cc via the cloudflared tunnel), so
// the harness defaults to READ-ONLY scenarios and hard-aborts the ramp when p95
// crosses --max-p95 (default 750 ms). That guard is what makes it safe to run
// against prod: it backs off before real users notice, instead of after.
//
// Default target is 127.0.0.1:8080 — the container's published port, which bypasses
// the Cloudflare tunnel and edge cache. That is deliberate: it measures ORIGIN
// capacity, the thing code changes actually move. Point --target at
// https://api.komiq.cc to measure the whole delivery path including the tunnel
// (expect much lower ceilings there — the tunnel, not the server, becomes the limit).
//
// USAGE
//   node loadtest.mjs --list
//   node loadtest.mjs --scenario health    --vus 1,4,16,64 --duration 10
//   node loadtest.mjs --scenario discovery --rate 40 --duration 30
//   node loadtest.mjs --scenario mixed     --vus 1,2,4,8,16,32 --duration 15 --json out.json

import http from 'node:http';
import https from 'node:https';
import { URL } from 'node:url';
import { readFileSync, writeFileSync } from 'node:fs';

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------
// Each scenario is a weighted list of request factories. Weights let `mixed`
// approximate a real traffic blend rather than hammering one endpoint, which is
// what makes the resulting RPS translatable into "users" (see --think).
//
// GraphQL operations here MUST stay in sync with what the reader actually sends;
// a load test against queries nobody runs measures nothing. See bench/README.md.

const GQL = (query, variables = {}) => ({
  method: 'POST',
  path: '/graphql',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ query, variables }),
});

const SCENARIOS = {
  // Pure framework floor: no DB work at all. Establishes the ceiling imposed by
  // tokio + axum + the kernel on this box, which every other number sits under.
  health: [[1, () => ({ method: 'GET', path: '/health' })]],

  // `/health/ready` runs `SELECT 1` through the pool, so this isolates pool
  // acquire + round-trip cost from query cost.
  ready: [[1, () => ({ method: 'GET', path: '/health/ready' })]],

  // The home feed: up to 4 rails x 20 items, every one carrying SeriesFields.
  // This is the single most-requested read in the product and the biggest
  // amplifier of the per-row `detectedAt` N+1.
  discovery: [[1, () => GQL(Q.discovery)]],

  // Browse rows. Fetched CLIENT-side by the reader, so the SvelteKit edge cache
  // never sees them — every browse scroll is an origin hit against the
  // materialized `browse_catalogue` (~115k rows). Page is randomized because
  // paging deeper is where OFFSET scans get expensive.
  search: [[1, () => GQL(Q.search, {
    query: '', page: 1 + ((Math.random() * 5) | 0), includeNsfw: false,
  })]],

  // A full catalogue aggregate with no in-process cache — worth isolating
  // because one uncached aggregate can dominate a mixed profile.
  facets: [[1, () => GQL(Q.facets)]],

  // One home page view = 3 parallel ops in the real client. Modelled as the two
  // whose text we mirror here, so 1 "request" in this scenario is ~half a page view.
  home: [
    [1, () => GQL(Q.discovery)],
    [1, () => GQL(Q.updates, { page: 1 })],
  ],

  // Realistic blend. Weights approximate the traffic ranking measured in the hot
  // path map: home > browse > facets. Refine from access logs before treating
  // the derived user counts as gospel.
  mixed: [
    [5, () => GQL(Q.discovery)],
    [3, () => GQL(Q.search, { query: '', page: 1 + ((Math.random() * 5) | 0), includeNsfw: false })],
    [2, () => GQL(Q.updates, { page: 1 })],
    [1, () => GQL(Q.facets)],
  ],
};

// GraphQL operation text, copied from packages/api/src/operations.ts so the harness
// exercises exactly what the reader ships. Overridable via --queries <file.json>.
//
// SeriesFields is reproduced in full ON PURPOSE. It is the single most expensive
// thing in the API surface — every feed selects it, and it drags in the per-row
// `detectedAt` and `scan` resolvers. Trimming it here to "just id and title" would
// make the server look 5-10x faster than it is in production.
const SERIES_FIELDS = `fragment SeriesFields on Series {
  id title altTitles author artist description genres type status coverUrl
  sourceId chapterCount isMarked isNsfw
  rating { average count distribution }
  scan { avgIntervalHours overrideIntervalHours pollEveryMinutes paused
         statusOverride pausedOverride pollEveryMinutesOverride
         lastScannedAt nextScanAt }
  createdAt updatedAt latestChapterAt detectedAt
}`;

let Q = {
  discovery: `${SERIES_FIELDS}
query Discovery { discovery { kind title genre items { ...SeriesFields } } }`,

  updates: `${SERIES_FIELDS}
query Updates($page: Int) {
  updates(page: $page) { items { ...SeriesFields } page hasNextPage total }
}`,

  // NOTE — this is the DEPLOYED argument surface, which is NARROWER than the one
  // in packages/api/src/operations.ts. The running server rejects `sort`,
  // `hasChapters`, `types`, `status` and `contentRating` ("Unknown argument"), and
  // the BrowseSort enum does not exist on it at all. That is the server-before-
  // reader ordering the SEARCH comment in operations.ts warns about: Browse has no
  // fallback path, so shipping the reader first breaks it for everyone. Widen this
  // back to the full argument set only after the browse_catalogue server change is
  // deployed.
  search: `${SERIES_FIELDS}
query Search($query: String!, $page: Int, $genres: [String!], $includeNsfw: Boolean) {
  search(query: $query, page: $page, genres: $genres, includeNsfw: $includeNsfw) {
    items { ...SeriesFields } page hasNextPage total
  }
}`,

  facets: `query GenreFacets { genreFacets { genre count } }`,
};

// ---------------------------------------------------------------------------
// Arg parsing
// ---------------------------------------------------------------------------

const args = parseArgs(process.argv.slice(2));

function parseArgs(argv) {
  const out = {
    target: 'http://127.0.0.1:8080',
    scenario: 'health',
    vus: null,
    rate: null,
    duration: 10,
    warmup: 3,
    maxP95: 750,
    json: null,
    think: 5,
    list: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => argv[++i];
    switch (a) {
      case '--target': out.target = next(); break;
      case '--scenario': out.scenario = next(); break;
      case '--vus': out.vus = next().split(',').map((n) => parseInt(n, 10)); break;
      case '--rate': out.rate = parseFloat(next()); break;
      case '--duration': out.duration = parseFloat(next()); break;
      case '--warmup': out.warmup = parseFloat(next()); break;
      case '--max-p95': out.maxP95 = parseFloat(next()); break;
      case '--json': out.json = next(); break;
      case '--think': out.think = parseFloat(next()); break;
      case '--queries': Q = { ...Q, ...JSON.parse(readFileSync(next(), 'utf8')) }; break;
      case '--list': out.list = true; break;
      case '-h': case '--help': usage(); process.exit(0);
      default: die(`unknown arg: ${a}`);
    }
  }
  if (!out.vus && !out.rate) out.vus = [1, 2, 4, 8, 16, 32, 64];
  return out;
}

function usage() {
  console.log(`komika load generator

  --target <url>       base URL            (default http://127.0.0.1:8080)
  --scenario <name>    ${Object.keys(SCENARIOS).join(' | ')}
  --vus a,b,c          closed-loop ramp    (default 1,2,4,8,16,32,64)
  --rate <rps>         open-loop arrival rate (mutually exclusive with --vus)
  --duration <sec>     measured window per step   (default 10)
  --warmup <sec>       unmeasured window per step (default 3)
  --max-p95 <ms>       abort ramp above this p95  (default 750)
  --think <sec>        seconds between requests per real user, for the
                       req/s -> concurrent-users conversion (default 5)
  --queries <file>     JSON overriding the GraphQL operation text
  --json <file>        write full results as JSON
  --list               print scenarios and exit`);
}

function die(msg) { console.error(`error: ${msg}`); process.exit(1); }

// ---------------------------------------------------------------------------
// HTTP driver
// ---------------------------------------------------------------------------

const base = new URL(args.target);
const isTls = base.protocol === 'https:';
const mod = isTls ? https : http;

// One shared keep-alive Agent. maxSockets is raised per-step to the VU count:
// leaving it at Node's default would silently queue requests inside the client
// and we would be measuring our own generator, not the server.
const agent = new mod.Agent({ keepAlive: true, maxSockets: Infinity, maxFreeSockets: 4096 });

function once(req) {
  return new Promise((resolve) => {
    const t0 = process.hrtime.bigint();
    const opts = {
      protocol: base.protocol,
      hostname: base.hostname,
      port: base.port || (isTls ? 443 : 80),
      path: req.path,
      method: req.method,
      headers: { ...(req.headers || {}), connection: 'keep-alive' },
      agent,
    };
    const r = mod.request(opts, (res) => {
      let n = 0;
      let bad = false;
      res.on('data', (c) => {
        n += c.length;
        // A GraphQL error is HTTP 200 with an `errors` key. Counting those as
        // successes would make a broken query look like great throughput, so
        // sniff the body. Only the first chunk is inspected — enough, because
        // async-graphql serializes `errors` before `data`.
        if (!bad && n <= c.length && c.includes('"errors"')) bad = true;
      });
      res.on('end', () => {
        const us = Number(process.hrtime.bigint() - t0) / 1000;
        resolve({ ms: us / 1000, status: res.statusCode, bytes: n, gqlError: bad });
      });
    });
    r.on('error', (e) => {
      const us = Number(process.hrtime.bigint() - t0) / 1000;
      resolve({ ms: us / 1000, status: 0, bytes: 0, err: e.code || e.message });
    });
    if (req.body) r.write(req.body);
    r.end();
  });
}

function weightedPicker(spec) {
  const total = spec.reduce((s, [w]) => s + w, 0);
  return () => {
    let x = Math.random() * total;
    for (const [w, f] of spec) { if ((x -= w) <= 0) return f(); }
    return spec[spec.length - 1][1]();
  };
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

function summarize(lat, meta, wallSec) {
  const a = Float64Array.from(lat).sort();
  const q = (p) => (a.length ? a[Math.min(a.length - 1, Math.floor((p / 100) * a.length))] : NaN);
  const ok = meta.ok;
  return {
    requests: meta.total,
    ok,
    httpErrors: meta.http,
    gqlErrors: meta.gql,
    netErrors: meta.net,
    rps: +(meta.total / wallSec).toFixed(1),
    okRps: +(ok / wallSec).toFixed(1),
    bytesPerSec: Math.round(meta.bytes / wallSec),
    p50: +q(50).toFixed(2),
    p90: +q(90).toFixed(2),
    p95: +q(95).toFixed(2),
    p99: +q(99).toFixed(2),
    max: +(a.length ? a[a.length - 1] : NaN).toFixed(2),
    mean: +(a.length ? a.reduce((s, x) => s + x, 0) / a.length : NaN).toFixed(2),
  };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------------------
// Load shapes
// ---------------------------------------------------------------------------

async function closedLoop(makeReq, vus, seconds, collect) {
  const until = Date.now() + seconds * 1000;
  const worker = async () => {
    while (Date.now() < until) {
      const r = await once(makeReq());
      collect(r);
    }
  };
  await Promise.all(Array.from({ length: vus }, worker));
}

async function openLoop(makeReq, rps, seconds, collect) {
  // Fire on a fixed schedule regardless of completion. In-flight requests are
  // tracked so the run does not end while responses are still outstanding.
  const inflight = [];
  const gapMs = 1000 / rps;
  const start = Date.now();
  let sent = 0;
  while (Date.now() - start < seconds * 1000) {
    const due = start + sent * gapMs;
    const wait = due - Date.now();
    if (wait > 1) await sleep(wait);
    const p = once(makeReq()).then(collect);
    inflight.push(p);
    if (inflight.length > 50000) inflight.splice(0, 25000); // bound memory on long runs
    sent++;
  }
  await Promise.allSettled(inflight);
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

async function step(spec, { vus, rate }, seconds, warmupSec) {
  const makeReq = weightedPicker(spec);

  // Warmup is discarded: the first requests of a step pay TCP handshakes, SQLite
  // page-cache misses and JIT-ish warmup in the JSON path. Including them would
  // pull p99 up by an order of magnitude and understate steady-state capacity.
  if (warmupSec > 0) {
    const noop = () => {};
    if (rate) await openLoop(makeReq, rate, warmupSec, noop);
    else await closedLoop(makeReq, vus, warmupSec, noop);
  }

  const lat = [];
  const meta = { total: 0, ok: 0, http: 0, gql: 0, net: 0, bytes: 0 };
  const collect = (r) => {
    meta.total++;
    meta.bytes += r.bytes;
    lat.push(r.ms);
    if (r.status === 0) meta.net++;
    else if (r.status >= 400) meta.http++;
    else if (r.gqlError) meta.gql++;
    else meta.ok++;
  };

  const t0 = Date.now();
  if (rate) await openLoop(makeReq, rate, seconds, collect);
  else await closedLoop(makeReq, vus, seconds, collect);
  const wall = (Date.now() - t0) / 1000;

  return summarize(lat, meta, wall);
}

async function main() {
  if (args.list) {
    for (const [k, v] of Object.entries(SCENARIOS)) console.log(`${k.padEnd(10)} ${v.length} request type(s)`);
    return;
  }
  const spec = SCENARIOS[args.scenario];
  if (!spec) die(`unknown scenario '${args.scenario}' (try --list)`);

  console.log(`target    ${args.target}`);
  console.log(`scenario  ${args.scenario}`);
  console.log(`mode      ${args.rate ? `open-loop @ ${args.rate} rps` : `closed-loop ramp ${args.vus.join(',')}`}`);
  console.log(`window    ${args.warmup}s warmup + ${args.duration}s measured\n`);

  const head = ['load', 'rps', 'ok/s', 'p50', 'p90', 'p95', 'p99', 'max', 'err', 'users~'];
  console.log(head.map((h, i) => (i === 0 ? h.padEnd(9) : h.padStart(9))).join(''));
  console.log('-'.repeat(90));

  const results = [];
  const steps = args.rate ? [{ rate: args.rate }] : args.vus.map((v) => ({ vus: v }));

  for (const s of steps) {
    const r = await step(spec, s, args.duration, args.warmup);
    const label = s.rate ? `${s.rate}rps` : `${s.vus}vu`;
    const errs = r.httpErrors + r.gqlErrors + r.netErrors;
    // req/s -> concurrent users, assuming each real user emits one request every
    // `--think` seconds. This is the only step that turns a server metric into a
    // product metric, and it is only as good as that think-time assumption.
    const users = Math.round(r.okRps * args.think);
    results.push({ ...s, ...r, users });

    const row = [
      label.padEnd(9),
      String(r.rps).padStart(9),
      String(r.okRps).padStart(9),
      r.p50.toFixed(1).padStart(9),
      r.p90.toFixed(1).padStart(9),
      r.p95.toFixed(1).padStart(9),
      r.p99.toFixed(1).padStart(9),
      r.max.toFixed(1).padStart(9),
      String(errs).padStart(9),
      String(users).padStart(9),
    ].join('');
    console.log(row);

    // Prod guard. Stop climbing the moment the server is visibly hurting, so a
    // capacity probe never becomes an outage.
    if (r.p95 > args.maxP95) {
      console.log(`\n! p95 ${r.p95.toFixed(1)}ms exceeded --max-p95 ${args.maxP95}ms — stopping ramp.`);
      break;
    }
    if (errs > r.requests * 0.05) {
      console.log(`\n! error rate ${((errs / r.requests) * 100).toFixed(1)}% > 5% — stopping ramp.`);
      break;
    }
  }

  // The knee: highest ok/s observed. Past this, added concurrency buys latency,
  // not throughput.
  const best = results.reduce((a, b) => (b.okRps > a.okRps ? b : a), results[0]);
  console.log('\n' + '-'.repeat(90));
  console.log(`peak      ${best.okRps} ok req/s @ ${best.vus ? best.vus + ' VUs' : best.rate + ' rps'}  (p95 ${best.p95}ms)`);
  console.log(`implied   ~${best.users} concurrent users at ${args.think}s think-time`);

  if (args.json) {
    writeFileSync(args.json, JSON.stringify({ args: { ...args }, results }, null, 2));
    console.log(`\nwrote ${args.json}`);
  }
}

main().catch((e) => { console.error(e); process.exit(1); });
