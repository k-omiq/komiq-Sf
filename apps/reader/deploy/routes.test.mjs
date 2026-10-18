import assert from 'node:assert/strict';
import { existsSync, readFileSync, realpathSync, statSync } from 'node:fs';
import { createRequire, registerHooks } from 'node:module';
import { pathToFileURL, fileURLToPath } from 'node:url';
import test from 'node:test';
import { unstable_readConfig } from 'wrangler';

const config = unstable_readConfig({
	config: fileURLToPath(new URL('../wrangler.toml', import.meta.url)),
});
const staticConfig = unstable_readConfig({
	config: fileURLToPath(new URL('../wrangler.static.toml', import.meta.url)),
});

test('Workers config selects nonempty series and static config contains no script', () => {
	assert.deepEqual(config.assets.run_worker_first, ['/series/*', '!/series/']);
	assert.equal(config.assets.not_found_handling, 'single-page-application');
	assert.equal(
		config.main,
		fileURLToPath(new URL('../.svelte-kit/cloudflare/_worker.js', import.meta.url)),
	);
	assert.equal(
		existsSync(new URL('../.svelte-kit/cloudflare/_routes.json', import.meta.url)),
		false,
	);
	assert.equal(staticConfig.main, undefined);
	assert.equal(staticConfig.assets.binding, undefined);
	assert.equal(staticConfig.assets.run_worker_first, undefined);
	assert.equal(staticConfig.assets.not_found_handling, 'single-page-application');
	assert.equal(config.observability?.enabled, true);
});

test('static deployment is isolated from production and has no public routes', () => {
	assert.equal(staticConfig.name, 'komika-reader-static');
	assert.notEqual(staticConfig.name, config.name);
	assert.equal(staticConfig.workers_dev, false);
	assert.equal(staticConfig.route, undefined);
	assert.deepEqual(staticConfig.routes ?? [], []);
});

registerHooks({
	resolve(specifier, context, next) {
		if (specifier === 'cloudflare:workers') {
			return {
				shortCircuit: true,
				url:
					'data:text/javascript,' +
					encodeURIComponent(
						'export class WorkerEntrypoint { constructor(ctx, env) { this.ctx = ctx; this.env = env; } }',
					),
			};
		}
		return next(specifier, context);
	},
});
const require = createRequire(
	realpathSync(
		fileURLToPath(new URL('../../../node_modules/wrangler/package.json', import.meta.url)),
	),
);
const bundled = new URL('./workers/assets/', pathToFileURL(require.resolve('miniflare')));
const { default: AssetWorker } = await import(new URL('assets.worker.js', bundled));
const { RouterInnerEntrypoint: Router } = await import(new URL('router.worker.js', bundled));
const { ASSETS_PLUGIN } = require('miniflare');

async function routerFor(settings) {
	// No workerd runtime or sockets: use the bundled configuration/header parsers
	// and routers, replacing only platform bindings and asset storage.
	const directory = pathToFileURL(`${settings.assets.directory}/`);
	const services = await ASSETS_PLUGIN.getServices({
		options: {
			config: {
				name: settings.name,
				compatibilityDate: settings.compatibility_date,
				compatibilityFlags: settings.compatibility_flags,
				assets: {
					directory: settings.assets.directory,
					hasUserWorker: Boolean(settings.main),
					runWorkerFirst: settings.assets.run_worker_first,
					notFoundHandling: settings.assets.not_found_handling,
				},
			},
		},
		log: {
			debug() {},
			info() {},
			warn: (message) => assert.fail(message),
			error: (message) => assert.fail(message),
		},
	});
	function serviceConfig(name) {
		return JSON.parse(
			services
				.find((service) => service.name === `${name}:${settings.name}`)
				.worker.bindings.find((binding) => binding.name === 'CONFIG').json,
		);
	}
	class DiskAssets extends AssetWorker {
		async unstable_exists(path) {
			try {
				return statSync(new URL(`.${path}`, directory)).isFile() ? path : null;
			} catch {
				return null;
			}
		}
		async unstable_getByETag(path) {
			return {
				readableStream: new Response(readFileSync(new URL(`.${path}`, directory))).body,
				contentType: path.endsWith('.js') ? 'text/javascript' : 'text/html',
				cacheStatus: 'HIT',
			};
		}
	}
	const asset = new DiskAssets({}, { CONFIG: serviceConfig('assets:assets-service') });
	let invocations = 0;
	const router = new Router(
		{},
		{
			CONFIG: serviceConfig('assets:router'),
			ASSET_WORKER: asset,
			USER_WORKER: {
				fetch: () => {
					invocations++;
					return new Response('series SSR');
				},
			},
		},
	);
	return {
		shell: readFileSync(new URL('index.html', directory), 'utf8'),
		fetch: (path, headers = {}) =>
			router.fetch(new Request(`http://localhost${path}`, { headers })),
		get invocations() {
			return invocations;
		},
	};
}

test('bundled router excludes slugless series while keeping series SSR', async () => {
	const router = await routerFor(config);
	for (const path of [
		'/',
		'/browse?sort=updated',
		'/read/chapter',
		'/library',
		'/unknown/path',
		'/series',
		'/series/',
		'/series/?share=1',
		'/series-other/test',
	]) {
		for (const headers of [{}, { 'Sec-Fetch-Mode': 'navigate' }]) {
			const response = await router.fetch(path, headers);
			assert.equal(response.status, 200, path);
			assert.equal(await response.text(), router.shell, path);
			assert.equal(router.invocations, 0, path);
		}
	}
	for (const path of [
		'/series/example',
		'/series/example?share=1',
		'/series/example/__data.json',
	]) {
		assert.equal(await (await router.fetch(path)).text(), 'series SSR');
	}
	assert.equal(router.invocations, 3);
});

for (const settings of [config, staticConfig]) {
	test(`${settings.name}: chunks are kept a year and never invoke a User Worker`, async () => {
		const router = await routerFor(settings);
		for (const path of ['/_app/missing.js', '/_app/immutable/missing.js']) {
			for (const mode of [undefined, 'navigate', 'cors', 'same-origin']) {
				const response = await router.fetch(path, mode ? { 'Sec-Fetch-Mode': mode } : {});
				// The bundled worker's fetch path applies SPA fallback for every mode, and
				// _headers matches the requested URL, so a missing chunk would get the shell
				// under the year-long chunk header. That is why deploy/release.mjs never drops
				// a chunk: every release carries all earlier releases' chunks.
				assert.equal(response.status, 200);
				assert.equal(await response.text(), router.shell);
				assert.equal(
					response.headers.get('Cache-Control'),
					path.startsWith('/_app/immutable/') ? 'public, max-age=31536000, immutable' : 'no-cache',
				);
				assert.equal(response.headers.get('X-Robots-Tag'), 'noindex');
				assert.equal(router.invocations, 0);
			}
		}
		const entry = router.shell.match(/import\("([^\"]+\/entry\/start\.[^\"]+)"\)/)[1];
		const existing = await router.fetch(entry, { 'Sec-Fetch-Mode': 'cors' });
		assert.equal(existing.status, 200);
		assert.equal(existing.headers.get('Content-Type'), 'text/javascript');
		assert.equal(existing.headers.get('Cache-Control'), 'public, max-age=31536000, immutable');
		assert.notEqual(await existing.text(), router.shell);
		assert.equal(router.invocations, 0);
	});
}
