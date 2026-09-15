// Headless, deterministic check for the on-device→hosted fallback ladder memo (N2.2).
//
// Pure logic: no Tauri, no engine, no network. Drives the REAL CompositeBackend from
// @komika/api against a MOCK hosted backend and a FAKE local ContentBackend whose
// `refFor`/`chapters` throw (a work that can't be served on-device). Proves the
// session-lifetime `localUnusable` memo:
//   (a) a work whose local resolution throws still returns the HOSTED chapter list,
//   (b) a SECOND open of that work does NOT re-invoke the failing local path
//       (the memo short-circuits — call counters stay flat),
//   (c) a DIFFERENT work is still attempted locally (the memo is per-work), and
//   (d) a not-ready engine is a TRANSIENT rung, never memoed (it retries once ready).
// And the same ladder for `pages` (a Suwayomi chapter read on-device by the source's
// own keys, which the server hands out through `chapterOrigin`): (f)–(k) below.

import { CompositeBackend, LocalSuwayomiBackend } from '@komika/api';
import type { Backend, ContentBackend, ExtensionInstallAsk, SourceRef } from '@komika/api';
import type { Chapter, Id, WorkSource } from '@komika/types';

let failures = 0;
function check(label: string, cond: boolean): void {
	console.log(`${cond ? 'PASS' : 'FAIL'}: ${label}`);
	if (!cond) failures++;
}

/** A minimal hosted Chapter stub — only the fields the reconciliation reads. */
function chapterStub(id: string, number: number): Chapter {
	return {
		id,
		number,
		title: `Chapter ${number}`,
		scanlator: null,
		read: false,
		uploadedAt: null,
	} as unknown as Chapter;
}

/** A minimal MangaDex WorkSource — the only mapping the spine reconciliation uses. */
function workSourceStub(): WorkSource {
	return {
		sourceType: 'mangadex',
		sourceId: 'mangadex',
		sourceKey: '11111111-1111-1111-1111-111111111111',
		lang: 'en',
		extension: null,
	} as unknown as WorkSource;
}

/**
 * A mock hosted backend that serves a fixed canonical chapter list per work and a
 * usable WorkSource. Counts `canonicalChapters` calls so we can
 * confirm hosted is always the authoritative return.
 */
function mockHosted(rec: { canonicalChapters: number }): Backend {
	const hostedChs = [chapterStub('c_1', 1), chapterStub('c_2', 2)];
	return {
		canonicalChapters: async (_workId: Id) => {
			rec.canonicalChapters++;
			// Return fresh clones so identity comparisons reflect content, not aliasing.
			return hostedChs.map((c) => ({ ...c }));
		},
		workSources: async (_workId: Id) => [workSourceStub()],
		canonicalPages: async () => [],
	} as unknown as Backend;
}

/**
 * A fake local ContentBackend that is always ready but whose per-source resolution
 * FAILS: `refFor` throws (extension can't install). Counts `refFor`/`chapters` so a
 * test can assert the memo short-circuits repeat provisioning.
 */
function fakeLocalFailing(rec: {
	isReady: number;
	refFor: number;
	chapters: number;
}): ContentBackend & { refFor(ws: unknown, title?: string): Promise<SourceRef | null> } {
	return {
		isReady: async () => {
			rec.isReady++;
			return true;
		},
		series: async () => {
			throw new Error('unused');
		},
		chapters: async () => {
			rec.chapters++;
			throw new Error('engine chapters unavailable');
		},
		pages: async () => [],
		refFor: async () => {
			rec.refFor++;
			throw new Error('extension install failed (repo down)');
		},
	} as ContentBackend & { refFor(ws: unknown, title?: string): Promise<SourceRef | null> };
}

/** A fake local backend whose readiness is controlled by a mutable flag. */
function fakeLocalGatedReady(
	state: { ready: boolean },
	rec: { refFor: number; chapters: number },
): ContentBackend & { refFor(ws: unknown): Promise<SourceRef | null> } {
	return {
		isReady: async () => state.ready,
		series: async () => {
			throw new Error('unused');
		},
		chapters: async () => {
			rec.chapters++;
			throw new Error('engine chapters unavailable');
		},
		pages: async () => [],
		refFor: async () => {
			rec.refFor++;
			throw new Error('extension install failed (repo down)');
		},
	} as ContentBackend & { refFor(ws: unknown): Promise<SourceRef | null> };
}

async function main(): Promise<void> {
	// ---- (a)+(b) failing local resolution → hosted list, memo skips the retry ----
	{
		const host = { canonicalChapters: 0 };
		const local = { isReady: 0, refFor: 0, chapters: 0 };
		const be = new CompositeBackend({ hosted: mockHosted(host), local: fakeLocalFailing(local) });

		const first = await be.canonicalChapters('w_alpha');
		check(
			'(a) failing local resolution still returns the hosted chapter list',
			first.length === 2 && first[0].id === 'c_1',
		);
		check(
			'(a) hosted canonicalChapters was consulted (authoritative return)',
			host.canonicalChapters === 1,
		);
		check('(a) local refFor was attempted exactly once on first open', local.refFor === 1);

		const refForAfterFirst = local.refFor;
		const readyAfterFirst = local.isReady;
		const second = await be.canonicalChapters('w_alpha');
		check('(b) second open of the same work still returns the hosted list', second.length === 2);
		check(
			'(b) memo short-circuits: local refFor NOT re-invoked on second open',
			local.refFor === refForAfterFirst,
		);
		check(
			'(b) memo short-circuits before localReady(): isReady NOT re-queried',
			local.isReady === readyAfterFirst,
		);
		check('(b) hosted still served the second open', host.canonicalChapters === 2);
	}

	// ---- (c) a DIFFERENT work is still attempted locally (memo is per-work) ----
	{
		const host = { canonicalChapters: 0 };
		const local = { isReady: 0, refFor: 0, chapters: 0 };
		const be = new CompositeBackend({ hosted: mockHosted(host), local: fakeLocalFailing(local) });

		await be.canonicalChapters('w_alpha'); // memoes w_alpha (refFor throws)
		check('(c) precondition: w_alpha attempted once', local.refFor === 1);

		await be.canonicalChapters('w_beta'); // different work — must still try locally
		check('(c) a different work IS attempted locally (per-work memo)', local.refFor === 2);

		// w_alpha stays memoed even after w_beta ran.
		await be.canonicalChapters('w_alpha');
		check('(c) w_alpha remains memoed after w_beta ran (refFor still 2)', local.refFor === 2);
	}

	// ---- (d) not-ready engine is TRANSIENT, never memoed → retries once ready ----
	{
		const host = { canonicalChapters: 0 };
		const state = { ready: false };
		const rec = { refFor: 0, chapters: 0 };
		const be = new CompositeBackend({
			hosted: mockHosted(host),
			local: fakeLocalGatedReady(state, rec),
		});

		await be.canonicalChapters('w_gamma'); // engine down → local skipped, NOT memoed
		check('(d) not-ready engine skips local without attempting refFor', rec.refFor === 0);

		state.ready = true; // engine comes up
		await be.canonicalChapters('w_gamma'); // must retry local now (not poisoned by not-ready)
		check(
			'(d) once ready, the same work IS retried locally (not-ready not memoed)',
			rec.refFor === 1,
		);
	}

	// ---- (e) no local backend at all → pure hosted passthrough (web/flag-off safety) ----
	{
		const host = { canonicalChapters: 0 };
		const be = new CompositeBackend({ hosted: mockHosted(host), local: null });
		const chs = await be.canonicalChapters('w_delta');
		check(
			'(e) no local backend → hosted list returned unchanged',
			chs.length === 2 && host.canonicalChapters === 1,
		);
	}

	// ---- pages(): a Suwayomi chapter fetched on-device by the source's own keys ----
	const origin = (mangaUrl: string, chapterUrl: string) => ({
		sourceId: '42',
		mangaUrl,
		mangaTitle: 'Some Manga',
		chapterUrl,
		extension: { pkgName: 'org.example.ext', repoUrl: 'https://example/repo.json' },
	});
	const HOSTED_PAGE = [
		{ index: 0, sourceUrl: 'https://server.example/api/v1/manga/9/chapter/1/page/0' },
	];
	type PagesRec = { hostedPages: number; origin: number; resolve: number; localPages: number };
	const newRec = (): PagesRec => ({ hostedPages: 0, origin: 0, resolve: 0, localPages: 0 });
	/** Hosted: chapter ids `1xx` belong to manga A, `2xx` to manga B. */
	const pagesHosted = (rec: PagesRec, originImpl?: (id: Id) => unknown): Backend =>
		({
			pages: async () => {
				rec.hostedPages++;
				return HOSTED_PAGE;
			},
			chapterOrigin: async (id: Id) => {
				rec.origin++;
				if (originImpl) return originImpl(id);
				return origin(String(id).startsWith('1') ? '/manga/a' : '/manga/b', `/chapter/${id}`);
			},
		}) as unknown as Backend;
	const pagesLocal = (
		rec: PagesRec,
		resolve: (ref: SourceRef, chapterUrl: string) => Promise<Id | null>,
		ready: () => boolean = () => true,
	): ContentBackend =>
		({
			isReady: async () => ready(),
			resolveChapter: async (ref: SourceRef, chapterUrl: string) => {
				rec.resolve++;
				return resolve(ref, chapterUrl);
			},
			pages: async (id: Id) => {
				rec.localPages++;
				return [{ index: 0, sourceUrl: `engine:/api/v1/manga/3/chapter/${id}/page/0` }];
			},
		}) as unknown as ContentBackend;

	// ---- (f) the engine finds the chapter → its pages, and the server fetches nothing ----
	{
		const rec = newRec();
		let seen: { ref: SourceRef; chapterUrl: string } | null = null;
		const be = new CompositeBackend({
			hosted: pagesHosted(rec),
			local: pagesLocal(rec, async (ref, chapterUrl) => {
				seen = { ref, chapterUrl };
				return '77';
			}),
		});
		const pages = await be.pages('101');
		check(
			'(f) pages come from the engine',
			pages.length === 1 && pages[0].sourceUrl.startsWith('engine:/api/v1/'),
		);
		check('(f) the server was NOT asked to fetch pages', rec.hostedPages === 0);
		const s = seen as { ref: SourceRef; chapterUrl: string } | null;
		check(
			"(f) the engine was given the source's own keys and the extension",
			s?.ref.sourceId === '42' &&
				s.ref.sourceKey === '/manga/a' &&
				s.ref.pkgName === 'org.example.ext' &&
				s.ref.title === 'Some Manga' &&
				s.chapterUrl === '/chapter/101',
		);
	}

	// ---- (g) a server that predates chapterOrigin → hosted pages, engine untouched ----
	{
		const rec = newRec();
		const be = new CompositeBackend({
			hosted: pagesHosted(rec, () => undefined),
			local: pagesLocal(rec, async () => '77'),
		});
		const pages = await be.pages('101');
		check('(g) old server → hosted pages', pages === HOSTED_PAGE && rec.resolve === 0);
		const none = new CompositeBackend({
			hosted: { pages: async () => HOSTED_PAGE } as unknown as Backend,
			local: pagesLocal(rec, async () => '77'),
		});
		check(
			'(g) backend without chapterOrigin → hosted pages',
			(await none.pages('101')) === HOSTED_PAGE,
		);
	}

	// ---- (h) the manga cannot be read on-device → hosted, and the manga is memoed ----
	{
		const rec = newRec();
		const be = new CompositeBackend({
			hosted: pagesHosted(rec),
			local: pagesLocal(rec, async () => {
				throw new Error('extension install failed');
			}),
		});
		check(
			'(h) a failing engine still returns the hosted pages',
			(await be.pages('101')) === HOSTED_PAGE,
		);
		await be.pages('102'); // another chapter of manga A
		check('(h) the same manga is not retried on-device', rec.resolve === 1);
		await be.pages('201'); // manga B
		check('(h) a different manga IS tried on-device', rec.resolve === 2);
	}

	// ---- (i) one chapter the source does not list → only that chapter is memoed ----
	{
		const rec = newRec();
		const be = new CompositeBackend({
			hosted: pagesHosted(rec),
			local: pagesLocal(rec, async (_ref, chapterUrl) =>
				chapterUrl === '/chapter/101' ? null : '77',
			),
		});
		check(
			'(i) an unlisted chapter returns the hosted pages',
			(await be.pages('101')) === HOSTED_PAGE,
		);
		await be.pages('101');
		check('(i) the unlisted chapter is not looked up again', rec.resolve === 1 && rec.origin === 1);
		const other = await be.pages('102');
		check(
			'(i) another chapter of the same manga is still read on-device',
			other[0].sourceUrl.startsWith('engine:') && rec.resolve === 2,
		);
	}

	// ---- (j) engine not ready → hosted, nothing memoed, retried once ready ----
	{
		const rec = newRec();
		const state = { ready: false };
		const be = new CompositeBackend({
			hosted: pagesHosted(rec),
			local: pagesLocal(
				rec,
				async () => '77',
				() => state.ready,
			),
		});
		check(
			'(j) not-ready engine → hosted pages, no server lookup spent',
			(await be.pages('101')) === HOSTED_PAGE && rec.origin === 0,
		);
		state.ready = true;
		const pages = await be.pages('101');
		check(
			'(j) once ready, the same chapter is read on-device',
			pages[0].sourceUrl.startsWith('engine:'),
		);
	}

	// ---- (k) the server lookup itself fails → hosted pages, nothing memoed ----
	{
		const rec = newRec();
		let fail = true;
		const be = new CompositeBackend({
			hosted: pagesHosted(rec, (id) => {
				if (fail) throw new Error('server unreachable');
				return origin('/manga/a', `/chapter/${id}`);
			}),
			local: pagesLocal(rec, async () => '77'),
		});
		check(
			'(k) a failed lookup still returns the hosted pages',
			(await be.pages('101')) === HOSTED_PAGE,
		);
		fail = false;
		const pages = await be.pages('101');
		check('(k) the lookup failure was not memoed', pages[0].sourceUrl.startsWith('engine:'));
	}

	// ---- the REAL LocalSuwayomiBackend asks before it installs what a server names ----
	{
		const MANGADEX_PKG = 'eu.kanade.tachiyomi.extension.all.mangadex';
		const KEIYOUSHI = 'https://github.com/keiyoushi/extensions/raw/repo/index.pb';
		/** An engine with nothing installed; records the GraphQL operations it is sent. */
		const engine = (confirmInstall?: (ask: ExtensionInstallAsk) => Promise<boolean>) => {
			const ops: string[] = [];
			const local = new LocalSuwayomiBackend({ confirmInstall }) as unknown as {
				gql: (query: string) => Promise<unknown>;
				ensureProvisionedPkg: (pkg: string, repoUrl?: string) => Promise<void>;
			};
			local.gql = async (query) => {
				const op = query.match(/(?:query|mutation)\s+(\w+)/)?.[1] ?? '';
				ops.push(op);
				return op === 'ExtensionState' ? { extension: { isInstalled: false } } : {};
			};
			return { local, ops };
		};
		const changed = (ops: string[]) =>
			ops.some((op) => op === 'AddExtensionStore' || op === 'InstallExtension');
		const failed = (run: Promise<void>) =>
			run.then(
				() => false,
				() => true,
			);

		const asked: ExtensionInstallAsk[] = [];
		const no = engine(async (ask) => (asked.push(ask), false));
		check(
			'(l) a refused install throws and changes nothing in the engine',
			(await failed(no.local.ensureProvisionedPkg('org.example.ext', 'https://example/repo'))) &&
				!changed(no.ops) &&
				asked.length === 1 &&
				asked[0].pkgName === 'org.example.ext' &&
				asked[0].repoUrl === 'https://example/repo',
		);
		const silent = engine();
		check(
			'(l) no way to ask is a refusal, also for a package without a store',
			(await failed(silent.local.ensureProvisionedPkg('org.example.ext'))) && !changed(silent.ops),
		);
		const yes = engine(async () => true);
		await yes.local.ensureProvisionedPkg('org.example.ext', 'https://example/repo');
		check(
			'(m) an allowed install adds the store and installs the package',
			yes.ops.includes('AddExtensionStore') && yes.ops.includes('InstallExtension'),
		);
		const builtIn = engine(async () => {
			throw new Error('the built-in MangaDex install must not ask');
		});
		await builtIn.local.ensureProvisionedPkg(MANGADEX_PKG, KEIYOUSHI);
		check(
			'(n) the built-in MangaDex install does not ask',
			builtIn.ops.includes('InstallExtension'),
		);
		const otherStore = engine();
		check(
			'(n) the same package from another store does ask',
			(await failed(otherStore.local.ensureProvisionedPkg(MANGADEX_PKG, 'https://example/repo'))) &&
				!changed(otherStore.ops),
		);
	}

	console.log('');
	if (failures === 0) {
		console.log('ALL CHECKS PASSED');
	} else {
		console.log(`${failures} CHECK(S) FAILED`);
		process.exitCode = 1;
	}
}

main().catch((err) => {
	console.error('fallback-ladder.check crashed:', err);
	process.exitCode = 1;
});
