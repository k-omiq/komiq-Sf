/** Run: node --test --experimental-strip-types src/lib/data/social-repo.test.ts */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { beforeEach, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import { persistedServer, testGlobals } from './node-test-hooks.ts';
import type { CommentView } from './social-repo';

const {
	deleteComment,
	loadChapterComments,
	loadSeriesComments,
	submitChapterComment,
	submitSeriesComment,
} = await import('./social-repo.ts');

// Exercise the real shared transport too. Its constructor uses a parameter
// property, which Node's strip-only TypeScript runner cannot erase.
registerHooks({
	load(url, context, next) {
		if (url.endsWith('/packages/api/src/graphql-backend.ts')) {
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
const { GraphQLBackend } = await import('../../../../../packages/api/src/graphql-backend.ts');

const storage = new Map<string, string>();
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: {
		getItem: (key: string) => storage.get(key) ?? null,
		setItem: (key: string, value: string) => storage.set(key, value),
	},
});

beforeEach(() => {
	for (const key of Object.keys(testGlobals.backend)) delete testGlobals.backend[key];
	testGlobals.auth.user = { id: 'me', username: 'Me', isAdmin: false };
	testGlobals.config.backendEnabled = true;
	storage.clear();
	testGlobals.backend.deleteComment = async () => true;
});

function comment(
	id: string,
	parentId: string | null = null,
	overrides: Partial<CommentView> = {},
): CommentView {
	return {
		id,
		parentId,
		authorId: 'me',
		deleted: null,
		name: 'Me',
		initial: 'M',
		avatarUrl: '/avatar.webp',
		bg: 'grey',
		fg: 'white',
		body: 'body',
		time: 'just now',
		hasSpoiler: true,
		mediaUrl: '/image.webp',
		mediaWidth: 10,
		mediaHeight: 20,
		isOp: true,
		mine: true,
		likes: 2,
		dislikes: 1,
		myVote: 1,
		...overrides,
	};
}

test('an author deletes a leaf without removing its parent or siblings', async () => {
	const current = [comment('parent'), comment('leaf', 'parent'), comment('sibling', 'parent')];
	assert.deepEqual(
		(await deleteComment('leaf', current)).map((c) => c.id),
		['parent', 'sibling'],
	);
	assert.equal(current.length, 3);
});

test('an author deletion preserves every reply and blanks identifying content and votes', async () => {
	const current = [comment('root'), comment('reply', 'root'), comment('grandchild', 'reply')];
	const next = await deleteComment('root', current);
	assert.deepEqual(
		next[0],
		comment('root', null, {
			deleted: 'author',
			authorId: '',
			name: '',
			initial: '',
			avatarUrl: null,
			bg: '',
			fg: '',
			body: '',
			hasSpoiler: false,
			mediaUrl: null,
			mediaWidth: null,
			mediaHeight: null,
			isOp: false,
			mine: false,
			likes: 0,
			dislikes: 0,
			myVote: 0,
		}),
	);
	assert.equal(next[1], current[1]);
	assert.equal(next[2], current[2]);
	assert.equal(current[0].body, 'body');
});

test('admin deletion of another author uses a moderator tombstone', async () => {
	testGlobals.auth.user!.isAdmin = true;
	const next = await deleteComment('root', [
		comment('root', null, { authorId: 'other', mine: false }),
		comment('child', 'root'),
	]);
	assert.equal(next[0].deleted, 'moderator');
});

test('an admin deleting their own comment still uses an author tombstone', async () => {
	testGlobals.auth.user!.isAdmin = true;
	const next = await deleteComment('root', [comment('root'), comment('child', 'root')]);
	assert.equal(next[0].deleted, 'author');
});

test('removing the last leaf prunes a chain of tombstones up to a live ancestor', async () => {
	const next = await deleteComment('leaf', [
		comment('live'),
		comment('dead-root', 'live', { deleted: 'author', mine: false }),
		comment('dead-parent', 'dead-root', { deleted: 'moderator', mine: false }),
		comment('leaf', 'dead-parent'),
	]);
	assert.deepEqual(
		next.map((c) => c.id),
		['live'],
	);
});

test('a tombstone with a surviving sibling stays after leaf deletion', async () => {
	const next = await deleteComment('leaf', [
		comment('dead', null, { deleted: 'author', mine: false }),
		comment('leaf', 'dead'),
		comment('sibling', 'dead'),
	]);
	assert.deepEqual(
		next.map((c) => c.id),
		['dead', 'sibling'],
	);
});

test('a non-admin cannot delete another author even with a stale mine flag', async () => {
	let calls = 0;
	testGlobals.backend.deleteComment = async () => {
		calls++;
		return true;
	};
	await assert.rejects(
		deleteComment('other', [comment('other', null, { authorId: 'other', mine: true })]),
		/own comments/,
	);
	assert.equal(calls, 0);
});

test('anonymous deletion is refused before calling the backend', async () => {
	testGlobals.auth.user = null;
	await assert.rejects(deleteComment('root', [comment('root')]), /own comments/);
});

test('a failed server deletion leaves the thread unchanged', async () => {
	testGlobals.backend.deleteComment = async () => {
		throw new Error('Delete failed');
	};
	const current = [comment('root'), comment('child', 'root')];
	await assert.rejects(deleteComment('root', current), /Delete failed/);
	assert.equal(current[0].deleted, null);
	assert.equal(current.length, 2);
});

test('backend mapping treats absent deletedBy as live and normalizes both tombstone kinds', async () => {
	testGlobals.backend.comments = async () => ({
		items: [undefined, 'author', 'moderator'].map((deletedBy, i) => ({
			id: String(i),
			parentId: null,
			author: { id: 'me', username: 'Me', avatarUrl: '/avatar.webp' },
			body: 'body',
			hasSpoiler: true,
			mediaUrl: '/image.webp',
			mediaWidth: 10,
			mediaHeight: 20,
			createdAt: new Date().toISOString(),
			likes: 2,
			dislikes: 1,
			myVote: 1,
			deletedBy,
		})),
	});
	const next = await loadChapterComments('chapter', 'key');
	assert.equal(next[0].deleted, null);
	assert.equal(next[0].mine, true);
	for (const [i, deleted] of ['author', 'moderator'].entries()) {
		assert.equal(next[i + 1].deleted, deleted);
		assert.equal(next[i + 1].name, '');
		assert.equal(next[i + 1].mediaUrl, null);
		assert.equal(next[i + 1].likes, 0);
		assert.equal(next[i + 1].mine, false);
	}
});

for (const targetType of ['chapter', 'series'] as const) {
	test(`offline ${targetType} replies, tombstones and upward pruning survive reloads`, async (t) => {
		testGlobals.config.backendEnabled = false;
		let now = 1;
		t.mock.method(Date, 'now', () => now++);
		const submit = targetType === 'chapter' ? submitChapterComment : submitSeriesComment;
		const load = targetType === 'chapter' ? loadChapterComments : loadSeriesComments;
		let current = await submit('target', 'key', 'root', false, []);
		current = await submit('target', 'key', 'child', false, current, { parentId: current[0].id });
		const [root, child] = current;
		assert.equal((await load('target', 'key'))[1].parentId, root.id);
		assert.equal((await load('target', 'key'))[1].mine, true);
		current = await deleteComment(root.id, current, { targetType, storageKey: 'key' });
		assert.equal((await load('target', 'key'))[0].deleted, 'author');
		await deleteComment(child.id, current, { targetType, storageKey: 'key' });
		assert.deepEqual(await load('target', 'key'), []);
	});
}

test('legacy offline records still load with live defaults', async () => {
	testGlobals.config.backendEnabled = false;
	storage.set(
		'komika-social-v1',
		JSON.stringify({
			comments: {
				'chapter:key': [{ id: 'old', name: 'Old', initial: 'O', body: 'body', likes: 0 }],
			},
		}),
	);
	const [old] = await loadChapterComments('target', 'key');
	assert.equal(old.deleted, null);
	assert.equal(old.parentId, null);
	assert.equal(old.mine, false);
});

test('the comments query selects deletedBy for anonymous GET and signed-in POST', async (t) => {
	const client = new GraphQLBackend({ endpoint: testGlobals.config.apiEndpoint });
	testGlobals.backend.comments = client.comments.bind(client);
	const requests: { query: string; variables: unknown; authorization: string | null }[] = [];
	t.mock.method(
		globalThis,
		'fetch',
		persistedServer(async (input: RequestInfo | URL, init?: RequestInit) => {
			const url = new URL(String(input));
			const payload = init?.body
				? JSON.parse(String(init.body))
				: {
						query: url.searchParams.get('query'),
						variables: JSON.parse(url.searchParams.get('variables')!),
					};
			requests.push({ ...payload, authorization: new Headers(init?.headers).get('authorization') });
			return Response.json({
				data: { comments: { items: [], page: 1, total: 0, hasNextPage: false } },
			});
		}),
	);
	await loadChapterComments('chapter', 'key');
	client.setToken('test-token');
	await loadSeriesComments('series', 'key');
	assert.equal(requests.length, 2);
	assert.match(requests[0].query, /\bdeletedBy\b/);
	assert.match(requests[1].query, /\bdeletedBy\b/);
	assert.equal(requests[0].authorization, null);
	assert.equal(requests[1].authorization, 'Bearer test-token');
	assert.deepEqual(requests[0].variables, { targetType: 'chapter', targetId: 'chapter', page: 1 });
	assert.deepEqual(requests[1].variables, { targetType: 'series', targetId: 'series', page: 1 });
});
