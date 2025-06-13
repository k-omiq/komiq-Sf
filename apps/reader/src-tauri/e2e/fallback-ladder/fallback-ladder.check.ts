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

import { CompositeBackend } from '@komika/api';
import type { Backend, ContentBackend, SourceRef } from '@komika/api';
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

/** A minimal WorkSource carrying extension provisioning coords (a suwayomi source). */
function workSourceStub(): WorkSource {
	return {
		sourceType: 'suwayomi',
		sourceId: '42',
		sourceKey: 'manga/1',
		lang: 'en',
		extension: { pkgName: 'org.example.ext', repoUrl: 'https://example/repo.json' },
	} as unknown as WorkSource;
}

/**
 * A mock hosted backend that serves a fixed canonical chapter list per work and a
 * usable (extension-carrying) WorkSource. Counts `canonicalChapters` calls so we can
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
		check('(a) failing local resolution still returns the hosted chapter list', first.length === 2 && first[0].id === 'c_1');
		check('(a) hosted canonicalChapters was consulted (authoritative return)', host.canonicalChapters === 1);
		check('(a) local refFor was attempted exactly once on first open', local.refFor === 1);

		const refForAfterFirst = local.refFor;
		const readyAfterFirst = local.isReady;
		const second = await be.canonicalChapters('w_alpha');
		check('(b) second open of the same work still returns the hosted list', second.length === 2);
		check('(b) memo short-circuits: local refFor NOT re-invoked on second open', local.refFor === refForAfterFirst);
		check('(b) memo short-circuits before localReady(): isReady NOT re-queried', local.isReady === readyAfterFirst);
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
		check('(d) once ready, the same work IS retried locally (not-ready not memoed)', rec.refFor === 1);
	}

	// ---- (e) no local backend at all → pure hosted passthrough (web/flag-off safety) ----
	{
		const host = { canonicalChapters: 0 };
		const be = new CompositeBackend({ hosted: mockHosted(host), local: null });
		const chs = await be.canonicalChapters('w_delta');
		check('(e) no local backend → hosted list returned unchanged', chs.length === 2 && host.canonicalChapters === 1);
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
