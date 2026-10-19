import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire, registerHooks } from 'node:module';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

// Use the admin's existing compiler; the API has no build-time dependencies.
const ts = createRequire(new URL('../../../apps/admin/package.json', import.meta.url))(
	'typescript',
);
// The API uses bundler-style .js imports and constructor parameter properties.
registerHooks({
	resolve(specifier, context, next) {
		if (specifier.startsWith('.') && specifier.endsWith('.js') && context.parentURL) {
			const url = new URL(specifier.slice(0, -3) + '.ts', context.parentURL);
			if (existsSync(fileURLToPath(url))) return { url: url.href, shortCircuit: true };
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (/\/packages\/api\/src\/(graphql|composite)-backend\.ts$/.test(url)) {
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

const { GraphQLBackend } = await import('../src/graphql-backend.ts');
const { TurnstileCheck, turnstileKeyLoader } = await import('../src/turnstile.ts');
const ops = await import('../src/operations.ts');

const DUMMY_TOKEN = 'XXXX.DUMMY.TOKEN.XXXX';
let endpointNumber = 0;
const endpoint = () => `http://localhost:8080/graphql?turnstile=${++endpointNumber}`;

type Sent = { query?: string; variables?: Record<string, unknown> };

/** A backend whose every POST is recorded and answered with `data`. */
function recording(data: unknown) {
	const sent: Sent[] = [];
	const backend = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async (_url: RequestInfo | URL, init?: RequestInit) => {
			sent.push(JSON.parse(String(init?.body ?? '{}')) as Sent);
			return Response.json({ data });
		},
	});
	return { backend, sent };
}

const session = { token: 't', user: { id: 'u', username: 'bob' } };

test('sign-in sends the token in the Turnstile document, and the plain one without it', async () => {
	const { backend, sent } = recording({ login: session });
	await backend.login('bob', 'pw', DUMMY_TOKEN);
	await backend.login('bob', 'pw');
	assert.equal(sent[0].query, ops.LOGIN_TURNSTILE);
	assert.deepEqual(sent[0].variables, {
		username: 'bob',
		password: 'pw',
		turnstileToken: DUMMY_TOKEN,
	});
	// A server without Turnstile may predate the argument: the old document, no token.
	assert.equal(sent[1].query, ops.LOGIN);
	assert.deepEqual(sent[1].variables, { username: 'bob', password: 'pw' });
});

test('password changes and resets send the token the same way', async () => {
	const change = recording({ changePassword: session.user });
	await change.backend.changePassword('old-pw', 'new-pw', DUMMY_TOKEN);
	await change.backend.changePassword('old-pw', 'new-pw');
	assert.equal(change.sent[0].query, ops.CHANGE_PASSWORD_TURNSTILE);
	assert.equal(change.sent[0].variables?.turnstileToken, DUMMY_TOKEN);
	assert.equal(change.sent[1].query, ops.CHANGE_PASSWORD);
	assert.equal('turnstileToken' in (change.sent[1].variables ?? {}), false);

	const reset = recording({ resetUserPassword: true });
	await reset.backend.resetUserPassword('u2', 'temporary1', DUMMY_TOKEN);
	assert.equal(reset.sent[0].query, ops.RESET_USER_PASSWORD_TURNSTILE);
	assert.equal(reset.sent[0].variables?.turnstileToken, DUMMY_TOKEN);
});

test('input mutations carry the token inside their input', async () => {
	const { backend, sent } = recording({
		register: session,
		postComment: { id: 'c' },
		postReview: { id: 'r' },
		submitReport: { id: 'p' },
	});
	await backend.register({
		username: 'n',
		email: 'n@e.com',
		password: 'pw',
		turnstileToken: DUMMY_TOKEN,
	});
	await backend.postComment({
		targetType: 'series',
		targetId: 'w_1',
		body: 'hi',
		hasSpoiler: false,
		turnstileToken: DUMMY_TOKEN,
	});
	await backend.postReview({
		seriesId: 'w_1',
		score: 8,
		body: '',
		hasSpoiler: false,
		turnstileToken: DUMMY_TOKEN,
	});
	await backend.submitReport({
		kind: 'OTHER',
		detail: 'Something is wrong.',
		turnstileToken: DUMMY_TOKEN,
	});
	for (const request of sent) {
		const input = request.variables?.input as Record<string, unknown>;
		assert.equal(input.turnstileToken, DUMMY_TOKEN, request.query?.slice(0, 40));
	}
	// Without a token the field is not sent at all, so an older server sees its old input.
	await backend.register({ username: 'n', email: 'n@e.com', password: 'pw' });
	assert.equal(JSON.stringify(sent.at(-1)).includes('turnstileToken'), false);
});

test('the site key reads as null on a server that predates Turnstile', async () => {
	const on = recording({ instance: { turnstileSiteKey: '1x00000000000000000000AA' } });
	assert.equal(await on.backend.turnstileSiteKey(), '1x00000000000000000000AA');
	const off = recording({ instance: { turnstileSiteKey: null } });
	assert.equal(await off.backend.turnstileSiteKey(), null);
	const old = new GraphQLBackend({
		endpoint: endpoint(),
		fetch: async () =>
			Response.json({
				errors: [{ message: 'Unknown field "turnstileSiteKey" on type "Instance".' }],
			}),
	});
	assert.equal(await old.turnstileSiteKey(), null);
});

test('the site key is asked once, and a failed request is asked again', async () => {
	let calls = 0;
	let fail = true;
	const load = turnstileKeyLoader({
		turnstileSiteKey: async () => {
			calls++;
			if (fail) throw new Error('offline');
			return 'key';
		},
	});
	await assert.rejects(load(), /offline/);
	fail = false;
	assert.equal(await load(), 'key');
	assert.equal(await load(), 'key');
	assert.equal(calls, 2);
	// A backend without Turnstile asks for no check.
	assert.equal(await turnstileKeyLoader({})(), null);
});

/** Cloudflare's widget API, driven by hand. One per run: the script loads once per page. */
function fakeTurnstile() {
	const widgets: Record<string, unknown>[] = [];
	const calls: string[] = [];
	const api = {
		render(_container: unknown, options: Record<string, unknown>) {
			widgets.push(options);
			return `w${widgets.length}`;
		},
		reset(id: string) {
			calls.push(`reset ${id}`);
		},
		remove(id: string) {
			calls.push(`remove ${id}`);
		},
	};
	(globalThis as { window?: unknown }).window = { turnstile: api };
	const callback = (name: string, ...args: unknown[]) =>
		(widgets.at(-1)![name] as (...a: unknown[]) => unknown)(...args);
	return { widgets, calls, callback };
}

const fake = fakeTurnstile();
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

test('a check hands each token out once and starts the next check', async () => {
	const check = new TurnstileCheck({} as HTMLElement, '1x00000000000000000000AA');
	await tick();
	assert.equal(fake.widgets.at(-1)!.sitekey, '1x00000000000000000000AA');
	assert.equal(fake.widgets.at(-1)!.appearance, 'interaction-only');

	// A token that arrived before the form was sent.
	fake.callback('callback', 'token-1');
	assert.equal(await check.token(), 'token-1');
	assert.deepEqual(fake.calls, ['reset w1']);

	// A form sent before the check finished waits for it.
	const waiting = check.token();
	fake.callback('callback', 'token-2');
	assert.equal(await waiting, 'token-2');
	assert.deepEqual(fake.calls, ['reset w1', 'reset w1']);

	// An expired token is never handed out.
	fake.callback('callback', 'token-3');
	fake.callback('expired-callback');
	const afterExpiry = check.token();
	fake.callback('callback', 'token-4');
	assert.equal(await afterExpiry, 'token-4');

	// A failed check rejects the waiting form with a message, and the next try restarts it.
	const failing = check.token();
	assert.equal(fake.callback('error-callback', '300010'), true);
	await assert.rejects(failing, /did not pass/);
	const retry = check.token();
	assert.equal(fake.calls.at(-1), 'reset w1');
	fake.callback('callback', 'token-5');
	assert.equal(await retry, 'token-5');

	check.remove();
	assert.equal(fake.calls.at(-1), 'remove w1');
	await assert.rejects(check.token(), /closed/);
});

test('a browser that cannot run the check says so', async () => {
	const check = new TurnstileCheck({} as HTMLElement, 'key');
	await tick();
	const waiting = check.token();
	fake.callback('unsupported-callback');
	await assert.rejects(waiting, /cannot run the security check/);
	await assert.rejects(check.token(), /cannot run the security check/);
});
