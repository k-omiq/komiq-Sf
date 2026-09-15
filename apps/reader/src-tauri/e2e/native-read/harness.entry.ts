// Headless integration harness: proves Komika's on-device live-read path end-to-end
// against a REAL booted Suwayomi engine, driving the REAL composite + local backends
// with a shimmed Tauri `invoke` (aliased to ./tauri-core-stub.mjs at bundle time).

// (1) Tauri detection must see the internals marker BEFORE any backend code runs.
(globalThis as any).window = { __TAURI_INTERNALS__: {} };

import { createCompositeBackend, createLocalSuwayomiBackend } from '@komika/api';
import type { Backend } from '@komika/api';

const PORT = process.env.SUWA_PORT || '4572';
const BASE = `http://127.0.0.1:${PORT}`;
const MANGADEX_PKG = 'eu.kanade.tachiyomi.extension.all.mangadex';
// `index.pb`: the older `index.min.json` on the same branch now holds only two
// "update your app" stubs, so the harness would never find MangaDex in it.
const KEIYOUSHI = 'https://github.com/keiyoushi/extensions/raw/repo/index.pb';

// --- ground-truth GraphQL (direct to engine, bypassing the code under test) ---
async function gql<T = any>(query: string, variables?: Record<string, unknown>): Promise<T> {
	const res = await fetch(`${BASE}/api/graphql`, {
		method: 'POST',
		headers: { 'content-type': 'application/json' },
		body: JSON.stringify({ query, variables }),
	});
	const json = await res.json();
	if (json.errors?.length) throw new Error(json.errors.map((e: any) => e.message).join('; '));
	return json.data;
}

interface EngineChapter {
	id: number;
	chapterNumber: number;
	scanlator: string | null;
	name: string;
}
interface Facts {
	uuid: string;
	engineMangaId: number;
	sourceId: string;
	title: string;
	chapters: EngineChapter[];
}

async function bootstrapMangaDex(): Promise<Facts> {
	// Install MangaDex extension (idempotent).
	await gql(
		`mutation{ addExtensionStore(input:{indexUrl:"${KEIYOUSHI}"}){ extensionStore{ indexUrl } } }`,
	).catch(() => {});
	await gql(`mutation{ fetchExtensions(input:{}){ extensions{ pkgName } } }`);
	await gql(
		`mutation{ updateExtension(input:{id:"${MANGADEX_PKG}", patch:{install:true}}){ extension{ pkgName isInstalled } } }`,
	);

	// Resolve the EN MangaDex source id.
	const s = await gql<{
		sources: { nodes: { id: string; lang: string; extension: { pkgName: string } | null }[] };
	}>(`query{ sources{ nodes{ id lang extension{ pkgName } } } }`);
	const mdx = s.sources.nodes.filter((n) => n.extension?.pkgName === MANGADEX_PKG);
	const enSource = mdx.find((n) => n.lang === 'en') ?? mdx[0];
	if (!enSource) throw new Error('no MangaDex EN source after install');
	console.log(
		`[bootstrap] MangaDex EN source id = ${enSource.id} (of ${mdx.length} mangadex sources)`,
	);

	// Browse POPULAR and find a manga that actually has EN chapters.
	for (let page = 1; page <= 3; page++) {
		const browse = await gql<{
			fetchSourceManga: { mangas: { id: number; url: string; title: string }[] };
		}>(
			`mutation($src:LongString!,$p:Int!){ fetchSourceManga(input:{source:$src,type:POPULAR,page:$p}){ mangas{ id url title } } }`,
			{ src: enSource.id, p: page },
		);
		for (const m of browse.fetchSourceManga.mangas) {
			const uuid = m.url.match(/([0-9a-f-]{36})/i)?.[1];
			if (!uuid) continue;
			let chapters: EngineChapter[] = [];
			try {
				const d = await gql<{ fetchMangaAndChapters: { chapters: EngineChapter[] } }>(
					`mutation($id:Int!){ fetchMangaAndChapters(input:{id:$id,fetchManga:false,fetchChapters:true}){ chapters{ id chapterNumber scanlator name } } }`,
					{ id: m.id },
				);
				chapters = (d.fetchMangaAndChapters.chapters ?? []).filter(
					(c) => Number.isFinite(c.chapterNumber) && c.chapterNumber >= 0,
				);
			} catch (e) {
				console.log(
					`[bootstrap]   skip "${m.title}" (chapters fetch failed: ${(e as Error).message})`,
				);
				continue;
			}
			if (chapters.length >= 2) {
				console.log(
					`[bootstrap] picked "${m.title}" uuid=${uuid} engineId=${m.id} chapters=${chapters.length}`,
				);
				return { uuid, engineMangaId: m.id, sourceId: enSource.id, title: m.title, chapters };
			}
			console.log(`[bootstrap]   "${m.title}" has ${chapters.length} usable chapters, trying next`);
		}
	}
	throw new Error('no popular MangaDex manga with EN chapters found in 3 pages');
}

// --- mock hosted backend: only implements the methods the canonical read path touches ---
const HOSTED_SENTINEL = 'HOSTED_SENTINEL';
function makeMockHosted(facts: Facts) {
	// Build canned canonical chapters whose NUMBERS exactly match engine chapters.
	// Use the first 3 distinct engine chapter numbers for stable, cheap assertions.
	const engine = facts.chapters;
	const seenNums = new Set<number>();
	const cannedFromEngine: { number: number; scanlator: string | null }[] = [];
	for (const c of engine) {
		if (cannedFromEngine.length >= 3) break;
		if (seenNums.has(c.chapterNumber)) continue;
		seenNums.add(c.chapterNumber);
		cannedFromEngine.push({ number: c.chapterNumber, scanlator: c.scanlator });
	}

	// Detect a same-number collision to exercise the scanlator tiebreak (assertion E).
	const numCounts = new Map<number, EngineChapter[]>();
	for (const c of engine) {
		const arr = numCounts.get(c.chapterNumber) ?? [];
		arr.push(c);
		numCounts.set(c.chapterNumber, arr);
	}
	let tiebreak: { number: number; scanlator: string; expectedEngineId: number } | null = null;
	for (const [num, arr] of numCounts) {
		const withScan = arr.filter((c) => c.scanlator != null && c.scanlator !== '');
		const distinct = new Set(withScan.map((c) => c.scanlator));
		if (arr.length > 1 && distinct.size > 1) {
			// Pick the SECOND scanlated variant so a correct tiebreak must NOT just take matches[0].
			const target = withScan[1];
			tiebreak = { number: num, scanlator: target.scanlator!, expectedEngineId: target.id };
			break;
		}
	}

	const chapterList: any[] = cannedFromEngine.map((c, i) => ({
		id: `cuuid-${c.number}`,
		seriesId: 'w_test',
		number: c.number,
		title: `Canonical ch ${c.number}`,
		pageCount: 0,
		uploadedAt: '',
		scanlator: c.scanlator,
		read: false,
		lastPageRead: 0,
		bookmarked: false,
		isDownloaded: false,
	}));

	// Extra chapter matching NO engine chapter -> must fall back to hosted (assertion D).
	const FALLBACK_ID = 'cuuid-999.5';
	chapterList.push({
		id: FALLBACK_ID,
		seriesId: 'w_test',
		number: 999.5,
		title: 'Canonical unmatched',
		pageCount: 0,
		uploadedAt: '',
		scanlator: null,
		read: false,
		lastPageRead: 0,
		bookmarked: false,
		isDownloaded: false,
	});

	// If a tiebreak collision exists, add a dedicated canonical chapter carrying the
	// scanlator of the SECOND engine variant (distinct id so it doesn't collide).
	let tiebreakCanonicalId: string | null = null;
	if (tiebreak) {
		tiebreakCanonicalId = `cuuid-tiebreak-${tiebreak.number}`;
		chapterList.push({
			id: tiebreakCanonicalId,
			seriesId: 'w_test',
			number: tiebreak.number,
			title: `Canonical tiebreak ${tiebreak.number}`,
			pageCount: 0,
			uploadedAt: '',
			scanlator: tiebreak.scanlator,
			read: false,
			lastPageRead: 0,
			bookmarked: false,
			isDownloaded: false,
		});
	}

	const notUsed = (m: string) => () => {
		throw new Error(`mockHosted.${m} not used in this path`);
	};

	const hosted: any = {
		workSources: async (_workId: string) => [
			{
				sourceType: 'mangadex',
				sourceId: 'mangadex',
				sourceKey: facts.uuid, // bare uuid
				sourceUrl: null,
				isNsfw: false,
				lang: 'en',
				extension: null,
			},
		],
		canonicalChapters: async (_workId: string) => chapterList.map((c) => ({ ...c })),
		canonicalPages: async (_chapterId: string) => [{ index: 0, sourceUrl: HOSTED_SENTINEL }],
		canonicalSeries: notUsed('canonicalSeries'),
		isReady: async () => true,
		// Unused-in-path stubs (throw if the path unexpectedly reaches them):
		session: async () => null,
		setToken: () => {},
		series: notUsed('series'),
		chapters: notUsed('chapters'),
		pages: notUsed('pages'),
		setProgress: notUsed('setProgress'),
	};

	return { hosted: hosted as Backend, chapterList, FALLBACK_ID, tiebreak, tiebreakCanonicalId };
}

// --- assertion harness ---
let pass = 0,
	fail = 0;
const results: string[] = [];
function assert(name: string, cond: boolean, evidence: string) {
	const tag = cond ? 'PASS' : 'FAIL';
	if (cond) pass++;
	else fail++;
	const line = `[${tag}] ${name} — ${evidence}`;
	results.push(line);
	console.log(line);
}

function magic(buf: Uint8Array): string {
	const hex = [...buf.slice(0, 4)]
		.map((b) => b.toString(16).padStart(2, '0').toUpperCase())
		.join(' ');
	let kind = 'unknown';
	if (buf[0] === 0xff && buf[1] === 0xd8 && buf[2] === 0xff) kind = 'JPEG';
	else if (buf[0] === 0x89 && buf[1] === 0x50 && buf[2] === 0x4e && buf[3] === 0x47) kind = 'PNG';
	else if (buf[0] === 0x52 && buf[1] === 0x49 && buf[2] === 0x46 && buf[3] === 0x46)
		kind = 'RIFF/WEBP';
	return `${kind} (${hex})`;
}

// The exact document the SERVER sends its engine for `chapterOrigin`
// (apps/server/src/suwayomi.rs `chapter_origin`). Keep the two in step.
const SERVER_CHAPTER_ORIGIN_DOC =
	'query CO($id: Int!) { chapters(condition: { id: $id }) { nodes { url mangaId manga { url title sourceId } } } }';

/**
 * F–I: a Suwayomi chapter read by a DEVICE engine that has never seen the manga.
 *
 * The engine booted first plays the server's engine: it holds the chapter under its own
 * row ids. The second engine is empty — no extension, no manga. The only thing that
 * crosses from one to the other is what `chapterOrigin` hands out: the source's own keys.
 */
async function selfFetch(facts: Facts): Promise<void> {
	const serverChapter = facts.chapters[0];

	// ---- F: the server's lookup works against a real engine ----
	const co = await gql<{
		chapters: {
			nodes: {
				url: string;
				mangaId: number;
				manga: { url: string; title: string; sourceId: string };
			}[];
		};
	}>(SERVER_CHAPTER_ORIGIN_DOC, { id: serverChapter.id });
	const node = co.chapters.nodes[0];
	assert(
		'F server-chapter-origin',
		!!node &&
			!!node.url &&
			node.manga.url.includes(facts.uuid) &&
			node.manga.sourceId === facts.sourceId,
		`chapter ${serverChapter.id} -> chapterUrl=${node?.url} mangaUrl=${node?.manga.url} sourceId=${node?.manga.sourceId}`,
	);
	if (!node) return;

	// From here on "the device" is the second, empty engine.
	const devicePort = process.env.SUWA_DEVICE_PORT;
	if (!devicePort) {
		assert('G self-fetch-on-fresh-engine', false, 'SUWA_DEVICE_PORT not set (run via run.sh)');
		return;
	}
	(globalThis as any).__SUWA_DEVICE_PORT = devicePort;
	const deviceGql = async <T = any>(query: string, variables?: Record<string, unknown>) => {
		const res = await fetch(`http://127.0.0.1:${devicePort}/api/graphql`, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify({ query, variables }),
		});
		const json = await res.json();
		if (json.errors?.length) throw new Error(json.errors.map((e: any) => e.message).join('; '));
		return json.data as T;
	};
	const before = await deviceGql<{ mangas: { totalCount: number } }>(
		`query($sid: LongString!, $url: String!){ mangas(condition:{ sourceId:$sid, url:$url }){ totalCount } }`,
		{ sid: node.manga.sourceId, url: node.manga.url },
	);

	let hostedPagesCalls = 0;
	const hosted = {
		chapterOrigin: async (_chapterId: string) => ({
			sourceId: node.manga.sourceId,
			mangaUrl: node.manga.url,
			mangaTitle: node.manga.title,
			chapterUrl: node.url,
			extension: { pkgName: MANGADEX_PKG, repoUrl: KEIYOUSHI },
		}),
		pages: async (_chapterId: string) => {
			hostedPagesCalls++;
			return [{ index: 0, sourceUrl: HOSTED_SENTINEL }];
		},
		setToken: () => {},
	} as unknown as Backend;
	const composite = createCompositeBackend({ hosted, local: createLocalSuwayomiBackend() });

	// ---- G: the device finds the chapter by the source's keys and returns ITS pages ----
	// The first open has to install the extension and ask the source for the manga and its
	// chapter list, so it may run past the composite's budget and answer from the server;
	// the work carries on, and a later open is served on-device.
	const started = Date.now();
	let pages = await composite.pages(String(serverChapter.id));
	const firstWasHosted = pages[0]?.sourceUrl === HOSTED_SENTINEL;
	while (pages[0]?.sourceUrl === HOSTED_SENTINEL && Date.now() - started < 120_000) {
		await new Promise((r) => setTimeout(r, 2000));
		pages = await composite.pages(String(serverChapter.id));
	}
	const p0 = pages[0]?.sourceUrl ?? '';
	const local = p0.startsWith('engine:/api/');
	assert(
		'G self-fetch-on-fresh-engine',
		local && before.mangas.totalCount === 0,
		`device knew the manga before: ${before.mangas.totalCount}; first open from ${firstWasHosted ? 'server (budget)' : 'device'}; ` +
			`after ${Math.round((Date.now() - started) / 1000)}s -> ${pages.length} pages; page[0]=${p0}; hosted pages calls=${hostedPagesCalls}`,
	);
	if (!local) return;

	// ---- H: those pages are real image bytes from the device engine ----
	const { invoke } = await import('@tauri-apps/api/core');
	const ab = (await invoke('suwayomi_image', { path: p0.slice('engine:'.length) })) as ArrayBuffer;
	const bytes = new Uint8Array(ab);
	const okMagic =
		(bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) ||
		(bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47) ||
		(bytes[0] === 0x52 && bytes[1] === 0x49 && bytes[2] === 0x46 && bytes[3] === 0x46);
	assert(
		'H self-fetch-image-bytes',
		bytes.length > 1000 && okMagic,
		`size=${bytes.length}B magic=${magic(bytes)}`,
	);

	// ---- I: a later session finds the chapter in the engine's own database ----
	// A new backend has empty caches, like the app after a restart. The by-key lookup is
	// asserted directly too: inside the backend a rejected query only falls through to
	// the source, which would hide a document the engine does not accept.
	const known = await deviceGql<{ chapters: { nodes: { id: number }[] } }>(
		`query($mid: Int!, $url: String!){ chapters(condition:{ mangaId:$mid, url:$url }){ nodes{ id } } }`,
		{ mid: Number(p0.match(/\/manga\/(\d+)\//)?.[1]), url: node.url },
	);
	const hostedBefore = hostedPagesCalls;
	const t = Date.now();
	const again = await createCompositeBackend({
		hosted,
		local: createLocalSuwayomiBackend(),
	}).pages(String(serverChapter.id));
	assert(
		'I self-fetch-warm-open',
		known.chapters.nodes.length === 1 &&
			(again[0]?.sourceUrl ?? '').startsWith('engine:/api/') &&
			hostedPagesCalls === hostedBefore,
		`by-key lookup -> ${known.chapters.nodes.length} row; new session -> ${again.length} pages in ${Date.now() - t}ms; server fetched pages: ${hostedPagesCalls - hostedBefore}`,
	);
}

async function main() {
	console.log(`[harness] engine base = ${BASE}`);
	const facts = await bootstrapMangaDex();

	const { hosted, chapterList, FALLBACK_ID, tiebreak, tiebreakCanonicalId } = makeMockHosted(facts);
	const composite = createCompositeBackend({ hosted, local: createLocalSuwayomiBackend() });
	const workId = 'w_test';

	// ---- A: canonicalChapters returns the HOSTED canned list unchanged ----
	const chs = await composite.canonicalChapters!(workId);
	const idsMatch =
		chs.length === chapterList.length &&
		chs.every((c, i) => c.id === chapterList[i].id && c.number === chapterList[i].number);
	assert(
		'A identity-preserved',
		idsMatch,
		`hosted uuids/order intact: [${chs.map((c) => c.id).join(', ')}]`,
	);

	// ---- B: reconciliation mapped a matched chapter to a LOCAL engine path ----
	// Pick the first canned chapter that corresponds to a real engine chapter number.
	const matchable = chapterList.find(
		(c) => c.number !== 999.5 && c.id.startsWith('cuuid-') && !c.id.includes('tiebreak'),
	);
	const pages = await composite.canonicalPages!(matchable.id);
	const p0 = pages[0]?.sourceUrl ?? '';
	const isLocal = typeof p0 === 'string' && p0.startsWith('engine:/api/');
	assert(
		'B reconciliation-local-path',
		isLocal,
		`canonical ${matchable.id} (num ${matchable.number}) -> ${pages.length} pages; page[0]=${p0}`,
	);

	// ---- C: the local page path yields real image bytes (JPEG/PNG magic, > 1000 B) ----
	if (isLocal) {
		const { invoke } = await import('@tauri-apps/api/core');
		const ab = (await invoke('suwayomi_image', {
			path: p0.slice('engine:'.length),
		})) as ArrayBuffer;
		const bytes = new Uint8Array(ab);
		const okMagic =
			(bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) ||
			(bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47) ||
			(bytes[0] === 0x52 && bytes[1] === 0x49 && bytes[2] === 0x46 && bytes[3] === 0x46); // WEBP tolerated
		assert(
			'C image-bytes',
			bytes.length > 1000 && okMagic,
			`size=${bytes.length}B magic=${magic(bytes)}`,
		);
	} else {
		assert('C image-bytes', false, 'skipped — B did not produce a local path');
	}

	// ---- D: unmatched canonical chapter falls back to HOSTED sentinel ----
	const dPages = await composite.canonicalPages!(FALLBACK_ID);
	const isSentinel = dPages.length === 1 && dPages[0].sourceUrl === HOSTED_SENTINEL;
	assert(
		'D hosted-fallback',
		isSentinel,
		`canonical ${FALLBACK_ID} (num 999.5) -> page[0]=${dPages[0]?.sourceUrl}`,
	);

	// ---- E (bonus): scanlator tiebreak on a same-number collision ----
	if (tiebreak && tiebreakCanonicalId) {
		const ePages = await composite.canonicalPages!(tiebreakCanonicalId);
		const ep0 = ePages[0]?.sourceUrl ?? '';
		// The engine proxy path embeds the chapter id: /api/v1/manga/<m>/chapter/<engineChapterId>/page/0
		const chapterIdInPath = ep0.match(/\/chapter\/(\d+)\//)?.[1];
		const mapped = chapterIdInPath === String(tiebreak.expectedEngineId);
		assert(
			'E scanlator-tiebreak',
			mapped,
			`num ${tiebreak.number} scanlator="${tiebreak.scanlator}" expected engineChapter=${tiebreak.expectedEngineId}; path=${ep0} (chapterId in path=${chapterIdInPath})`,
		);
	} else {
		results.push(
			'[SKIP] E scanlator-tiebreak — source has no same-number multi-scanlator collision',
		);
		console.log(results[results.length - 1]);
	}

	await selfFetch(facts);

	console.log(`\n[harness] ${pass} passed, ${fail} failed`);
	// Emit a machine-readable summary for the orchestrator.
	console.log(
		'HARNESS_RESULT_JSON=' +
			JSON.stringify({
				pass,
				fail,
				results,
				facts: {
					uuid: facts.uuid,
					title: facts.title,
					engineMangaId: facts.engineMangaId,
					chapterCount: facts.chapters.length,
				},
			}),
	);
	process.exit(fail === 0 ? 0 : 1);
}

main().catch((e) => {
	console.error('[harness] FATAL', e);
	process.exit(2);
});
