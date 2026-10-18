import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, renameSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { retainImmutableAssets } from './retain-assets.mjs';

// Release the reader: build, copy every earlier release's chunks into the output,
// deploy, then record the output as the new last release.
//
// `/_app/immutable/*` is kept a year by browsers (deploy/static-assets.mjs), and Workers
// Assets answers a missing chunk URL with the SPA shell, so a release must never drop a
// chunk that an open tab or a cached page may still import. The record of the last
// release lives in the git common directory, which the main checkout and every worktree
// share, so a release from any checkout keeps the chunks of the one before it.

const USAGE =
	'Usage: node apps/reader/deploy/release.mjs [--static] [--previous DIR | --no-previous] [-- WRANGLER_ARGS]';

const TARGETS = {
	web: { env: {}, output: '.svelte-kit/cloudflare', config: 'wrangler.toml' },
	static: { env: { KOMIKA_TARGET: 'static' }, output: 'build', config: 'wrangler.static.toml' },
};

/** @param {string} command @param {string[]} args @param {import('node:child_process').ExecFileSyncOptions} options */
function run(command, args, options) {
	execFileSync(command, args, { stdio: 'inherit', ...options });
}

/**
 * @param {{
 *   app: string,
 *   record: string,
 *   target?: 'web' | 'static',
 *   previous?: string | null,
 *   wranglerArgs?: string[],
 *   exec?: typeof run,
 * }} options `previous` defaults to `record`; `null` releases without earlier chunks.
 */
export function release({ app, record, target = 'web', previous, wranglerArgs = [], exec = run }) {
	const { env, output, config } = TARGETS[target];
	const source = previous === undefined ? record : previous;
	if (source !== null && !existsSync(join(source, '_app/immutable'))) {
		throw new Error(
			`No earlier release at ${source}. Pass --previous with an earlier release output, or --no-previous for a first release.`,
		);
	}
	const buildEnv = { ...process.env, ...env };
	if (!env.KOMIKA_TARGET) delete buildEnv.KOMIKA_TARGET;
	delete buildEnv.TAURI_ENV_PLATFORM;
	exec('pnpm', ['build'], { cwd: app, env: buildEnv });

	const current = join(app, output);
	if (source !== null) {
		console.log(`Retained ${retainImmutableAssets(source, current)} earlier immutable files`);
	}
	exec('pnpm', ['exec', 'wrangler', 'deploy', '--config', config, ...wranglerArgs], { cwd: app });
	if (wranglerArgs.includes('--dry-run')) return;

	// Swap the record in whole, so a failed copy leaves the old record in place.
	const next = `${record}.next`;
	rmSync(next, { recursive: true, force: true });
	cpSync(join(current, '_app/immutable'), join(next, '_app/immutable'), { recursive: true });
	rmSync(record, { recursive: true, force: true });
	renameSync(next, record);
	console.log(`Recorded this release's immutable files in ${record}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	const args = process.argv.slice(2);
	const split = args.indexOf('--');
	const own = split < 0 ? args : args.slice(0, split);
	/** @type {'web' | 'static'} */
	let target = 'web';
	/** @type {string | null | undefined} */
	let previous;
	for (let i = 0; i < own.length; i++) {
		if (own[i] === '--static') target = 'static';
		else if (own[i] === '--no-previous') previous = null;
		else if (own[i] === '--previous' && own[i + 1]) previous = resolve(own[++i]);
		else throw new Error(USAGE);
	}
	const app = fileURLToPath(new URL('..', import.meta.url));
	const common = execFileSync('git', ['rev-parse', '--path-format=absolute', '--git-common-dir'], {
		cwd: app,
		encoding: 'utf8',
	}).trim();
	release({
		app,
		record: join(common, 'komika-reader-release', target),
		target,
		previous,
		wranglerArgs: split < 0 ? [] : args.slice(split + 1),
	});
}
