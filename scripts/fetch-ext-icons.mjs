#!/usr/bin/env node
/**
 * Refresh the vendored extension icons in `assets/ext-icons/`.
 *
 * WHY THESE ARE VENDORED
 * Keiyoushi emptied `extensions/repo/icon/` when they migrated their index from
 * `index.min.json` to `index.pb`, so the URL Komika used to build for every
 * extension logo (`repo/icon/{pkgName}.png`) started 404ing. The icons each
 * `index.pb` record points at live in the SOURCE repo instead, keyed by the
 * package's trailing `{lang}.{dir}` segments. Rather than hotlink a third-party
 * layout that has now broken once, we snapshot the set and serve it ourselves
 * from `GET /ext-icons/{pkgName}.png` (see `serve_ext_icon` in the server).
 *
 * TWO SOURCES, MERGED
 * Neither source alone is complete, so this script tries both per package:
 *   1. Keiyoushi's source repo via jsDelivr — covers every extension whose
 *      `index.pb` record publishes an icon (~1327 at time of writing).
 *   2. The local Suwayomi engine's `/api/v1/extension/icon/{pkg}` — this
 *      unpacks the icon from the INSTALLED APK, so it only answers for
 *      installed extensions, but it covers a few the source repo omits.
 * The remainder (~41) ship no icon in either place; the UI renders its
 * initial-letter placeholder for those, which is the correct outcome.
 *
 * The package list comes from our own engine rather than from `index.pb`: it is
 * an official API (no protobuf parsing), and it scopes the snapshot to exactly
 * the extensions this deployment actually lists.
 *
 * USAGE
 *   node scripts/fetch-ext-icons.mjs            # refresh in place
 *   SUWA_URL=http://host:4567 node scripts/...  # non-default engine
 *   node scripts/fetch-ext-icons.mjs --prune    # also delete now-orphaned icons
 *
 * Re-running is safe and idempotent: bytes are only written when they differ, so
 * an unchanged refresh produces an empty git diff.
 */
import { mkdir, readdir, readFile, writeFile, unlink } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const SUWA_URL = (process.env.SUWA_URL || 'http://127.0.0.1:4567').replace(/\/$/, '');
const OUT_DIR = join(dirname(dirname(fileURLToPath(import.meta.url))), 'assets', 'ext-icons');
const CONCURRENCY = 16;
const PRUNE = process.argv.includes('--prune');

/** Keiyoushi's source-repo icon path, derived from the package name.
 *  `eu.kanade.tachiyomi.extension.en.foo` → `src/en/foo/…/ic_launcher.png`.
 *  Verified against the live `index.pb`: this reproduces every published icon
 *  URL exactly. Mirrors `keiyoushi_icon_url` in the server's `graphql/mod.rs`. */
function storeIconUrl(pkg) {
	const m = pkg.match(/^eu\.kanade\.tachiyomi\.extension\.([^.]+)\.([^.]+)$/);
	if (!m) return null;
	return `https://cdn.jsdelivr.net/gh/keiyoushi/extensions-source@main/src/${m[1]}/${m[2]}/res/mipmap-xhdpi/ic_launcher.png`;
}

async function fetchBytes(url) {
	try {
		const res = await fetch(url, { signal: AbortSignal.timeout(30_000) });
		if (!res.ok) return null;
		const buf = Buffer.from(await res.arrayBuffer());
		// Guard against an error page served with a 200: every icon is a PNG.
		return buf.length && buf.subarray(0, 8).equals(Buffer.from('89504e470d0a1a0a', 'hex'))
			? buf
			: null;
	} catch {
		return null;
	}
}

async function listExtensions() {
	const res = await fetch(`${SUWA_URL}/api/graphql`, {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ query: '{ extensions { nodes { pkgName } } }' }),
	});
	if (!res.ok) throw new Error(`engine returned ${res.status} — is Suwayomi up at ${SUWA_URL}?`);
	const body = await res.json();
	const nodes = body?.data?.extensions?.nodes;
	if (!Array.isArray(nodes) || nodes.length === 0) {
		throw new Error('engine listed no extensions — refusing to rewrite the icon set');
	}
	return nodes.map((n) => n.pkgName);
}

/** Write only when the bytes actually changed, so a no-op refresh stays out of git. */
async function writeIfChanged(path, bytes) {
	const prev = await readFile(path).catch(() => null);
	if (prev && prev.equals(bytes)) return 'unchanged';
	await writeFile(path, bytes);
	return prev ? 'updated' : 'added';
}

async function fetchOne(pkg) {
	const store = storeIconUrl(pkg);
	let bytes = store ? await fetchBytes(store) : null;
	let source = bytes ? 'store' : null;
	if (!bytes) {
		bytes = await fetchBytes(`${SUWA_URL}/api/v1/extension/icon/${pkg}`);
		source = bytes ? 'engine' : null;
	}
	if (!bytes) return { pkg, source: null, state: 'missing' };
	const state = await writeIfChanged(join(OUT_DIR, `${pkg}.png`), bytes);
	return { pkg, source, state };
}

/** Run `task` over `items` with a bounded number of in-flight requests. */
async function mapLimit(items, limit, task) {
	const out = [];
	let next = 0;
	await Promise.all(
		Array.from({ length: Math.min(limit, items.length) }, async () => {
			while (next < items.length) {
				const i = next++;
				out[i] = await task(items[i]);
			}
		}),
	);
	return out;
}

const pkgs = await listExtensions();
await mkdir(OUT_DIR, { recursive: true });
console.log(`${pkgs.length} extensions listed by the engine; fetching icons…`);

const results = await mapLimit(pkgs, CONCURRENCY, fetchOne);
const tally = (key, val) => results.filter((r) => r[key] === val).length;

if (PRUNE) {
	const keep = new Set(results.filter((r) => r.source).map((r) => `${r.pkg}.png`));
	const onDisk = (await readdir(OUT_DIR)).filter((f) => f.endsWith('.png'));
	for (const f of onDisk.filter((f) => !keep.has(f))) {
		await unlink(join(OUT_DIR, f));
		console.log(`pruned ${f}`);
	}
}

console.log(
	`store=${tally('source', 'store')} engine=${tally('source', 'engine')} ` +
		`no-icon=${tally('source', null)} | ` +
		`added=${tally('state', 'added')} updated=${tally('state', 'updated')} ` +
		`unchanged=${tally('state', 'unchanged')}`,
);
const missing = results.filter((r) => !r.source).map((r) => r.pkg);
if (missing.length) {
	console.log(`\nNo icon published by either source (placeholder is expected):`);
	for (const p of missing) console.log(`  ${p}`);
}
