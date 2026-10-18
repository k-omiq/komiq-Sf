import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { release } from './release.mjs';

// A fake app whose every build emits one new chunk plus one unchanged chunk.
function fakeApp() {
	const root = mkdtempSync(join(tmpdir(), 'komiq-release-'));
	const app = join(root, 'app');
	const calls = [];
	let builds = 0;
	/** @param {string} command @param {string[]} args @param {{ env?: NodeJS.ProcessEnv }} options */
	const exec = (command, args, options) => {
		calls.push({ command, args, target: options.env?.KOMIKA_TARGET });
		if (args[0] !== 'build') return;
		builds++;
		const output = join(
			app,
			options.env?.KOMIKA_TARGET === 'static' ? 'build' : '.svelte-kit/cloudflare',
		);
		const chunks = join(output, '_app/immutable/chunks');
		rmSync(output, { recursive: true, force: true });
		mkdirSync(chunks, { recursive: true });
		writeFileSync(join(chunks, `release-${builds}.js`), `export const build = ${builds};`);
		writeFileSync(join(chunks, 'shared.js'), 'export {};');
	};
	const files = (directory) => readdirSync(join(directory, '_app/immutable/chunks')).sort();
	return { app, record: join(root, 'record'), calls, exec, files };
}

test('every release keeps the chunks of all earlier releases', () => {
	const { app, record, calls, exec, files } = fakeApp();
	release({ app, record, previous: null, exec });
	assert.deepEqual(files(record), ['release-1.js', 'shared.js']);
	release({ app, record, exec });
	release({ app, record, exec, wranglerArgs: ['--profile', 'example'] });
	const output = join(app, '.svelte-kit/cloudflare');
	assert.deepEqual(files(output), ['release-1.js', 'release-2.js', 'release-3.js', 'shared.js']);
	assert.deepEqual(files(record), files(output));
	assert.deepEqual(calls.at(-1), {
		command: 'pnpm',
		args: ['exec', 'wrangler', 'deploy', '--config', 'wrangler.toml', '--profile', 'example'],
		target: undefined,
	});
});

test('a dry run retains earlier chunks but does not move the record', () => {
	const { app, record, exec, files } = fakeApp();
	release({ app, record, previous: null, exec });
	release({ app, record, exec, wranglerArgs: ['--dry-run'] });
	assert.deepEqual(files(join(app, '.svelte-kit/cloudflare')), [
		'release-1.js',
		'release-2.js',
		'shared.js',
	]);
	assert.deepEqual(files(record), ['release-1.js', 'shared.js']);
});

test('a release without an earlier one is refused before building', () => {
	const { app, record, calls, exec } = fakeApp();
	assert.throws(() => release({ app, record, exec }), /No earlier release/);
	assert.deepEqual(calls, []);
});

test('the web build ignores a stray KOMIKA_TARGET; the static build sets its own', () => {
	const stray = process.env.KOMIKA_TARGET;
	process.env.KOMIKA_TARGET = 'tauri';
	try {
		const web = fakeApp();
		release({ app: web.app, record: web.record, previous: null, exec: web.exec });
		assert.equal(web.calls[0].target, undefined);
		const flat = fakeApp();
		release({
			app: flat.app,
			record: flat.record,
			target: 'static',
			previous: null,
			exec: flat.exec,
		});
		assert.equal(flat.calls[0].target, 'static');
		assert.deepEqual(flat.calls[1].args.slice(3, 5), ['--config', 'wrangler.static.toml']);
	} finally {
		if (stray === undefined) delete process.env.KOMIKA_TARGET;
		else process.env.KOMIKA_TARGET = stray;
	}
});
