import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { afterEach, beforeEach, mock, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';
import './node-test-hooks.ts';

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

const { createSessionKeeper, notifyAuthRejected, onAuthRejected } =
	await import('../../../../../packages/api/src/session-keeper.ts');
const { GraphQLBackend } = await import('../../../../../packages/api/src/graphql-backend.ts');

type Answer = { token: string; user: { id: string } } | null;

/** A browser window and document: just enough for `watch()` to hang its listeners on. */
const win = new EventTarget();
const doc = Object.assign(new EventTarget(), { visibilityState: 'visible' });

beforeEach(() => {
	Object.assign(globalThis, { window: win, document: doc });
	mock.timers.enable({ apis: ['setTimeout', 'Date'] });
});
afterEach(() => {
	mock.timers.reset();
	delete (globalThis as { window?: unknown }).window;
	delete (globalThis as { document?: unknown }).document;
});

/** A keeper over a fake server. `answers` are served in order; an Error is thrown. */
function harness(answers: (Answer | Error | Promise<Answer>)[]) {
	const state = { token: 'A' as string | null, operation: 0, asked: 0, applied: [] as Answer[] };
	const keeper = createSessionKeeper({
		session: async () => {
			state.asked++;
			const next = await answers.shift();
			if (next instanceof Error) throw next;
			return (next ?? null) as never;
		},
		readToken: () => state.token,
		operation: () => state.operation,
		apply: (session) => {
			state.applied.push(session as Answer);
		},
		cleared: () => {},
	});
	return { state, keeper };
}

const flush = () => new Promise<void>((resolve) => setImmediate(resolve));

test('a check with no answer is retried, backing off, until the server answers', async () => {
	const { state, keeper } = harness([
		new Error('504'),
		new Error('504'),
		{ token: 'A', user: { id: 'A' } },
	]);
	const stop = keeper.watch();
	assert.equal(await keeper.check(), false);
	assert.deepEqual(state.applied, [], 'no answer is never applied as a sign-out');
	mock.timers.tick(2_000);
	await flush();
	assert.equal(state.asked, 2);
	mock.timers.tick(4_999);
	await flush();
	assert.equal(state.asked, 2, 'the second wait is longer');
	mock.timers.tick(1);
	await flush();
	assert.equal(state.asked, 3);
	assert.deepEqual(state.applied, [{ token: 'A', user: { id: 'A' } }]);
	mock.timers.tick(600_000);
	await flush();
	assert.equal(state.asked, 3, 'an answer stops the retries');
	stop();
});

test('a token swapped while the check was in flight is asked about again', async () => {
	let finish!: (answer: Answer) => void;
	const first = new Promise<Answer>((resolve) => (finish = resolve));
	const { state, keeper } = harness([first, { token: 'B', user: { id: 'B' } }]);
	const checking = keeper.check();
	state.token = 'B'; // Another tab signed in as B.
	finish({ token: 'A', user: { id: 'A' } });
	assert.equal(await checking, true);
	assert.equal(state.asked, 2);
	assert.deepEqual(state.applied, [{ token: 'B', user: { id: 'B' } }]);
});

test('coming back into view asks again at most once a minute', async () => {
	const { state, keeper } = harness([
		{ token: 'A', user: { id: 'A' } },
		{ token: 'A', user: { id: 'A' } },
	]);
	const stop = keeper.watch();
	await keeper.check();
	win.dispatchEvent(new Event('focus'));
	await flush();
	assert.equal(state.asked, 1);
	mock.timers.tick(60_000);
	doc.dispatchEvent(new Event('visibilitychange'));
	await flush();
	assert.equal(state.asked, 2);
	stop();
	mock.timers.tick(60_000);
	win.dispatchEvent(new Event('focus'));
	await flush();
	assert.equal(state.asked, 2, 'stop() removes the listeners');
});

test('a refused request asks the server, which then ends the session', async () => {
	const { state, keeper } = harness([{ token: 'A', user: { id: 'A' } }, null]);
	const stop = keeper.watch();
	await keeper.check();
	mock.timers.tick(10_000);
	notifyAuthRejected();
	notifyAuthRejected(); // A run of refusals makes one check.
	await flush();
	assert.equal(state.asked, 2);
	assert.deepEqual(state.applied.at(-1), null);
	stop();
});

test('GraphQLBackend reports a refused token, and only a token it sent', async () => {
	let rejected = 0;
	const stop = onAuthRejected(() => rejected++);
	const backend = new GraphQLBackend({
		endpoint: 'https://fixture-auth.test/graphql',
		fetch: async () => Response.json({ errors: [{ message: 'Not authenticated' }] }),
	});
	await assert.rejects(() => backend.logout(), /Not authenticated/);
	assert.equal(rejected, 0, 'an anonymous request has no session to end');
	backend.setToken('A');
	await assert.rejects(() => backend.logout(), /Not authenticated/);
	assert.equal(rejected, 1);
	stop();
});
