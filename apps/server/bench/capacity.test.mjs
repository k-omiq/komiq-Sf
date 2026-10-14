// Tests for capacity.mjs. Zero dependencies: `node --test capacity.test.mjs`.
//
// A local mock server stands in for komika-server: GraphQL answers for every operation
// the probe sends, a Prometheus `/metrics` text endpoint whose counters move with the
// load, and image bytes on the own-origin and `/img` routes. The probe itself runs as a
// child process, the way an operator runs it, with short steps so the whole file takes
// seconds. Nothing here leaves 127.0.0.1.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, copyFileSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
	parsePrometheus,
	metricsWindow,
	deriveMetrics,
	seriesKey,
	isProductionHost,
	productionRefusal,
	makeRng,
	resolveImage,
	readSettings,
	redactCommand,
	imageClass,
	loadFixtures,
	inspect,
	setProductionAddresses,
	readProductionAddresses,
	loadProductionConfig,
	ProductionConfigError,
	PRODUCTION_UNCONFIGURED_WARNING,
	PRODUCTION_IPS_FILE,
} from './capacity.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const TOOL = join(HERE, 'capacity.mjs');

// The stand-in production address (RFC 5737 documentation range). The real one is
// configuration, never source; these tests neither need nor read it.
const PROD = '203.0.113.207';
setProductionAddresses([PROD]);

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

const WORDS = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf', 'hotel', 'india', 'juliet'];
const WORK_COUNT = 40;
const SLOW_WORK = 'w_7'; // series page slower than the test's --prewarm-max-ms
const BROKEN_WORK = 'w_8'; // series page answers a GraphQL error
const PAGE_BYTES = 3000;

function work(i) {
	return {
		id: `w_${i}`,
		workId: `w_${i}`,
		title: `${WORDS[i % WORDS.length]} ${WORDS[(i * 3 + 1) % WORDS.length]} story ${i}`,
		// Every fifth cover is a foreign MangaDex address, which goes through /img.
		coverUrl: i % 5 === 0 ? `https://uploads.mangadex.org/covers/m${i}/c.jpg` : `/covers/w_${i}.webp`,
		chapterCount: 3,
	};
}
const WORKS = Array.from({ length: WORK_COUNT }, (_, i) => work(i));

function fakeWebp(size) {
	const b = Buffer.alloc(size, 7);
	b.write('RIFF', 0, 'latin1');
	b.writeUInt32LE(size - 8, 4);
	b.write('WEBP', 8, 'latin1');
	return b;
}
const IMAGE = fakeWebp(PAGE_BYTES);

function startMock() {
	const m = {
		requests: 0,
		graphql: 0,
		images: 0,
		imageBytes: {},
		hits: 0,
		misses: 0,
		inflight: 0,
		maxInflight: 0,
		comments: 0,
		ops: {},
		seen: new Set(),
	};
	const answer = (op, vars, auth) => {
		const page = vars?.page ?? 1;
		const slice = (list) => list.slice((page - 1) * 20, page * 20);
		switch (op) {
			case 'Discovery':
				return { discovery: [{ kind: 'trending', title: 'Trending', genre: null, items: WORKS.slice(0, 10) }] };
			case 'UpdatesFeed':
				return { updatesFeed: { items: slice(WORKS), page, hasNextPage: page * 20 < WORKS.length, total: WORKS.length } };
			case 'Search': {
				const q = String(vars?.query ?? '');
				const list = q ? WORKS.filter((w) => w.title.includes(q.split(' ')[0])) : WORKS;
				return { search: { items: slice(list), page, hasNextPage: page * 20 < list.length, total: list.length } };
			}
			case 'SeriesPage': {
				const id = vars.workId;
				const i = Number(id.slice(2));
				return {
					seriesPage: {
						workId: id,
						series: { ...WORKS[i] },
						spineChapters: [1, 2, 3].map((n) => ({ id: `${id}-c${n}`, number: String(n), pageCount: 4 })),
						sources: [],
					},
				};
			}
			case 'CanonicalPages':
			case 'Pages': {
				const [wid, c] = String(vars.chapterId).split('-c');
				const mid = Number(String(wid).slice(2));
				const list = [0, 1, 2, 3].map((p) => ({ index: p, sourceUrl: `/api/v1/manga/${mid}/chapter/${c}/page/${p}` }));
				return op === 'Pages' ? { pages: list } : { canonicalPages: list };
			}
			case 'Register':
				return { register: { token: `t-${vars.input.username}`, user: { id: vars.input.username, username: vars.input.username } } };
			case 'Login':
				return { login: { token: `t-${vars.username}`, user: { id: vars.username, username: vars.username } } };
			case 'Session': {
				const name = String(auth ?? '').replace(/^Bearer t-/, '');
				return { session: name ? { token: `t-${name}`, user: { id: name, username: name } } : null };
			}
			case 'SetProgress':
				return { setProgress: true };
			case 'Mark':
				return { mark: { id: vars.seriesId, isMarked: vars.marked } };
			case 'RecordView':
				return { recordView: true };
			case 'Comments':
			case 'Reviews':
				return { [op === 'Comments' ? 'comments' : 'reviews']: { items: [], page: 1, hasNextPage: false, total: 0 } };
			case 'MyReview':
				return { myReview: null };
			case 'PostComment':
				return { postComment: { id: `cm${++m.comments}` } };
			case 'DeleteComment':
				return { deleteComment: true };
			default:
				return null;
		}
	};
	const metricsText = () =>
		[
			'# HELP komika_mock_requests_total Requests by lane',
			'# TYPE komika_mock_requests_total counter',
			`komika_mock_requests_total{lane="graphql"} ${m.graphql}`,
			`komika_mock_requests_total{lane="media"} ${m.images}`,
			'# TYPE komika_image_cache_hits_total counter',
			`komika_image_cache_hits_total{cache="pages"} ${m.hits}`,
			'# TYPE komika_image_cache_misses_total counter',
			`komika_image_cache_misses_total{cache="pages"} ${m.misses}`,
			'# TYPE komika_admission_refused_total counter',
			`komika_admission_refused_total{lane="local"} ${Math.floor(m.graphql / 5)}`,
			'komika_admission_refused_total{lane="global"} 0',
			'# TYPE komika_media_bytes_total counter',
			...Object.entries(m.imageBytes).map(([route, n]) => `komika_media_bytes_total{route="${route}"} ${n}`),
			'# TYPE komika_inflight gauge',
			`komika_inflight{lane="graphql"} ${m.inflight}`,
			`komika_inflight_max ${m.maxInflight}`,
			'# TYPE komika_scan_state_total gauge',
			'komika_scan_state_total 5',
			'',
		].join('\n');

	const server = http.createServer((req, res) => {
		m.requests++;
		const url = new URL(req.url, 'http://mock');
		if (url.pathname === '/metrics') {
			res.writeHead(200, { 'content-type': 'text/plain; version=0.0.4' });
			res.end(metricsText());
			return;
		}
		const route = url.pathname.startsWith('/covers/')
			? 'covers'
			: url.pathname.startsWith('/api/v1/manga/')
				? 'engine'
				: url.pathname === '/img'
					? 'img'
					: url.pathname.startsWith('/fixture/')
						? 'fixture'
						: null;
		if (route) {
			m.images++;
			const key = url.pathname + url.search;
			const hit = m.seen.has(key);
			m.seen.add(key);
			if (hit) m.hits++;
			else m.misses++;
			m.imageBytes[route] = (m.imageBytes[route] ?? 0) + IMAGE.length;
			res.writeHead(200, {
				'content-type': 'image/webp',
				'content-length': IMAGE.length,
				'cache-control': 'public, max-age=60',
				'x-cache': hit ? 'HIT' : 'MISS',
			});
			res.end(IMAGE);
			return;
		}
		if (url.pathname !== '/graphql') {
			res.writeHead(404).end('not found');
			return;
		}
		m.graphql++;
		m.inflight++;
		m.maxInflight = Math.max(m.maxInflight, m.inflight);
		const chunks = [];
		req.on('data', (c) => chunks.push(c));
		req.on('end', () => {
			let query;
			let vars;
			if (req.method === 'GET') {
				query = url.searchParams.get('query');
				vars = JSON.parse(url.searchParams.get('variables') ?? 'null');
			} else {
				const body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
				query = body.query;
				vars = body.variables;
			}
			// The definition line, not a word inside one of the operation's # comments.
			const op = /^\s*(?:query|mutation)\s+(\w+)/m.exec(query)?.[1] ?? 'unknown';
			m.ops[op] = (m.ops[op] ?? 0) + 1;
			const reply = (status, obj) => {
				m.inflight--;
				res.writeHead(status, { 'content-type': 'application/json' });
				res.end(JSON.stringify(obj));
			};
			// m.bad: answers a broken edge or proxy gives, which must count as errors.
			if (m.bad && op === 'Discovery') {
				m.inflight--;
				res.writeHead(200, { 'content-type': 'text/html' });
				res.end('<html><body>Sign in</body></html>');
				return;
			}
			if (m.bad && op === 'UpdatesFeed') {
				m.inflight--;
				res.writeHead(302, { location: '/login' });
				res.end();
				return;
			}
			if (op === 'SeriesPage' && vars?.workId === BROKEN_WORK) {
				reply(200, { data: null, errors: [{ message: 'upstream failed' }] });
				return;
			}
			const data = answer(op, vars, req.headers.authorization);
			const delay = op === 'SeriesPage' && vars?.workId === SLOW_WORK ? 250 : 0;
			setTimeout(() => reply(200, data ? { data } : { errors: [{ message: `unknown operation ${op}` }] }), delay);
		});
	});
	return new Promise((resolve) => {
		server.listen(0, '127.0.0.1', () => {
			m.port = server.address().port;
			m.url = `http://127.0.0.1:${m.port}`;
			m.close = () => new Promise((r) => server.close(r));
			resolve(m);
		});
	});
}

/** Run the probe; resolves with { code, stdout, stderr }. */
function runTool(argv, timeoutMs = 60_000, { tool = TOOL, env = { ...process.env, KOMIKA_PRODUCTION_IPS: PROD } } = {}) {
	return new Promise((resolve, reject) => {
		const child = spawn(process.execPath, [tool, ...argv], { stdio: ['ignore', 'pipe', 'pipe'], env });
		let stdout = '';
		let stderr = '';
		child.stdout.on('data', (c) => (stdout += c));
		child.stderr.on('data', (c) => (stderr += c));
		const timer = setTimeout(() => {
			child.kill('SIGKILL');
			reject(new Error(`timed out\n${stdout}\n${stderr}`));
		}, timeoutMs);
		child.on('close', (code) => {
			clearTimeout(timer);
			resolve({ code, stdout, stderr });
		});
	});
}

const tmp = () => mkdtempSync(join(tmpdir(), 'capacity-test-'));
// Fast setup against the mock: no pacing to speak of, one slow work left out.
const FAST = ['--setup-rate', '2000', '--setup-concurrency', '4', '--prewarm-max-ms', '120', '--pool', '16'];

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

test('Prometheus text: labels, escapes, TYPE lines and suffix rules', () => {
	const text = [
		'# HELP komika_x_total a counter',
		'# TYPE komika_x_total counter',
		'komika_x_total{lane="local",kind="a\\"b"} 12',
		'komika_x_total{kind="z", lane="global"} 3 1700000000000',
		'# TYPE komika_scan_state_total gauge',
		'komika_scan_state_total 5',
		'# TYPE komika_lat_seconds histogram',
		'komika_lat_seconds_bucket{le="0.1"} 4',
		'komika_lat_seconds_bucket{le="+Inf"} 9',
		'komika_lat_seconds_sum 1.5',
		'komika_lat_seconds_count 9',
		'untyped_thing_total 7',
		'untyped_level 2.5',
		'weird_value NaN',
		'not a sample line {',
	].join('\n');
	const { series } = parsePrometheus(text);
	const a = series.get('komika_x_total{kind="a\\"b",lane="local"}');
	assert.ok(a, [...series.keys()].join(' '));
	assert.deepEqual(a.labels, { lane: 'local', kind: 'a"b' });
	assert.equal(a.value, 12);
	assert.equal(a.type, 'counter');
	assert.equal(series.get('komika_x_total{kind="z",lane="global"}').value, 3);
	// Declared a gauge: stays one although the name ends in _total.
	assert.equal(series.get('komika_scan_state_total').type, 'gauge');
	assert.equal(series.get('komika_lat_seconds_bucket{le="+Inf"}').type, 'counter');
	assert.equal(series.get('komika_lat_seconds_sum').type, 'counter');
	assert.equal(series.get('komika_lat_seconds_count').type, 'counter');
	assert.equal(series.get('untyped_thing_total').type, 'counter');
	assert.equal(series.get('untyped_level').type, 'gauge');
	assert.ok(Number.isNaN(series.get('weird_value').value));
	assert.equal(series.size, 10);
	assert.equal(seriesKey('m', { b: '2', a: '1' }), 'm{a="1",b="2"}');
});

test('metrics window: counter deltas, resets, gauge first/last/min/max, derived', () => {
	const scrape = (phase, at, body) => ({ phase, at, ok: true, parsed: parsePrometheus(body) });
	const t = (reqs, hits, misses, refused, inflight, bytes, restarted) =>
		[
			'# TYPE komika_req_total counter',
			`komika_req_total{lane="graphql"} ${reqs}`,
			`komika_cache_hits_total{cache="series"} ${hits}`,
			`komika_cache_misses_total{cache="series"} ${misses}`,
			`komika_lookup_total{cache="rows",result="hit"} ${hits * 2}`,
			`komika_lookup_total{cache="rows",result="miss"} ${misses * 2}`,
			`komika_admission_refused_total{lane="local"} ${refused}`,
			'komika_admission_refused_total{lane="global"} 1',
			`komika_media_bytes_total{route="pages"} ${bytes}`,
			`komika_restarts_total ${restarted}`,
			'# TYPE komika_inflight gauge',
			`komika_inflight{lane="graphql"} ${inflight}`,
		].join('\n');
	const w = metricsWindow([
		scrape('before', 1000, t(100, 10, 10, 0, 3, 0, 50)),
		scrape('sample', 2000, t(150, 40, 20, 2, 9, 1000, 2)),
		{ phase: 'sample', at: 2500, ok: false, error: 'timeout' },
		scrape('after', 3000, t(200, 70, 30, 4, 1, 5000, 4)),
	]);
	assert.equal(w.scrapes, 4);
	assert.equal(w.good, 3);
	assert.deepEqual(w.failures, [{ phase: 'sample', at: 2500, error: 'timeout' }]);
	assert.equal(w.counters['komika_req_total{lane="graphql"}'], 100);
	assert.equal(w.counters['komika_admission_refused_total{lane="local"}'], 4);
	assert.equal(w.counters['komika_admission_refused_total{lane="global"}'], 0);
	// 50 -> 4 is a restart: the delta is the last value, and it is listed.
	assert.equal(w.counters.komika_restarts_total, 4);
	assert.deepEqual(w.resets, ['komika_restarts_total']);
	assert.deepEqual(w.gauges['komika_inflight{lane="graphql"}'], { first: 3, last: 1, min: 1, max: 9, samples: 3 });

	const d = w.derived;
	const byKey = Object.fromEntries(d.hitRatios.map((h) => [h.key, h]));
	assert.deepEqual(byKey['komika_cache_*_total{cache="series"}'], {
		key: 'komika_cache_*_total{cache="series"}',
		unit: 'requests',
		hits: 60,
		misses: 20,
		ratio: 0.75,
	});
	assert.equal(byKey['komika_lookup_total{cache="rows",result="*"}'].ratio, 0.75);
	assert.deepEqual(d.refusingLanes, ['local']);
	assert.equal(d.bytes[0].series, 'komika_media_bytes_total{route="pages"}');
	assert.equal(d.bytes[0].delta, 5000);

	// A byte hit/miss pair is reported in bytes.
	const meta = new Map([
		['x_hit_bytes_total', { name: 'x_hit_bytes_total', labels: {} }],
		['x_miss_bytes_total', { name: 'x_miss_bytes_total', labels: {} }],
	]);
	const bd = deriveMetrics({ x_hit_bytes_total: 900, x_miss_bytes_total: 100 }, meta);
	assert.equal(bd.hitRatios[0].unit, 'bytes');
	assert.equal(bd.hitRatios[0].ratio, 0.9);
	// A refusal told by a label value, not by the name.
	const lk = 'komika_admission_total{lane="upstream",outcome="refused"}';
	const ld = deriveMetrics({ [lk]: 3 }, new Map([[lk, { name: 'komika_admission_total', labels: { lane: 'upstream', outcome: 'refused' } }]]));
	assert.deepEqual(ld.refusingLanes, ['upstream']);

	// A reset BETWEEN samples: 100 -> 150 -> 20 -> 120 is 50 + 20 + 100, and flagged.
	const c = (v) => `# TYPE c_total counter\nc_total ${v}`;
	const mid = metricsWindow([scrape('before', 1, c(100)), scrape('sample', 2, c(150)), scrape('sample', 3, c(20)), scrape('after', 4, c(120))]);
	assert.equal(mid.counters.c_total, 170);
	assert.equal(mid.reset, true);
	assert.deepEqual(mid.resets, ['c_total']);
	assert.equal(w.reset, true);
	const clean = metricsWindow([scrape('before', 1, c(1)), scrape('after', 2, c(9))]);
	assert.equal(clean.counters.c_total, 8);
	assert.equal(clean.reset, false);

	// One good scrape: gauges, but no counter delta can be honest.
	const one = metricsWindow([scrape('after', 1, t(1, 1, 1, 1, 1, 1, 1))]);
	assert.equal(one.counters, null);
	assert.ok(one.gauges['komika_inflight{lane="graphql"}']);
});

test('production hosts are recognised in every form', () => {
	for (const h of [
		'komiq.cc',
		'KOMIQ.CC.',
		'api.komiq.cc',
		'img.komiq.cc',
		'203.0.113.207',
		'[::ffff:203.0.113.207]',
		// What WHATWG URL parsing turns the mapped form into, and other spellings.
		'[::ffff:cb00:71cf]',
		'::ffff:cb00:71cf',
		'[::203.0.113.207]',
		'64:ff9b::cb00:71cf',
		'2002:cb00:71cf::1',
		'3405803983',
		'0xcb0071cf',
		'0xcb.0x0.0x71.0xcf',
		'0313.0.0161.0317',
	])
		assert.equal(isProductionHost(h), true, h);
	for (const h of ['komiq.cc.example.org', 'notkomiq.cc', '198.51.100.6', '127.0.0.1', 'localhost', '[::1]', '::ffff:cb00:71ce', '203.0.113.206'])
		assert.equal(isProductionHost(h), false, h);
	// The URL parser really does rewrite the mapped address; the check must still see it.
	assert.equal(new URL('http://[::ffff:203.0.113.207]/').hostname, '[::ffff:cb00:71cf]');
	const base = { target: 'http://127.0.0.1:8080' };
	assert.equal(productionRefusal(base), null);
	assert.match(productionRefusal({ ...base, target: 'https://api.komiq.cc' }), /--target/);
	assert.match(productionRefusal({ ...base, target: 'http://3405803983:8080' }), /203\.0\.113\.207/);
	assert.match(productionRefusal({ ...base, target: 'http://[::ffff:203.0.113.207]:8080/' }), /--target/);
	assert.match(productionRefusal({ ...base, metricsUrl: 'http://komiq.cc/metrics' }), /--metrics-url/);
	assert.match(productionRefusal({ ...base, imgBase: 'https://img.komiq.cc' }), /--img-base/);
	assert.match(productionRefusal({ ...base, fixtureImages: 'https://x.komiq.cc/f/{n}.webp' }), /--fixture-images/);
	assert.match(productionRefusal(base, ['http://[::ffff:cb00:71cf]']), /--fixture-images/);
	assert.match(productionRefusal(base, ['http://fixtures.test/?src=https://api.komiq.cc/x']), /query/);
	for (const cmd of [
		'ssh ubuntu@203.0.113.207 ./measure.sh',
		'ssh api.komiq.cc true',
		'ssh 3405803983 true',
		'ssh -p 22 ubuntu@[::ffff:cb00:71cf] true',
		'curl http://0313.0.0161.0317:9091/metrics',
	])
		assert.match(productionRefusal({ ...base, hostCmd: cmd }) ?? '', /--host-cmd/, cmd);
	assert.equal(productionRefusal({ ...base, hostCmd: 'ssh testhost ~/measure.sh --json {label} {seconds}' }), null);
	assert.equal(productionRefusal({ ...base, hostCmd: 'ssh ubuntu@203.0.113.206 ./measure.sh' }), null);
});

test('production addresses come from the environment and the local file, joined', () => {
	const dir = tmp();
	const none = join(dir, 'absent.local');
	const file = join(dir, 'production-ips.local');
	try {
		// The default file sits in deploy/, next to watch.sh, which reads the same one.
		assert.equal(PRODUCTION_IPS_FILE, join(HERE, '..', '..', '..', 'deploy', 'production-ips.local'));
		// Environment: commas and/or whitespace, duplicates once.
		assert.deepEqual(readProductionAddresses({ env: { KOMIKA_PRODUCTION_IPS: ' 203.0.113.1, 203.0.113.2\t203.0.113.3,,203.0.113.1\n' }, file: none }), [
			'203.0.113.1',
			'203.0.113.2',
			'203.0.113.3',
		]);
		assert.deepEqual(readProductionAddresses({ env: {}, file: none }), []);
		assert.deepEqual(readProductionAddresses({ env: { KOMIKA_PRODUCTION_IPS: '  ' }, file: none }), []);
		// File: one per line, # comments, blank lines, CRLF.
		writeFileSync(file, '# the production host\n198.51.100.7\r\n\n  198.51.100.8  # second\n#198.51.100.9\n');
		assert.deepEqual(readProductionAddresses({ env: {}, file }), ['198.51.100.7', '198.51.100.8']);
		// Both: the union.
		assert.deepEqual(readProductionAddresses({ env: { KOMIKA_PRODUCTION_IPS: '203.0.113.1,198.51.100.7' }, file }), [
			'203.0.113.1',
			'198.51.100.7',
			'198.51.100.8',
		]);
		// Anything that is not a plain IPv4 address fails closed, naming the entry.
		for (const bad of ['komiq.cc', '203.0.113', '203.0.113.256', '203.0.113.07', '0xcb0071cf', '::ffff:cb00:71cf', '203.0.113.1/32'])
			assert.throws(() => readProductionAddresses({ env: { KOMIKA_PRODUCTION_IPS: `203.0.113.1,${bad}` }, file: none }), (e) => {
				assert.ok(e instanceof ProductionConfigError, String(e));
				assert.ok(e.message.includes(`'${bad}'`), e.message);
				return true;
			});
		writeFileSync(file, '198.51.100.7\n198.51.100.8 198.51.100.9\n');
		assert.throws(() => readProductionAddresses({ env: {}, file }), /production-ips\.local:2: '198\.51\.100\.8 198\.51\.100\.9'/);
		// A file that exists but cannot be read is not "no addresses".
		assert.throws(() => readProductionAddresses({ env: {}, file: dir }), ProductionConfigError);
		// Use it: the configured addresses are refused, and nothing is warned.
		const warned = [];
		writeFileSync(file, '198.51.100.7\n');
		loadProductionConfig({ env: { KOMIKA_PRODUCTION_IPS: '203.0.113.1' }, file, warn: (m) => warned.push(m) });
		assert.deepEqual(warned, []);
		for (const h of ['198.51.100.7', '203.0.113.1', '[::ffff:c633:6407]']) assert.equal(isProductionHost(h), true, h);
		assert.equal(isProductionHost(PROD), false);
		// Nothing configured: komiq.cc names are still refused, and one warning says so.
		loadProductionConfig({ env: {}, file: none, warn: (m) => warned.push(m) });
		assert.deepEqual(warned, [PRODUCTION_UNCONFIGURED_WARNING]);
		assert.match(warned[0], /only komiq\.cc and \*\.komiq\.cc are refused/);
		for (const h of ['komiq.cc', 'api.komiq.cc']) assert.equal(isProductionHost(h), true, h);
		assert.match(productionRefusal({ target: 'https://api.komiq.cc' }) ?? '', /--target/);
		assert.equal(productionRefusal({ target: 'http://203.0.113.1:8080' }), null);
	} finally {
		setProductionAddresses([PROD]);
		rmSync(dir, { recursive: true, force: true });
	}
});

test('host command text is redacted before it is recorded', () => {
	const r = redactCommand(
		'PGPASSWORD=hunter3 sshpass -p s3cret ssh x; curl https://u:pw9@h/m --password=abc API_TOKEN="q w" keep=me',
	);
	for (const secret of ['hunter3', 's3cret', 'pw9', 'abc', 'q w']) assert.ok(!r.includes(secret), `${secret} in ${r}`);
	assert.match(r, /PGPASSWORD=<redacted>/);
	assert.match(r, /sshpass -p <redacted>/);
	assert.match(r, /https:\/\/u:<redacted>@h\/m/);
	assert.match(r, /keep=me/);
});

test('image policy: store covers and fixtures; everything else is real-source', () => {
	const target = new URL('http://127.0.0.1:9000');
	const img = new URL('http://127.0.0.1:9000');
	assert.deepEqual(resolveImage('/covers/w_1.webp', target, img), {
		url: 'http://127.0.0.1:9000/covers/w_1.webp?cache_policy=2',
		route: 'covers',
		cls: 'store',
	});
	const page = resolveImage('/api/v1/manga/4/chapter/2/page/0', target, img);
	assert.equal(page.route, 'engine-page');
	assert.equal(page.cls, 'real');
	assert.equal(resolveImage('/api/v1/manga/4/thumbnail', target, img).cls, 'real');
	const foreign = resolveImage('https://uploads.mangadex.org/covers/a/b.jpg', target, img);
	assert.equal(foreign.route, 'img-proxy');
	assert.equal(foreign.cls, 'real');
	assert.equal(foreign.url, `http://127.0.0.1:9000/img?src=${encodeURIComponent('https://uploads.mangadex.org/covers/a/b.jpg')}&cache_policy=2`);
	// A cover route with a nested source in its query is not a store read.
	assert.equal(resolveImage('/covers/w_1.webp?src=https://uploads.mangadex.org/x', target, img).cls, 'real');
	// A production host anywhere in the query is never fetched, proxied or not.
	assert.equal(resolveImage(`/img?src=${encodeURIComponent('https://api.komiq.cc/api/v1/manga/1/thumbnail')}`, target, img).skip, 'production-in-query');
	assert.equal(resolveImage('https://uploads.mangadex.org/x.jpg?u=203.0.113.207', target, img).skip, 'production-in-query');
	assert.equal(resolveImage('https://uploads.mangadex.org/x.jpg?u=203.0.113.206', target, img).skip, undefined);
	// A production page address is rewritten to the target, and is still real-source.
	assert.equal(resolveImage('https://api.komiq.cc/api/v1/manga/7/chapter/1/page/0', target, img).cls, 'real');
	// An own-asset address on production is loaded from the target, never from production.
	const prod = resolveImage('https://api.komiq.cc/api/v1/manga/7/thumbnail', target, img);
	assert.equal(prod.url, 'http://127.0.0.1:9000/api/v1/manga/7/thumbnail?cache_policy=2');
	assert.equal(prod.rewrittenFrom, 'api.komiq.cc');
	assert.equal(resolveImage('https://api.komiq.cc/other', target, img).skip, 'production-host');
	assert.equal(resolveImage('', target, img).skip, 'empty');
	assert.equal(imageClass('fixture', new URL('http://f.test/1.webp')), 'fixture');
});

test('fixtures: fragment-only templates refused, file addresses deduplicated on the request address', () => {
	const target = new URL('http://127.0.0.1:9000');
	assert.throws(() => loadFixtures('http://f.test/a.webp#{n}', 'r', target), /fragment/);
	assert.throws(() => loadFixtures('http://f{n}.test/a.webp', 'r', target), /host/);
	assert.throws(() => loadFixtures('http://f.test/a.webp', 'r', target), /\{n\}/);
	const t = loadFixtures('/fixture/{n}.webp?r={run}#x', 'run1', target, 5);
	t.setStandIns(3);
	assert.equal(t.standIn(0).url, 'http://127.0.0.1:9000/fixture/5.webp?r=run1');
	assert.equal(t.coldNext().url, 'http://127.0.0.1:9000/fixture/8.webp?r=run1');
	const dir = tmp();
	try {
		const f = join(dir, 'f.txt');
		writeFileSync(f, '/fixture/1.webp\n/fixture/1.webp#again\nhttp://127.0.0.1:9000/fixture/1.webp\n/fixture/2.webp\n# comment\n/fixture/3.webp\n/fixture/4.webp\n');
		const fx = loadFixtures(f, 'r', target);
		fx.setStandIns(10);
		const rep = fx.report();
		assert.equal(rep.urls, 4);
		assert.equal(rep.duplicatesDropped, 2);
		assert.equal(rep.standIns, 2); // at most half, so cold addresses remain
		const cold = [fx.coldNext(), fx.coldNext(), fx.coldNext()];
		assert.deepEqual(cold.map((c) => c?.url ?? null), ['http://127.0.0.1:9000/fixture/3.webp', 'http://127.0.0.1:9000/fixture/4.webp', null]);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test('GraphQL answers need 2xx and a JSON envelope', () => {
	const req = { label: 'x' };
	const k = (status, body) => inspect(req, status, 1, body.length, Buffer.from(body)).kind;
	assert.equal(k(200, '{"data":{"a":1}}'), null);
	assert.equal(k(200, '{"data":null,"errors":[{"message":"no"}]}'), 'graphql');
	assert.equal(k(200, ''), 'bad_envelope');
	assert.equal(k(200, '<html>login</html>'), 'bad_envelope');
	assert.equal(k(200, '{"data":{"a":'), 'bad_envelope'); // truncated
	assert.equal(k(200, '[1,2]'), 'bad_envelope');
	assert.equal(k(200, '{"ok":true}'), 'bad_envelope');
	assert.equal(k(302, ''), 'http_302');
	assert.equal(k(304, ''), 'http_304');
	assert.equal(k(503, '{"errors":[{"message":"Server busy"}]}'), 'http_503');
});

test('seeded randomness repeats', () => {
	const a = makeRng('42', 'pool');
	const b = makeRng('42', 'pool');
	const c = makeRng('43', 'pool');
	const xs = Array.from({ length: 5 }, () => a());
	assert.deepEqual(xs, Array.from({ length: 5 }, () => b()));
	assert.notDeepEqual(xs, Array.from({ length: 5 }, () => c()));
	assert.ok(xs.every((x) => x >= 0 && x < 1));
});

test('settings capture keeps secrets out', () => {
	const dir = tmp();
	try {
		const f = join(dir, 'server.env');
		writeFileSync(
			f,
			'# captured\nDATABASE_URL=postgres://komika:hunter2@db:5432/komika\nexport KOMIKA_REGISTRATION_OPEN="on"\nSESSION_SECRET=abc123\nDATABASE_READ_POOL_SIZE=4\n',
		);
		const s = readSettings(f);
		assert.equal(s.format, 'env');
		assert.equal(s.entries, 4);
		assert.equal(s.values.KOMIKA_REGISTRATION_OPEN, 'on');
		assert.equal(s.values.DATABASE_READ_POOL_SIZE, '4');
		assert.equal(s.values.SESSION_SECRET, '<redacted>');
		assert.equal(s.values.DATABASE_URL, 'postgres://komika:<redacted>@db:5432/komika');
		assert.ok(!JSON.stringify(s).includes('hunter2'));
		assert.ok(!JSON.stringify(s).includes('abc123'));
		assert.match(s.sha256, /^[0-9a-f]{64}$/);
		// Same settings in another order and layout: same hash. A changed value: another.
		writeFileSync(f, 'DATABASE_READ_POOL_SIZE=4\nSESSION_SECRET=abc123\nKOMIKA_REGISTRATION_OPEN=on\nDATABASE_URL=postgres://komika:hunter2@db:5432/komika\n');
		assert.equal(readSettings(f).sha256, s.sha256);
		writeFileSync(f, 'DATABASE_READ_POOL_SIZE=8\nSESSION_SECRET=abc123\nKOMIKA_REGISTRATION_OPEN=on\nDATABASE_URL=postgres://komika:hunter2@db:5432/komika\n');
		assert.notEqual(readSettings(f).sha256, s.sha256);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

// ---------------------------------------------------------------------------
// The tool as a process, against the mock
// ---------------------------------------------------------------------------

test('refuses production before sending anything', async () => {
	const mock = await startMock();
	const dir = tmp();
	const prodFixtures = join(dir, 'fixtures.txt');
	writeFileSync(prodFixtures, `${mock.url}/fixture/1.webp\nhttp://0xcb0071cf/fixture/2.webp\n`);
	try {
		for (const extra of [
			['--target', 'https://api.komiq.cc'],
			['--target', mock.url, '--metrics-url', 'http://203.0.113.207:9091/metrics'],
			['--target', mock.url, '--img-base', 'https://img.komiq.cc'],
			['--target', mock.url, '--host-cmd', 'ssh komiq.cc ./measure.sh {label} {seconds}'],
			['--target', mock.url, '--host-cmd', 'ssh ubuntu@3405803983 ./measure.sh {label} {seconds}'],
			['--target', 'http://[::ffff:203.0.113.207]:8080'],
			['--target', mock.url, '--fixture-images', 'http://[::ffff:cb00:71cf]/f/{n}.webp'],
			['--target', mock.url, '--fixture-images', prodFixtures],
		]) {
			const r = await runTool([...extra, '--scenario', 'read', '--vus', '1', '--duration', '0.2']);
			assert.equal(r.code, 3, r.stderr);
			assert.match(r.stderr, /refusing to run: .*production/);
			assert.match(r.stderr, /no override/);
		}
		assert.equal(mock.requests, 0);
	} finally {
		await mock.close();
		rmSync(dir, { recursive: true, force: true });
	}
});

test('the tool reads the production addresses from the environment and deploy/production-ips.local', async () => {
	// A copy of the tool in a scratch tree, so its deploy/production-ips.local is ours and
	// the real one (present or not) plays no part.
	const dir = tmp();
	const tool = join(dir, 'apps', 'server', 'bench', 'capacity.mjs');
	mkdirSync(dirname(tool), { recursive: true });
	mkdirSync(join(dir, 'deploy'));
	copyFileSync(TOOL, tool);
	const file = join(dir, 'deploy', 'production-ips.local');
	const env = { ...process.env };
	delete env.KOMIKA_PRODUCTION_IPS;
	// --list exits after the production checks, before any request or lookup.
	const list = (target, extra = {}) => runTool(['--target', target, '--list'], 30_000, { tool, env: { ...env, ...extra } });
	const warning = /warning: no production address is configured/g;
	try {
		// Nothing configured: the tool runs, komiq.cc is still refused, one warning.
		let r = await list('http://203.0.113.207:8080');
		assert.equal(r.code, 0, r.stderr);
		assert.equal(r.stderr.match(warning)?.length, 1, r.stderr);
		r = await list('https://api.komiq.cc');
		assert.equal(r.code, 3, r.stderr);
		assert.match(r.stderr, /refusing to run: --target .*production/);
		assert.equal(r.stderr.match(warning)?.length, 1, r.stderr);
		// The file, with a comment.
		writeFileSync(file, '# stand-in production host\n203.0.113.207\n');
		r = await list('http://0xcb.0x0.0x71.0xcf:8080');
		assert.equal(r.code, 3, r.stderr);
		assert.match(r.stderr, /refusing to run: --target .*production \(203\.0\.113\.207\)/);
		assert.doesNotMatch(r.stderr, warning);
		r = await list('http://203.0.113.206:8080');
		assert.equal(r.code, 0, r.stderr);
		// The environment adds to the file.
		r = await list('http://198.51.100.6:8080', { KOMIKA_PRODUCTION_IPS: '198.51.100.5, 198.51.100.6' });
		assert.equal(r.code, 3, r.stderr);
		r = await list('http://[::ffff:203.0.113.207]:8080', { KOMIKA_PRODUCTION_IPS: '198.51.100.6' });
		assert.equal(r.code, 3, r.stderr);
		// A bad entry stops the tool, whatever the target.
		r = await list('http://127.0.0.1:9', { KOMIKA_PRODUCTION_IPS: 'production.example' });
		assert.equal(r.code, 3, r.stderr);
		assert.match(r.stderr, /refusing to run: production address configuration: not an IPv4 address: KOMIKA_PRODUCTION_IPS: 'production\.example'/);
		writeFileSync(file, '203.0.113.207\n203.0.113.300 # typo\n');
		r = await list('http://127.0.0.1:9');
		assert.equal(r.code, 3, r.stderr);
		assert.match(r.stderr, /production-ips\.local:2: '203\.0\.113\.300'/);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
});

test('bad options are refused before anything is sent', async () => {
	const mock = await startMock();
	try {
		for (const [extra, why] of [
			[['--vus', '1', '--real-source-sample', '201'], /real-source-sample/],
			[['--vus', '1', '--real-source-sample', '2.5'], /real-source-sample/],
			[['--vus', '1', '--scenario', 'image-cold', '--fixture-images', `${mock.url}/fixture/a.webp#{n}`], /fragment/],
			[['--users', '1', '--scenario', 'mixed'], /page images need --fixture-images/],
		]) {
			const r = await runTool(['--target', mock.url, '--duration', '0.2', ...extra]);
			assert.equal(r.code, 2, `${extra.join(' ')}: ${r.stderr}`);
			assert.match(r.stderr, why);
		}
		assert.equal(mock.requests, 0);
	} finally {
		await mock.close();
	}
});

test('HTML and redirect answers to GraphQL are errors, not successes', async () => {
	const mock = await startMock();
	const dir = tmp();
	try {
		mock.bad = true;
		const out = join(dir, 'run.json');
		const r = await runTool([
			'--target', mock.url, ...FAST, '--seed', 'b', '--scenario', 'read', '--vus', '2',
			'--warmup', '0', '--duration', '0.5', '--max-errors', '1',
			'--mix', 'discovery=1,updates=1,latestAdded=0,browse=0,series=0,search=0', '--json', out,
		]);
		assert.equal(r.code, 0, r.stderr + r.stdout);
		const p = JSON.parse(readFileSync(out, 'utf8')).steps[0].loads[0];
		assert.ok(p.errorsByKind.bad_envelope > 0, JSON.stringify(p.errorsByKind));
		assert.ok(p.errorsByKind.http_302 > 0, JSON.stringify(p.errorsByKind));
		assert.equal(p.read.ok, 0);
		assert.equal(p.errors, p.requests);
	} finally {
		await mock.close();
		rmSync(dir, { recursive: true, force: true });
	}
});

test('image-cold refuses to start without --fixture-images', async () => {
	const mock = await startMock();
	try {
		const r = await runTool(['--target', mock.url, '--scenario', 'image-cold', '--vus', '1', '--duration', '0.2']);
		assert.equal(r.code, 2);
		assert.match(r.stderr, /image-cold requires --fixture-images/);
		assert.equal(mock.requests, 0);
	} finally {
		await mock.close();
	}
});

test('the same seed picks the same pool; another seed does not', async () => {
	const mock = await startMock();
	const dir = tmp();
	try {
		const run = async (seed, name) => {
			const out = join(dir, name);
			const r = await runTool(['--target', mock.url, ...FAST, '--pool', '12', '--sample', '10', '--seed', seed, '--setup-only', '--json', out]);
			assert.equal(r.code, 0, r.stderr + r.stdout);
			return JSON.parse(readFileSync(out, 'utf8'));
		};
		const a = await run('s1', 'a.json');
		const b = await run('s1', 'b.json');
		const c = await run('s2', 'c.json');
		assert.equal(a.manifest.seed, 's1');
		assert.equal(a.pool.workIds.length, 12);
		assert.deepEqual(a.pool.workIds, b.pool.workIds);
		assert.deepEqual(a.pool.hotOrder, b.pool.hotOrder);
		assert.equal(a.pool.digest, b.pool.digest);
		assert.notDeepEqual(a.pool.workIds, c.pool.workIds);
		assert.notEqual(a.pool.digest, c.pool.digest);
		assert.deepEqual(a.steps, []);
	} finally {
		await mock.close();
		rmSync(dir, { recursive: true, force: true });
	}
});

test('a suite writes one JSON: manifest, pool report, steps, metrics, host, omitted', async () => {
	const mock = await startMock();
	const dir = tmp();
	try {
		const settings = join(dir, 'server.env');
		writeFileSync(settings, 'DATABASE_URL=postgres://u:secretpw@db/k\nREQUEST_BUDGET_GRAPHQL=64\n');
		const suite = join(dir, 'suite.json');
		writeFileSync(
			suite,
			JSON.stringify({
				name: 'test-suite',
				defaults: { warmup: 0.2, duration: 0.8, 'metrics-interval': 0.3, 'image-chapters': 3, 'image-urls': 30 },
				steps: [
					{ name: 'metadata', scenario: 'read', vus: [2] },
					{ name: 'writes-seq-hot', scenario: 'write', vus: [2], 'progress-pattern': 'sequential', 'hot-works': 2 },
					{ name: 'warm-images', scenario: 'image-warm', vus: [2] },
					{ name: 'cold-images', scenario: 'image-cold', vus: [2], requires: ['fixture-images'] },
				],
			}),
		);
		const out = join(dir, 'run.json');
		const r = await runTool([
			'--target', mock.url,
			...FAST,
			'--suite', suite,
			'--seed', 'suite-seed',
			'--state', join(dir, 'state.json'),
			'--metrics-url', `${mock.url}/metrics`,
			// Secrets in the command must never reach the JSON; READY opens the window.
			'--host-cmd',
			`PGPASSWORD=hunter3 true sshpass -p s3cret x://u:pw9@h; printf 'READY\\n' >&2; printf '{"label":"%s","seconds":%s,"host":{"busy_cores":0.5}}\\n' {label} {seconds}`,
			'--real-source-sample', '5',
			'--image-digest', 'sha256:feedface',
			'--settings-file', settings,
			'--json', out,
		]);
		assert.equal(r.code, 0, r.stderr + r.stdout);
		const raw = readFileSync(out, 'utf8');
		assert.ok(!raw.includes('secretpw'), 'a secret from the settings file leaked');
		for (const secret of ['hunter3', 's3cret', 'pw9']) assert.ok(!raw.includes(secret), `${secret} from --host-cmd leaked`);
		assert.ok(!r.stdout.includes('hunter3') && !r.stderr.includes('hunter3'));
		const j = JSON.parse(raw);
		assert.equal(j.schema, 'komika-capacity/2');

		// Manifest.
		const m = j.manifest;
		assert.equal(m.target, mock.url);
		assert.equal(m.seed, 'suite-seed');
		assert.equal(m.imageDigest, 'sha256:feedface');
		assert.equal(m.node, process.version);
		assert.ok(m.generator.cpus >= 1 && m.generator.hostname);
		assert.ok('head' in m.git && 'dirtyFiles' in m.git);
		assert.equal(m.suite.name, 'test-suite');
		assert.equal(m.settings.values.REQUEST_BUDGET_GRAPHQL, '64');
		assert.equal(m.options.metricsInterval, 0.3);
		assert.ok(m.startedAt && m.finishedAt);
		assert.equal(m.options.password, undefined);
		assert.match(m.hostCommand, /PGPASSWORD=<redacted>/);
		assert.deepEqual(m.realSourceSample, { cap: 5, max: 200 });

		// Pool report: excluded works with their reason, dataset totals.
		assert.deepEqual(
			j.pool.excluded.map((e) => [e.id, e.reason]).filter(([id]) => id === SLOW_WORK || id === BROKEN_WORK).sort(),
			[[SLOW_WORK, 'slow'], [BROKEN_WORK, 'error']].filter(([id]) => j.pool.workIds.includes(id)).sort(),
		);
		assert.equal(j.pool.dataset.updatesFeedTotal, WORK_COUNT);
		assert.equal(j.pool.dataset.browseTotal, WORK_COUNT);

		// Steps: three ran, the cold one is omitted with its reason.
		assert.equal(j.steps.length, 4);
		assert.deepEqual(j.omitted.map((o) => o.step), ['cold-images']);
		assert.match(j.omitted[0].reason, /fixture-images/);
		const [read, write, warm, cold] = j.steps;
		assert.equal(cold.skipped, j.omitted[0].reason);
		assert.equal(cold.loads.length, 0);

		const p = read.loads[0];
		assert.ok(p.read.requests > 0 && p.errors === 0, JSON.stringify(p.errorsByKind));
		assert.ok(p.read.p95 !== null && p.read.p99 !== null);
		// Metrics: counters moved in the window, gauges kept, derived numbers present.
		assert.ok(p.metrics.good >= 2, JSON.stringify(p.metrics.failures));
		assert.ok(p.metrics.counters['komika_mock_requests_total{lane="graphql"}'] > 0);
		assert.ok(p.metrics.gauges.komika_scan_state_total.max === 5);
		assert.ok(p.metrics.gauges['komika_inflight{lane="graphql"}']);
		assert.ok(p.metrics.derived.refusingLanes.includes('local'));
		assert.ok(!p.metrics.derived.refusingLanes.includes('global'));
		// Host command: one per load step, label filled in.
		assert.equal(p.host.ok, true, JSON.stringify(p.host));
		assert.equal(p.host.json.label, 'metadata-2vu');
		assert.equal(p.host.json.seconds, 1);
		assert.equal(p.host.ready.ready, true);
		assert.match(p.host.command, /sshpass -p <redacted>/);
		// The window: nothing sent after it closed; what finished after it is counted.
		assert.equal(typeof p.afterBoundary.inflightAtBoundary, 'number');
		assert.equal(p.afterBoundary.completedAfterBoundary, p.afterBoundary.inflightAtBoundary);
		// The last scrape starts at the boundary, not after the drain.
		assert.ok(Math.abs(Date.parse(p.metrics.to) - Date.parse(p.endedAt)) < 25, `${p.metrics.to} vs ${p.endedAt}`);
		assert.ok(Date.parse(p.endedAt) - Date.parse(p.startedAt) >= 790);
		assert.equal(p.costInputs.hostBusyCores, 0.5);

		// Writes: the pattern and the hot works are recorded, and only hot works are hit.
		assert.equal(write.progressPattern, 'sequential');
		assert.equal(write.hotWorks.requested, 2);
		assert.equal(write.hotWorks.ids.length, 2);
		const wp = write.loads[0];
		assert.ok(wp.write.requests > 0, JSON.stringify(wp.errorsByKind));
		assert.ok(Object.keys(wp.ops).some((k) => k.startsWith('setProgress(seq')), Object.keys(wp.ops).join(','));
		assert.ok(wp.works.top.every((w) => write.hotWorks.ids.includes(w.id)));
		assert.equal(write.cacheState.earlierSteps[0], 'metadata');

		// Images: harvested, warmed once, then measured with bytes and cache headers.
		assert.ok(j.images.warmPass.ok > 0);
		assert.ok(j.images.pageUrls > 0 && j.images.coverUrls > 0);
		assert.ok(j.images.viaProxyByHost['uploads.mangadex.org'] >= 1);
		// The real-source cap: 5 fetches in the whole run, then none.
		assert.equal(j.realSource.cap, 5);
		assert.equal(j.realSource.used, 5);
		assert.ok(j.realSource.refusedAfterCap > 0 && j.realSource.capHitAt);
		const realFetched = ((mock.imageBytes.engine ?? 0) + (mock.imageBytes.img ?? 0)) / PAGE_BYTES;
		assert.equal(realFetched, 5);
		assert.ok(j.images.warmByClass.store > 0);
		const ip = warm.loads[0];
		assert.ok(ip.image.requests > 0 && ip.image.errors === 0, JSON.stringify(ip.errorsByKind));
		assert.equal(ip.image.bytes, ip.image.requests * PAGE_BYTES);
		assert.ok(ip.image.p95 !== null && ip.image.p99 !== null);
		assert.equal(ip.image.xCache.HIT, ip.image.requests); // warm: every one was fetched before
		assert.equal(ip.image.contentTypes['image/webp'], ip.image.requests);
		assert.equal(ip.image.magic.webp, ip.image.requests);
		assert.ok(ip.metrics.derived.hitRatios.some((h) => h.key === 'komika_image_cache_*_total{cache="pages"}' && h.ratio === 1));
		assert.ok(ip.metrics.derived.bytes.some((b) => b.delta > 0));
	} finally {
		await mock.close();
		rmSync(dir, { recursive: true, force: true });
	}
});

test('mixed, release-burst and image-cold run; failing metrics and host do not stop a run', async () => {
	const mock = await startMock();
	const dir = tmp();
	const dead = http.createServer();
	await new Promise((r) => dead.listen(0, '127.0.0.1', r));
	const deadPort = dead.address().port;
	await new Promise((r) => dead.close(r)); // nothing listens there now
	try {
		const fixtures = join(dir, 'fixtures.txt');
		// 12 addresses plus a duplicate that differs only by its fragment.
		writeFileSync(fixtures, [...Array.from({ length: 12 }, (_, i) => `/fixture/${i}.webp`), '/fixture/0.webp#dup'].join('\n'));
		const suite = join(dir, 'suite.json');
		const fast = { 'page-seconds': 0.05, think: 0.05, 'next-seconds': 0.05, 'chapter-pages': 2, chapters: 1, 'quit-share': 0, 'search-share': 0 };
		writeFileSync(
			suite,
			JSON.stringify({
				defaults: { warmup: 0.1, duration: 0.9, 'image-chapters': 4, 'image-urls': 40 },
				steps: [
					{ name: 'mixed', scenario: 'mixed', users: [2], ...fast },
					{ name: 'burst', scenario: 'release-burst', users: [3], warmup: 0.1, 'page-seconds': 0.05 },
					{ name: 'cold', scenario: 'image-cold', vus: [2], warmup: 0.2 },
				],
			}),
		);
		const out = join(dir, 'run.json');
		const r = await runTool([
			'--target', mock.url,
			...FAST,
			'--suite', suite,
			'--seed', 'x',
			'--state', join(dir, 'state.json'),
			'--fixture-images', fixtures,
			'--metrics-url', `http://127.0.0.1:${deadPort}/metrics`,
			'--host-cmd', 'echo nothing useful; exit 3',
			'--host-ready-timeout', '0.3',
			'--json', out,
		]);
		assert.equal(r.code, 0, r.stderr + r.stdout);
		const j = JSON.parse(readFileSync(out, 'utf8'));
		const [mixed, burst, cold] = j.steps;

		const mp = mixed.loads[0];
		assert.ok(mp.read.requests > 0 && mp.image.requests > 0, JSON.stringify(mp.ops));
		assert.ok(mp.ops.canonicalPages.requests > 0);
		assert.equal(mp.metrics.good, 0);
		assert.ok(mp.metrics.failures.length >= 2);
		assert.equal(mp.host.ok, false);
		assert.equal(mp.host.exitCode, 3);
		assert.equal(mp.host.ready.ready, false);
		// Page images are fixture stand-ins: no engine page and no /img fetch at all.
		assert.equal(mock.imageBytes.engine ?? 0, 0);
		assert.equal(mock.imageBytes.img ?? 0, 0);
		assert.ok(mp.image.byRoute.fixture.requests > 0);

		// Every user opened the same newest chapter.
		assert.equal(burst.burst.length, 1);
		assert.ok(burst.burst[0].chapter.endsWith('-c3'), burst.burst[0].chapter);
		const bp = burst.loads[0];
		assert.equal(bp.ops.seriesPage.requests, 3);
		assert.equal(bp.ops.canonicalPages.requests, 3);
		assert.ok(bp.image.requests >= 3);
		assert.equal(bp.works.distinctWorks, 1);

		// Cold: each fixture address once (after the page stand-ins), then the step stops.
		assert.equal(j.fixtures.urls, 12);
		assert.equal(j.fixtures.duplicatesDropped, 1);
		assert.equal(j.fixtures.standIns, 6);
		assert.equal(j.fixtures.coldUsed, 6);
		assert.equal(j.fixtures.exhausted, true);
		assert.equal(cold.stopped, 'fixture addresses used up');
		const cp = cold.loads[0];
		assert.ok(cp.image.requests <= 6 && cp.image.requests > 0);
		assert.ok(!cp.image.xCache?.HIT, 'a cold address was fetched twice');
	} finally {
		await mock.close();
		rmSync(dir, { recursive: true, force: true });
	}
});
