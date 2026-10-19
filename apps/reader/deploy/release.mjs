import { execFileSync } from 'node:child_process';
import {
	cpSync,
	existsSync,
	readFileSync,
	readdirSync,
	renameSync,
	rmSync,
	writeFileSync,
} from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { retainImmutableAssets } from './retain-assets.mjs';

// Release the reader: build, copy the earlier releases' recent chunks into the output,
// deploy, then record the output as the new last release.
//
// `/_app/immutable/*` is kept 7 days by browsers (deploy/static-assets.mjs), and Workers
// Assets answers a missing chunk URL with the SPA shell, so a release must never drop a
// chunk that an open tab or a cached page may still import. The record of the last
// release lives in the git common directory, which the main checkout and every worktree
// share, so a release from any checkout keeps the chunks of the one before it.
//
// A chunk is kept for RETAIN_MS after the last release that built it, then dropped:
// otherwise every release adds its chunks for good, and the Worker's asset file limit is
// reached after a few hundred releases. `retained.json` in the record holds, per file,
// when a build last produced it. A tab older than that reloads on a failed import.

/** How long a chunk is kept after the last release that built it. */
export const RETAIN_MS = 30 * 24 * 60 * 60 * 1000;
const STAMPS = 'retained.json';

/** Every file under `directory`, relative and with `/` separators. @param {string} directory */
function listFiles(directory, prefix = '') {
	/** @type {string[]} */
	const files = [];
	for (const entry of readdirSync(directory, { withFileTypes: true })) {
		const relative = `${prefix}${entry.name}`;
		if (entry.isDirectory()) files.push(...listFiles(join(directory, entry.name), `${relative}/`));
		else if (entry.isFile()) files.push(relative);
	}
	return files;
}

/** The stamps of an earlier release; a file without one counts as built `now`. */
function readStamps(/** @type {string} */ source, /** @type {number} */ now) {
	/** @type {Record<string, number>} */
	let stamps = {};
	try {
		stamps = JSON.parse(readFileSync(join(source, STAMPS), 'utf8'));
	} catch {
		// No stamps yet (the first release with them, or a --previous output).
	}
	return (/** @type {string} */ file) => stamps[file] ?? now;
}

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
 *   now?: number,
 * }} options `previous` defaults to `record`; `null` releases without earlier chunks.
 */
export function release({
	app,
	record,
	target = 'web',
	previous,
	wranglerArgs = [],
	exec = run,
	now = Date.now(),
}) {
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
	const immutable = join(current, '_app/immutable');
	/** @type {Record<string, number>} */
	const stamps = {};
	for (const file of listFiles(immutable)) stamps[file] = now;
	if (source !== null) {
		const builtAt = readStamps(source, now);
		const kept = retainImmutableAssets(source, current, (file) => {
			if (file in stamps) return true;
			if (now - builtAt(file) > RETAIN_MS) return false;
			stamps[file] = builtAt(file);
			return true;
		});
		console.log(`Retained ${kept} earlier immutable files`);
	}
	exec('pnpm', ['exec', 'wrangler', 'deploy', '--config', config, ...wranglerArgs], { cwd: app });
	if (wranglerArgs.includes('--dry-run')) return;

	// Swap the record in whole, so a failed copy leaves the old record in place.
	const next = `${record}.next`;
	rmSync(next, { recursive: true, force: true });
	cpSync(immutable, join(next, '_app/immutable'), { recursive: true });
	writeFileSync(join(next, STAMPS), JSON.stringify(stamps));
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
