import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';
import type { Backend } from '../../../../../packages/api/src/backend.ts';

registerHooks({
	load(url, context, next) {
		if (/\/packages\/api\/src\/(composite-backend|offline-queue|graphql-backend)\.ts$/.test(url)) {
			return {
				format: 'module',
				source: ts.transpileModule(readFileSync(fileURLToPath(url), 'utf8'), {
					compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
				}).outputText,
				shortCircuit: true,
			};
		}
		return next(url, context);
	},
});

const { CompositeBackend } = await import('../../../../../packages/api/src/composite-backend.ts');
const { OfflineWriteQueue, createMemoryStorageAdapter } =
	await import('../../../../../packages/api/src/offline-queue.ts');
const { GraphQLBackend } = await import('../../../../../packages/api/src/graphql-backend.ts');
const { GraphQLRequestError } = await import('../../../../../packages/api/src/partial-errors.ts');

type Write = { chapterId: string; epoch: number | null | undefined };

/** A server whose engine generation can change: numeric writes from another epoch are
 *  refused the way the real one refuses them (`STALE_NUMERIC_EPOCH`). Its `numericEpoch`
 *  is kept after the first read, like `GraphQLBackend.numericEpoch`. */
function fakeServer() {
	const server = {
		epoch: 3,
		online: false,
		accepted: [] as Write[],
		sent: [] as Write[],
		epochReads: 0,
		known: undefined as number | undefined,
	};
	const hosted = {
		setToken: () => {},
		numericEpoch: async () => {
			if (server.known !== undefined) return server.known;
			if (!server.online) throw new Error('offline');
			server.epochReads++;
			return (server.known = server.epoch);
		},
		setProgress: async (chapterId: string, _page: number, _read: boolean, epoch?: number | null) => {
			if (/^[0-9]+$/.test(chapterId) && epoch === undefined) epoch = server.known ?? null;
			server.sent.push({ chapterId, epoch });
			if (!server.online) throw new Error('offline');
			if (/^[0-9]+$/.test(chapterId) && epoch !== server.epoch) {
				throw new GraphQLRequestError([
					{
						message: 'Numeric reading progress belongs to an earlier engine generation',
						extensions: { code: 'STALE_NUMERIC_EPOCH' },
					},
				]);
			}
			server.accepted.push({ chapterId, epoch });
		},
	} as unknown as Backend;
	return { server, hosted };
}

function captureWarnings(t: { after: (fn: () => void) => void }) {
	const lines: string[] = [];
	const original = console.warn;
	console.warn = (...args: unknown[]) => void lines.push(args.map(String).join(' '));
	t.after(() => {
		console.warn = original;
	});
	return lines;
}

function signedIn(hosted: Backend) {
	const writes = new OfflineWriteQueue(createMemoryStorageAdapter());
	const composite = new CompositeBackend({ hosted, queue: writes });
	composite.setToken('token-A');
	composite.setCurrentUser('A');
	return { writes, composite };
}

test('a write queued before an engine change is dropped on replay, never filed on the new chapter', async (t) => {
	const warnings = captureWarnings(t);
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);

	// Read the epoch while online, before queueing an offline write.
	server.online = true;
	await composite.setProgress('41', 2, false);
	assert.deepEqual(server.accepted, [{ chapterId: '41', epoch: 3 }]);

	// Offline: the write is queued with the epoch known at queue time.
	server.online = false;
	await composite.setProgress('42', 5, true);
	assert.deepEqual(
		writes.peekAll().map((op) => op.kind === 'progress' && [op.chapterId, op.numericEpoch]),
		[['42', 3]],
	);

	// The operator starts a new engine generation; chapter 42 may now be another chapter.
	server.epoch = 4;
	server.online = true;
	await composite.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	// Let the drain the success started finish.
	await new Promise((resolve) => setTimeout(resolve, 0));

	assert.equal(writes.size(), 0, 'the stale write left the queue');
	assert.ok(
		!server.accepted.some((w) => w.chapterId === '42'),
		'the stale write was never accepted',
	);
	assert.deepEqual(
		server.sent.filter((w) => w.chapterId === '42').map((w) => w.epoch),
		[3, 3],
		'replayed once with its own epoch, never re-read and never retried',
	);
	assert.equal(server.epochReads, 1, 'the replay did not read the new epoch');
	assert.ok(warnings.some((line) => line.includes('earlier engine generation') && line.includes('42')));
});

test('a live write refused as stale is dropped, not queued', async (t) => {
	const warnings = captureWarnings(t);
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);
	server.online = true;
	await composite.setProgress('7', 1, false);
	server.epoch = 4; // changed while the app kept its old ids and epoch
	await composite.setProgress('7', 2, false);
	assert.equal(writes.size(), 0);
	assert.deepEqual(server.accepted, [{ chapterId: '7', epoch: 3 }]);
	assert.equal(warnings.length, 1);
});

test('a write queued with no known epoch replays with none, never with the current one', async () => {
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);
	// Offline from the start: the epoch was never read.
	await composite.setProgress('42', 5, true);
	const [op] = writes.peekAll();
	assert.equal(op.kind === 'progress' && op.numericEpoch, null);
	server.online = true;
	await composite.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	await new Promise((resolve) => setTimeout(resolve, 0));
	assert.deepEqual(
		server.sent.filter((w) => w.chapterId === '42').map((w) => w.epoch),
		[null, null],
		'sent without an epoch, so the server decides as it does for an old app',
	);
	assert.equal(writes.size(), 0, 'the server refused it as stale, so it was dropped');
});

test('an ordinary failure still stays queued for retry', async () => {
	const { server, hosted } = fakeServer();
	const { writes, composite } = signedIn(hosted);
	server.online = true;
	await composite.setProgress('41', 1, false);
	server.online = false;
	await composite.setProgress('42', 5, true);
	await composite.setProgress('43', 1, false);
	assert.deepEqual(
		writes.peekAll().map((op) => op.kind === 'progress' && [op.chapterId, op.numericEpoch, op.tries]),
		[
			['42', 3, 0],
			['43', 3, 0],
		],
	);
});

test('an op persisted before epochs existed replays with no epoch', async () => {
	const { server, hosted } = fakeServer();
	const storage = createMemoryStorageAdapter();
	storage.save(
		JSON.stringify([
			{ kind: 'progress', chapterId: '42', lastPageRead: 5, read: true, ts: 1, tries: 0, userId: 'A' },
		]),
	);
	const writes = new OfflineWriteQueue(storage);
	const composite = new CompositeBackend({ hosted, queue: writes });
	server.online = true;
	composite.setToken('token-A');
	composite.setCurrentUser('A');
	await composite.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	await new Promise((resolve) => setTimeout(resolve, 0));
	assert.deepEqual(
		server.sent.filter((w) => w.chapterId === '42').map((w) => w.epoch),
		[null],
	);
});

// --- GraphQLBackend: how the epoch reaches the wire ---

let endpointNumber = 0;
const endpoint = () => 'https://epoch-' + ++endpointNumber + '.test/graphql';

function recordingBackend(answer: (body: { query: string; variables?: Record<string, unknown> }) => unknown) {
	const bodies: { query: string; variables?: Record<string, unknown> }[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		fetch: async (_url, init) => {
			const body = JSON.parse(String(init?.body));
			bodies.push(body);
			return Response.json(answer(body));
		},
	});
	return { backend, bodies };
}

const isEpochRead = (b: { query: string }) => /query NumericEpoch/.test(b.query);

test('numeric writes carry one epoch read; uuid writes read and send none', async () => {
	const { backend, bodies } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 9 } } : { data: { setProgress: true, setChapterRead: true } },
	);
	await backend.setProgress('12', 3, false);
	await backend.setProgress('13', 1, true);
	await backend.setChapterRead('14', true);
	await backend.setProgress('a1b2c3d4-uuid-chapter', 1, false);
	assert.equal(bodies.filter(isEpochRead).length, 1, 'read once, then kept');
	const writes = bodies.filter((b) => !isEpochRead(b));
	assert.deepEqual(
		writes.map((b) => b.variables?.numericEpoch),
		[9, 9, 9, undefined],
	);
});

test('a replayed write sends exactly the epoch it was queued with, or none for null', async () => {
	const { backend, bodies } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 9 } } : { data: { setProgress: true } },
	);
	await backend.setProgress('12', 3, false, 4);
	await backend.setProgress('12', 3, false, null);
	assert.equal(bodies.filter(isEpochRead).length, 0, 'a replay never reads the current epoch');
	assert.deepEqual(
		bodies.map((b) => b.variables?.numericEpoch),
		[4, undefined],
	);
});

test('an old server: the epoch field is unknown and the argument is stripped', async () => {
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) return { errors: [{ message: 'Unknown field "numericEpoch" on type "QueryRoot".' }] };
		if (/numericEpoch/.test(b.query))
			return {
				errors: [{ message: 'Unknown argument "numericEpoch" on field "setProgress" of type "MutationRoot".' }],
			};
		return { data: { setProgress: true } };
	});
	await backend.setProgress('12', 3, false); // epoch unknown here: null, sent as none
	await backend.setProgress('12', 4, false, 2); // a replay with an epoch, on an old server
	const writes = bodies.filter((b) => !isEpochRead(b));
	assert.equal(bodies.filter(isEpochRead).length, 1, 'an old server is asked once');
	assert.equal(writes.at(-1)!.query.includes('numericEpoch'), false, 'stripped for an old server');
	assert.equal(writes.length, 3, 'one strip-and-retry, then the stripped form');
});

test('a stale refusal surfaces with its code', async () => {
	const { isStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
	const { backend } = recordingBackend((b) =>
		isEpochRead(b)
			? { data: { numericEpoch: 1 } }
			: {
					errors: [
						{
							message: 'Numeric reading progress belongs to an earlier engine generation',
							extensions: { code: 'STALE_NUMERIC_EPOCH' },
						},
					],
				},
	);
	await assert.rejects(backend.setProgress('12', 3, false), (e: unknown) => isStaleNumericEpoch(e));
	assert.equal(isStaleNumericEpoch(new Error('offline')), false);
});

const staleResponse = () => ({
	errors: [{ message: 'Earlier engine generation', extensions: { code: 'STALE_NUMERIC_EPOCH' } }],
});

test('M1 content reads pin the epoch before returning numeric ids', async (t) => {
	const reads: [string, (backend: InstanceType<typeof GraphQLBackend>) => Promise<unknown>][] = [
		['discovery', (b) => b.discovery()],
		['updates', (b) => b.updates()],
		['search', (b) => b.search('manga')],
		['searchAllSources', (b) => b.searchAllSources('manga')],
		['series', (b) => b.series('7')],
		['chapters', (b) => b.chapters('7')],
		['aggregatedChapters', (b) => b.aggregatedChapters('w_fixture')],
		['pages', (b) => b.pages('42')],
		['library', (b) => b.library()],
		['libraryProgress', (b) => b.libraryProgress()],
		['canonicalUpdates', (b) => b.canonicalUpdates()],
		['updatesFeed', (b) => b.updatesFeed()],
		['canonicalSeries', (b) => b.canonicalSeries('w_fixture')],
		['canonicalChapters', (b) => b.canonicalChapters('w_fixture')],
		['canonicalPages', (b) => b.canonicalPages('42')],
		['seriesPage', (b) => b.seriesPage('w_fixture', true)],
		['sourceBrowse', (b) => b.sourceBrowse('source', 'POPULAR')],
		['seriesSourcesBatch', (b) => b.seriesSourcesBatch(['7'])],
	];
	for (const [field, read] of reads) {
		await t.test(field, async () => {
			let epoch = 3;
			const { backend, bodies } = recordingBackend((b) => {
				if (isEpochRead(b)) return { data: { numericEpoch: epoch } };
				if (/mutation SetProgress/.test(b.query))
					return b.variables?.numericEpoch === epoch
						? { data: { setProgress: true } }
						: staleResponse();
				return { data: { [field]: [] } };
			});
			await read(backend);
			assert.ok(isEpochRead(bodies[0]), 'the epoch is read before the content');
			assert.ok(bodies.slice(1).every((b) => !b.query.includes('numericEpoch')));
			epoch = 4;
			await assert.rejects(backend.setProgress('42', 1, false), /Earlier engine generation/);
			assert.equal(bodies.at(-1)!.variables?.numericEpoch, 3);
			assert.equal(bodies.filter(isEpochRead).length, 1);
		});
	}
});

test('M1 a failed epoch read leaves content available and pins no epoch for writes', async (t) => {
	captureWarnings(t);
	let reads = 0;
	let epoch = 3;
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) {
			if (++reads === 1) throw new Error('offline');
			return { data: { numericEpoch: epoch } };
		}
		return { data: { chapters: [{ id: '42' }], setProgress: true, setChapterRead: true, mark: { id: '7' } } };
	});
	assert.deepEqual(await backend.chapters('7'), [{ id: '42' }]);
	epoch = 4;
	await backend.setProgress('42', 1, false);
	await backend.setChapterRead('42', true);
	await backend.mark('7', true);
	assert.equal(reads, 1);
	assert.equal(await backend.numericEpoch(), null);
	assert.ok(bodies.slice(2).every((b) => b.variables?.numericEpoch === undefined));
});

test('M1 a failed epoch read on the write path alone is retried', async () => {
	let reads = 0;
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) {
			if (++reads === 1) throw new Error('offline');
			return { data: { numericEpoch: 3 } };
		}
		return { data: { setProgress: true } };
	});
	await assert.rejects(backend.setProgress('42', 1, false), /offline/);
	await backend.setProgress('42', 1, false);
	assert.equal(reads, 2);
	assert.equal(bodies.at(-1)!.variables?.numericEpoch, 3);
});

test('M1 anonymous reads skip the epoch and keep cacheable GET documents', async () => {
	const requests: { query: string; method?: string }[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (input, init) => {
			requests.push({ query: new URL(String(input)).searchParams.get('query')!, method: init?.method });
			return Response.json({ data: { chapters: [{ id: '42' }] } });
		},
	});
	await backend.chapters('7');
	assert.equal(requests.length, 1);
	assert.ok(!isEpochRead(requests[0]));
	assert.equal(requests[0].method, undefined, 'anonymous reads still use GET');
});

test('M1 signed-in fetch views share one epoch request sent before the content', async () => {
	const requests: string[] = [];
	let finish!: (response: Response) => void;
	const mockFetch: typeof fetch = async (_url, init) => {
		const body = JSON.parse(String(init?.body));
		requests.push(body.query);
		if (isEpochRead(body)) return new Promise<Response>((resolve) => (finish = resolve));
		return Response.json({ data: { chapters: [{ id: '42' }], library: [{ id: '7' }] } });
	};
	const backend = new GraphQLBackend({ endpoint: endpoint(), token: 'A', fetch: mockFetch });
	const view = backend.withFetch(mockFetch);
	let done = false;
	const pending = Promise.all([backend.chapters('7'), view.library()]).then(() => (done = true));
	assert.ok(isEpochRead({ query: requests[0] }), 'the epoch request goes out first');
	await new Promise((resolve) => setTimeout(resolve, 0));
	assert.equal(requests.length, 1, 'content requests wait for the epoch response');
	assert.equal(done, false, 'ids are not handed out before the epoch is known');
	finish(Response.json({ data: { numericEpoch: 3 } }));
	await pending;
	assert.equal(requests.length, 3, 'both content requests follow the epoch response');
	assert.equal(requests.filter((q) => isEpochRead({ query: q })).length, 1);
	assert.ok(requests.slice(1).every((q) => !q.includes('numericEpoch')));
});

test('M1 content requests stop after an account switch during the epoch read', async () => {
	const requests: string[] = [];
	let finish!: (response: Response) => void;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		fetch: async (_url, init) => {
			const body = JSON.parse(String(init?.body));
			requests.push(body.query);
			if (isEpochRead(body)) return new Promise<Response>((resolve) => (finish = resolve));
			return Response.json({ data: { chapters: [{ id: '42' }] } });
		},
	});
	const pending = assert.rejects(backend.chapters('7'), /Session changed/);
	backend.setToken('B');
	finish(Response.json({ data: { numericEpoch: 3 } }));
	await pending;
	assert.equal(requests.length, 1);
	assert.ok(isEpochRead({ query: requests[0] }));
});

test("M1 a failed epoch read from a signed-out account does not pin the next account's epoch", async (t) => {
	captureWarnings(t);
	const bodies: { query: string; variables?: Record<string, unknown> }[] = [];
	let failFirst!: (response: Response) => void;
	let epochReads = 0;
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		token: 'A',
		fetch: async (_url, init) => {
			const body = JSON.parse(String(init?.body));
			bodies.push(body);
			if (isEpochRead(body)) {
				if (++epochReads === 1) return new Promise<Response>((resolve) => (failFirst = resolve));
				return Response.json({ data: { numericEpoch: 3 } });
			}
			return Response.json({ data: { chapters: [{ id: '42' }], setProgress: true } });
		},
	});
	const pending = assert.rejects(backend.chapters('7'), /Session changed/);
	backend.setToken('B');
	failFirst(new Response('down', { status: 500 }));
	await pending;
	await backend.chapters('7');
	await backend.setProgress('42', 1, false);
	assert.equal(epochReads, 2, "B reads its own epoch");
	assert.equal(bodies.at(-1)!.variables?.numericEpoch, 3);
});

test('M2 numeric writes cannot cross an account switch during the first epoch read', async (t) => {
	for (const method of ['setProgress', 'setChapterRead', 'mark'] as const) {
		await t.test(method, async () => {
			const requests: { query: string; token: string | null }[] = [];
			let finish!: (response: Response) => void;
			const backend = new GraphQLBackend({
				endpoint: endpoint(),
				token: 'A',
				fetch: async (_url, init) => {
					const body = JSON.parse(String(init?.body));
					requests.push({ query: body.query, token: new Headers(init?.headers).get('authorization') });
					if (isEpochRead(body)) return new Promise<Response>((resolve) => (finish = resolve));
					return Response.json({ data: { [method]: true } });
				},
			});
			const pending = method === 'setProgress'
				? backend.setProgress('42', 1, false)
				: backend[method]('42', true);
			const outcome = pending.then(() => null, (e: unknown) => e);
			assert.ok(isEpochRead(requests[0]));
			backend.setToken(null);
			backend.setToken('B');
			finish(Response.json({ data: { numericEpoch: 3 } }));
			assert.match(String(await outcome), /Session changed/);
			assert.deepEqual(requests.map((r) => r.token), ['Bearer A']);
		});
	}
});

test('M2 composite progress stops after an account switch during the epoch read', async () => {
	let finish!: (epoch: number) => void;
	let sent = 0;
	const hosted = {
		setToken: () => {},
		numericEpoch: () => new Promise<number>((resolve) => (finish = resolve)),
		setProgress: async () => { sent++; },
	} as unknown as Backend;
	const { composite, writes } = signedIn(hosted);
	const pending = composite.setProgress('42', 1, false);
	composite.setToken('token-B');
	composite.setCurrentUser('B');
	finish(3);
	await pending;
	assert.equal(sent, 0);
	assert.equal(writes.size(), 0);
});

test('M3 stale live progress notifies once across fetch views while replays stay silent', async (t) => {
	const { onStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
	assert.equal(typeof onStaleNumericEpoch, 'function');
	let notices = 0;
	const unsubscribe = onStaleNumericEpoch(() => { notices++; });
	t.after(unsubscribe);
	const answer = (b: { query: string }) =>
		isEpochRead(b) ? { data: { numericEpoch: 3 } } : staleResponse();
	const { backend } = recordingBackend(answer);
	await assert.rejects(backend.setProgress('42', 1, false, 2));
	await assert.rejects(backend.setProgress('42', 1, false, null));
	await assert.rejects(backend.setProgress('uuid-chapter', 1, false));
	assert.equal(notices, 0);
	const view = backend.withFetch(async (_url, init) =>
		Response.json(answer(JSON.parse(String(init?.body)))),
	);
	await assert.rejects(view.setProgress('42', 1, false));
	await assert.rejects(backend.setChapterRead('42', true));
	await assert.rejects(backend.setProgress('42', 2, false));
	assert.equal(notices, 1);
	const other = recordingBackend(answer).backend;
	await assert.rejects(other.setChapterRead('42', true));
	assert.equal(notices, 2, 'a separate backend has its own notification fence');
	unsubscribe();
	await assert.rejects(recordingBackend(answer).backend.setProgress('42', 1, false));
	assert.equal(notices, 2, 'unsubscribed listeners are removed');
});

test('M3 composite live progress notifies without queueing the stale write', async (t) => {
	captureWarnings(t);
	const { onStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
	assert.equal(typeof onStaleNumericEpoch, 'function');
	let notices = 0;
	t.after(onStaleNumericEpoch(() => { notices++; }));
	const { backend } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 3 } } : staleResponse(),
	);
	const { composite, writes } = signedIn(backend);
	await composite.setProgress('42', 1, false);
	await composite.setProgress('42', 2, false);
	assert.equal(notices, 1);
	assert.equal(writes.size(), 0);
});

test('composite live library edits notify while explicit epochs stay silent', async (t) => {
	const { onStaleNumericEpoch, isStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
	for (const field of ['mark', 'setLibraryStatus', 'setFavorite'] as const) {
		await t.test(field, async (t) => {
			let notices = 0;
			t.after(onStaleNumericEpoch(() => { notices++; }));
			const { backend, bodies } = recordingBackend((b) =>
				isEpochRead(b) ? { data: { numericEpoch: 3 } } : staleResponse(),
			);
			const { composite, writes } = signedIn(backend);
			const write = (id: string, epoch?: number | null) => field === 'setLibraryStatus'
				? composite.setLibraryStatus(id, 'reading', epoch)
				: composite[field](id, true, epoch);
			await assert.rejects(write('7', 2), isStaleNumericEpoch);
			await assert.rejects(write('7', null), isStaleNumericEpoch);
			await assert.rejects(write('w_canonical'), isStaleNumericEpoch);
			assert.equal(notices, 0);
			await assert.rejects(write('7'), isStaleNumericEpoch);
			await assert.rejects(write('8'), isStaleNumericEpoch);
			assert.equal(notices, 1);
			assert.equal(writes.size(), 0);
			assert.equal(bodies.filter(isEpochRead).length, 1);
			assert.deepEqual(bodies.filter((b) => !isEpochRead(b)).map((b) => b.variables?.numericEpoch),
				[2, undefined, undefined, 3, 3]);
		});
	}
});

test('M7 numeric marks send the cached or explicit epoch and uuid marks send none', async () => {
	const { backend, bodies } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 3 } } : { data: { mark: { id: '7' } } },
	);
	await backend.mark('7', true);
	await backend.mark('8', false);
	await backend.mark('7', true, 2);
	await backend.mark('7', true, null);
	await backend.mark('uuid-series', true, 2);
	assert.equal(bodies.filter(isEpochRead).length, 1);
	assert.deepEqual(bodies.filter((b) => !isEpochRead(b)).map((b) => b.variables?.numericEpoch),
		[3, 3, 2, undefined, undefined]);
	assert.match(bodies[1].query, /\$numericEpoch: Int/);
	assert.match(bodies[1].query, /mark\([^)]*numericEpoch: \$numericEpoch/);
});

test('numeric shelf and favourite edits send pinned or explicit epochs through the composite backend', async () => {
	for (const field of ['setLibraryStatus', 'setFavorite'] as const) {
		let epoch = 3;
		const { backend, bodies } = recordingBackend((b) =>
			isEpochRead(b) ? { data: { numericEpoch: epoch } } : { data: { [field]: { id: '7' } } },
		);
		const { composite } = signedIn(backend);
		const write = (id: string, given?: number | null) => field === 'setLibraryStatus'
			? composite.setLibraryStatus(id, 'reading', given)
			: composite.setFavorite(id, true, given);
		await write('7');
		epoch = 4;
		await write('8');
		await write('7', 2);
		await write('7', null);
		await write('w_canonical', 2);
		assert.equal(bodies.filter(isEpochRead).length, 1);
		const edits = bodies.filter((b) => !isEpochRead(b));
		assert.deepEqual(edits.map((b) => b.variables?.numericEpoch), [3, 3, 2, undefined, undefined]);
		assert.ok(edits.every((b) => b.query.includes('numericEpoch: $numericEpoch')));
		assert.ok(edits.every((b) => b.query.includes('$numericEpoch: Int')));
	}
});

test('old library epoch arguments are stripped per mutation without disabling other fences', async () => {
	for (const field of ['setLibraryStatus', 'setFavorite'] as const) {
		const other = field === 'setLibraryStatus' ? 'setFavorite' : 'setLibraryStatus';
		const { backend, bodies } = recordingBackend((b) => {
			if (isEpochRead(b)) return { data: { numericEpoch: 3 } };
			if (b.query.includes(`${field}(`) && b.query.includes('numericEpoch')) {
				return { errors: [{ message: `Unknown argument "numericEpoch" on field "${field}"` }] };
			}
			return { data: { setLibraryStatus: { id: '7' }, setFavorite: { id: '7' }, mark: { id: '7' }, setProgress: true } };
		});
		const write = () => field === 'setLibraryStatus'
			? backend.setLibraryStatus('7', 'reading')
			: backend.setFavorite('7', true);
		await write();
		await write();
		if (other === 'setFavorite') await backend.setFavorite('7', true);
		else await backend.setLibraryStatus('7', 'reading');
		await backend.mark('7', true);
		await backend.setProgress('8', 1, true);
		const edits = bodies.filter((b) => b.query.includes(`${field}(`));
		assert.equal(edits.length, 3, 'one strip and retry, then the remembered form');
		assert.ok(edits[0].query.includes('numericEpoch'));
		assert.ok(edits.slice(1).every((b) => !b.query.includes('numericEpoch')));
		const supported = bodies.filter((b) => b.query.includes(`${other}(`) || /mutation (Mark|SetProgress)\(/.test(b.query));
		assert.equal(supported.length, 3);
		assert.ok(supported.every((b) => b.query.includes('numericEpoch: $numericEpoch')));
		assert.ok(supported.every((b) => b.variables?.numericEpoch === 3));
	}
});

test('legacy servers without numeric epochs still accept shelf and favourite edits', async () => {
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) return { errors: [{ message: 'Unknown field "numericEpoch"' }] };
		if (b.query.includes('numericEpoch')) return { errors: [{ message: 'Unknown argument "numericEpoch"' }] };
		return { data: { setLibraryStatus: { id: '7' }, setFavorite: { id: '7' } } };
	});
	await backend.setLibraryStatus('7', 'reading');
	await backend.setFavorite('7', true);
	assert.equal(bodies.filter(isEpochRead).length, 1);
	assert.ok(bodies.filter((b) => !isEpochRead(b)).every((b) => b.variables?.numericEpoch === undefined));
	assert.ok(!bodies.at(-1)!.query.includes('numericEpoch'));
});

test('stale live library edits notify once and never refresh the pinned epoch', async (t) => {
	const { onStaleNumericEpoch, isStaleNumericEpoch } = await import('../../../../../packages/api/src/numeric-epoch.ts');
	let notices = 0;
	t.after(onStaleNumericEpoch(() => notices++));
	const { backend, bodies } = recordingBackend((b) =>
		isEpochRead(b) ? { data: { numericEpoch: 3 } } : staleResponse(),
	);
	await assert.rejects(backend.setFavorite('7', true, 2), isStaleNumericEpoch);
	assert.equal(notices, 0, 'an explicit epoch has the replay rule');
	await assert.rejects(backend.setLibraryStatus('7', 'reading'), isStaleNumericEpoch);
	await assert.rejects(backend.setFavorite('7', true), isStaleNumericEpoch);
	assert.equal(notices, 1);
	assert.equal(bodies.filter(isEpochRead).length, 1);
	assert.deepEqual(bodies.filter((b) => !isEpochRead(b)).map((b) => b.variables?.numericEpoch), [2, 3, 3]);
});

test('M7 an old server strips the mark epoch argument and remembers the fallback', async () => {
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) return { errors: [{ message: 'Unknown field "numericEpoch"' }] };
		if (b.query.includes('numericEpoch'))
			return { errors: [{ message: 'Unknown argument "numericEpoch" on field "mark"' }] };
		return { data: { mark: { id: '7' } } };
	});
	await backend.mark('7', true);
	await backend.mark('7', false, 2);
	const marks = bodies.filter((b) => !isEpochRead(b));
	assert.equal(bodies.filter(isEpochRead).length, 1);
	assert.equal(marks.length, 3);
	assert.ok(marks[0].query.includes('numericEpoch'));
	assert.ok(marks.slice(1).every((b) => !b.query.includes('numericEpoch')));
});

test('M7 a mark-only epoch fallback keeps numericEpoch on progress writes', async () => {
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) return { data: { numericEpoch: 3 } };
		if (/mutation Mark/.test(b.query)) {
			if (b.query.includes('numericEpoch'))
				return { errors: [{ message: 'Unknown argument "numericEpoch" on field "mark"' }] };
			return { data: { mark: { id: '7' } } };
		}
		if (!b.query.includes('numericEpoch') || b.variables?.numericEpoch !== 3) return staleResponse();
		return { data: { setProgress: true, setChapterRead: true } };
	});
	await backend.mark('7', true);
	await backend.setProgress('42', 1, false);
	await backend.setChapterRead('42', true);
	const progress = bodies.filter((b) => /mutation Set(?:Progress|ChapterRead)/.test(b.query));
	assert.equal(progress.length, 2);
	assert.ok(progress.every((b) => b.query.includes('numericEpoch: $numericEpoch')));
	assert.deepEqual(progress.map((b) => b.variables?.numericEpoch), [3, 3]);
});

test('M7 offline numeric marks persist their epoch and stale replay drops without a retry', async (t) => {
	captureWarnings(t);
	let epoch = 3;
	let online = true;
	const { backend, bodies } = recordingBackend((b) => {
		if (isEpochRead(b)) return { data: { numericEpoch: epoch } };
		if (!online) throw new Error('offline');
		if (b.variables?.numericEpoch !== epoch) return staleResponse();
		return { data: { mark: { id: '7' } } };
	});
	const storage = createMemoryStorageAdapter();
	const queue = new OfflineWriteQueue(storage);
	const composite = new CompositeBackend({ hosted: backend, queue });
	composite.setToken('A');
	composite.setCurrentUser('A');
	await composite.mark('7', true);
	online = false;
	await assert.rejects(composite.mark('8', true), /offline/);
	const persisted = new OfflineWriteQueue(storage);
	assert.equal(persisted.peekAll()[0].numericEpoch, 3);
	epoch = 4;
	online = true;
	const replay = new CompositeBackend({ hosted: backend, queue: persisted });
	replay.setCurrentUser('A');
	await new Promise((resolve) => setTimeout(resolve, 0));
	assert.equal(persisted.size(), 0);
	assert.deepEqual(bodies.filter((b) => b.variables?.seriesId === '8').map((b) => b.variables?.numericEpoch), [3, 3]);
	await assert.rejects(replay.mark('9', true), /Earlier engine generation/);
	assert.equal(persisted.size(), 0, 'a stale live mark is rethrown without enqueueing');
	assert.equal(bodies.filter(isEpochRead).length, 1);
});

test('M7 legacy and unknown mark epochs replay without reading a fresh epoch', async (t) => {
	captureWarnings(t);
	for (const numericEpoch of [undefined, null]) {
		const storage = createMemoryStorageAdapter();
		storage.save(JSON.stringify([
			{ kind: 'mark', seriesId: '7', marked: true, ts: 1, tries: 0, userId: 'A', numericEpoch },
		]));
		const queue = new OfflineWriteQueue(storage);
		const { backend, bodies } = recordingBackend(() => staleResponse());
		const composite = new CompositeBackend({ hosted: backend, queue });
		composite.setCurrentUser('A');
		await new Promise((resolve) => setTimeout(resolve, 0));
		assert.equal(queue.size(), 0);
		assert.equal(bodies.length, 1);
		assert.ok(!isEpochRead(bodies[0]));
		assert.equal(bodies[0].variables?.numericEpoch, undefined);
	}
});

test('M7 composite marks stop after an account switch during the epoch read', async () => {
	let finish!: (epoch: number) => void;
	let sent = 0;
	const hosted = {
		setToken: () => {},
		numericEpoch: () => new Promise<number>((resolve) => (finish = resolve)),
		mark: async () => { sent++; return { id: '7' }; },
	} as unknown as Backend;
	const { composite, writes } = signedIn(hosted);
	const pending = assert.rejects(composite.mark('7', true), /identity changed/);
	composite.setToken('token-B');
	composite.setCurrentUser('B');
	finish(3);
	await pending;
	assert.equal(sent, 0);
	assert.equal(writes.size(), 0);
});

test('H2 starting a new engine generation names the generation the admin saw', async () => {
	const { backend, bodies } = recordingBackend(() => ({ data: { startNewEngineGeneration: [] } }));
	await backend.startNewEngineGeneration!('gen-shown');
	assert.deepEqual(bodies[0].variables, { generationId: 'gen-shown' });
	assert.match(bodies[0].query, /startNewEngineGeneration\(generationId: \$generationId\)/);
});
