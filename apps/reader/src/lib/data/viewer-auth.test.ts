import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { beforeEach, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { compileModule } from 'svelte/compiler';
import ts from 'typescript';
import { testGlobals as g } from './node-test-hooks.ts';

registerHooks({
	resolve(specifier, context, next) {
		if (
			specifier === './context' &&
			/\/(auth|block-rules)\.svelte\.ts$/.test(context.parentURL ?? '')
		) {
			return {
				url: 'data:text/javascript,export const backend = globalThis.__komikaTest.backend;',
				shortCircuit: true,
			};
		}
		// The auth store's runtime import from the shared client is the session keeper.
		if (specifier === '@komika/api' && /\/auth\.svelte\.ts$/.test(context.parentURL ?? '')) {
			return {
				url: new URL('../../../../../packages/api/src/session-keeper.ts', import.meta.url).href,
				shortCircuit: true,
			};
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (/\/(auth|block-rules|translator-pref)\.svelte\.ts$/.test(url)) {
			const source = ts.transpileModule(readFileSync(fileURLToPath(url), 'utf8'), {
				compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
			}).outputText;
			return {
				format: 'module',
				source: compileModule(source, { filename: fileURLToPath(url), generate: 'client' }).js.code,
				shortCircuit: true,
			};
		}
		return next(url, context);
	},
});

const storage = new Map<string, string>();
Object.defineProperty(globalThis, 'localStorage', {
	configurable: true,
	value: {
		getItem: (key: string) => storage.get(key) ?? null,
		setItem: (key: string, value: string) => storage.set(key, value),
		removeItem: (key: string) => storage.delete(key),
	},
});

const { viewerCache, refreshViewerLoads } = await import('../viewer-cache.svelte.ts');
const { auth, initAuth, login, logout, revalidateSession, saveShowNsfw, TOKEN_KEY, USER_KEY } =
	await import('../auth.svelte.ts');
const { blockRulesChanged, saveBlockRules } = await import('../block-rules.svelte.ts');
const { setPreferredTranslator, clearPreferredTranslators } =
	await import('./translator-pref.svelte.ts');

function session(id = 'A', token = id, showNsfw = false, isAdmin = false) {
	return {
		token,
		user: {
			id,
			username: id,
			displayName: null,
			bio: null,
			avatarUrl: null,
			isAdmin,
			showNsfw,
			joinedAt: '2026-10-13T00:00:00Z',
		},
	};
}

beforeEach(async () => {
	await refreshViewerLoads();
	storage.clear();
	auth.user = null;
	auth.ready = false;
	g.navigating.to = null;
	g.navigating.complete = undefined;
	for (const key of Object.keys(g.backend)) delete g.backend[key];
	g.backend.setToken = () => {};
	g.backend.session = async () => session();
	g.backend.login = async () => session();
	g.backend.logout = async () => {};
});

test('restore, login with a renewed same-user token, and logout advance viewer generation', async () => {
	storage.set(TOKEN_KEY, 'A');
	let generation = viewerCache.generation;
	await initAuth();
	assert.equal(auth.user?.id, 'A');
	assert.ok(viewerCache.generation > generation);
	generation = viewerCache.generation;
	g.backend.login = async () => session('A', 'A-renewed');
	await login('A', 'password');
	assert.equal(auth.user?.id, 'A');
	assert.ok(viewerCache.generation > generation);
	generation = viewerCache.generation;
	await logout();
	assert.equal(auth.user, null);
	assert.equal(storage.has(TOKEN_KEY), false);
	assert.ok(viewerCache.generation > generation);
});

test('server revocation and revalidated content preferences invalidate previous results', async () => {
	await login('A', 'password');
	let generation = viewerCache.generation;
	g.backend.session = async () => session('A', 'A', true);
	await revalidateSession();
	assert.equal(auth.user?.showNsfw, true);
	assert.ok(viewerCache.generation > generation);
	generation = viewerCache.generation;
	g.backend.session = async () => null;
	await revalidateSession();
	assert.equal(auth.user, null);
	assert.equal(storage.has(TOKEN_KEY), false);
	assert.ok(viewerCache.generation > generation);
});

test('a restored token the server revoked clears the session on startup', async () => {
	storage.set(TOKEN_KEY, 'A');
	g.backend.session = async () => null;
	const generation = viewerCache.generation;
	await initAuth();
	assert.equal(auth.user, null);
	assert.equal(storage.has(TOKEN_KEY), false);
	assert.ok(viewerCache.generation > generation);
});

test('a start-up check with no answer keeps the last confirmed user signed in', async () => {
	await login('A', 'password');
	auth.user = null;
	auth.ready = false;
	g.backend.session = async () => {
		throw new Error('Backend error 504');
	};
	await initAuth();
	assert.equal(auth.ready, true);
	// `auth.user` was set to null above; TypeScript cannot see initAuth change it.
	assert.equal((auth.user as { id: string } | null)?.id, 'A', 'the remembered user is shown');
	assert.equal(storage.get(TOKEN_KEY), 'A');
	g.backend.session = async () => null;
	await revalidateSession();
	assert.equal(auth.user, null, 'only the server ends the session');
	assert.equal(storage.has(TOKEN_KEY), false);
	assert.equal(storage.has(USER_KEY), false, 'the remembered user goes with the token');
});

test('a token another tab stored during the start-up check is checked in its place', async () => {
	storage.set(TOKEN_KEY, 'A');
	let finish!: (value: ReturnType<typeof session>) => void;
	const calls: string[] = [];
	// Like the real backend, the session query sends the token held in memory.
	let sent: string | null = null;
	g.backend.setToken = (token: string | null) => (sent = token);
	g.backend.session = () => {
		const token = sent!;
		calls.push(token);
		return token === 'A'
			? new Promise<ReturnType<typeof session>>((resolve) => (finish = resolve))
			: Promise.resolve(session('B'));
	};
	const pending = initAuth();
	storage.set(TOKEN_KEY, 'B');
	finish(session('A'));
	await pending;
	assert.deepEqual(calls, ['A', 'B']);
	assert.equal(auth.user?.id, 'B');
});

test('a tab still on an older token checks the stored one and keeps the pairing', async () => {
	let sent: string | null = null;
	g.backend.setToken = (token: string | null) => (sent = token);
	g.backend.session = async () => (sent === 'B' ? session('B') : null);
	await login('A', 'password');
	storage.set(TOKEN_KEY, 'B'); // Another tab signed out of A and in as B.
	storage.delete(USER_KEY);
	g.backend.setShowNsfw = async () => true;
	await saveShowNsfw(true);
	assert.equal(storage.has(USER_KEY), false, "A is never saved as B's user");
	await revalidateSession();
	assert.equal(auth.user?.id, 'B');
	assert.equal(storage.get(TOKEN_KEY), 'B', "B's token survives the check");
	assert.equal(JSON.parse(storage.get(USER_KEY)!).id, 'B');
});

test('same-token admin demotion invalidates completed viewer results', async () => {
	g.backend.login = async () => session('A', 'A', false, true);
	await login('A', 'password');
	assert.equal(auth.user?.isAdmin, true);
	const generation = viewerCache.generation;
	g.backend.session = async () => session('A');
	await revalidateSession();
	assert.equal(auth.user?.isAdmin, false);
	assert.equal(storage.get(TOKEN_KEY), 'A');
	assert.ok(viewerCache.generation > generation);
});

test('saved block rules and translator preferences invalidate preloads', () => {
	let generation = viewerCache.generation;
	blockRulesChanged();
	assert.ok(viewerCache.generation > generation);
	generation = viewerCache.generation;
	setPreferredTranslator('w_fixture', 'suwayomi:77');
	assert.ok(viewerCache.generation > generation);
	generation = viewerCache.generation;
	setPreferredTranslator('w_fixture', 'suwayomi:77');
	assert.equal(viewerCache.generation, generation, 'the same preference has no transition');
	clearPreferredTranslators();
	assert.ok(viewerCache.generation > generation);
});

test('a late logout cannot erase a newer signed-in viewer', async () => {
	await login('A', 'password');
	let finish!: () => void;
	g.backend.logout = () => new Promise<void>((resolve) => (finish = resolve));
	const pending = logout();
	g.backend.login = async () => session('B');
	await login('B', 'password');
	finish();
	await pending;
	assert.equal(auth.user?.id, 'B');
	assert.equal(storage.get(TOKEN_KEY), 'B');
});

test('a preference change cannot cancel a successful logout', async () => {
	await login('A', 'password');
	let finish!: () => void;
	g.backend.logout = () => new Promise<void>((resolve) => (finish = resolve));
	const pending = logout();
	blockRulesChanged();
	finish();
	assert.deepEqual(await pending, { revoked: true });
	assert.equal(auth.user, null);
	assert.equal(storage.has(TOKEN_KEY), false);
});

test('the newest login intent owns auth even when an older login completes last', async () => {
	let finish!: (value: ReturnType<typeof session>) => void;
	g.backend.login = (username: string) =>
		username === 'A'
			? new Promise<ReturnType<typeof session>>((resolve) => (finish = resolve))
			: Promise.resolve(session('B'));
	const pending = login('A', 'password');
	await login('B', 'password');
	finish(session('A'));
	await pending;
	assert.equal(auth.user?.id, 'B');
	assert.equal(storage.get(TOKEN_KEY), 'B');
});

test('a saved NSFW preference survives an unrelated viewer change but never reaches another account', async () => {
	await login('A', 'password');
	let finish!: (value: boolean) => void;
	g.backend.setShowNsfw = () => new Promise<boolean>((resolve) => (finish = resolve));
	let pending = saveShowNsfw(true);
	setPreferredTranslator('w_nsfw', 'suwayomi:1'); // Unrelated generation bump.
	const generation = viewerCache.generation;
	finish(true);
	assert.equal(await pending, true);
	assert.equal(auth.user?.showNsfw, true, 'the stored server value is applied');
	assert.ok(viewerCache.generation > generation, 'results read without NSFW are invalidated');

	pending = saveShowNsfw(false);
	g.backend.login = async () => session('B');
	await login('B', 'password');
	const afterB = viewerCache.generation;
	finish(false);
	assert.equal(await pending, null);
	assert.equal(auth.user?.id, 'B');
	assert.equal(auth.user?.showNsfw, false);
	assert.equal(viewerCache.generation, afterB, "A's late save does not touch B's results");
});

test('saved block rules invalidate after an unrelated change and are dropped for another account', async () => {
	await login('A', 'password');
	const rule = { tags: ['fixture'], ratings: [] };
	let finish!: (value: (typeof rule)[]) => void;
	g.backend.setMyBlockRules = () => new Promise<(typeof rule)[]>((resolve) => (finish = resolve));
	let pending = saveBlockRules([rule]);
	clearPreferredTranslators(); // Unrelated generation bump.
	let generation = viewerCache.generation;
	finish([rule]);
	assert.deepEqual(await pending, [rule]);
	assert.ok(viewerCache.generation > generation);

	pending = saveBlockRules([]);
	g.backend.login = async () => session('B');
	await login('B', 'password');
	generation = viewerCache.generation;
	finish([]);
	assert.equal(await pending, null);
	assert.equal(viewerCache.generation, generation);
});
