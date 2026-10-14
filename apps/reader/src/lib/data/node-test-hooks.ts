/**
 * Lets `node --test --experimental-strip-types` import modules written for SvelteKit
 * (`source.ts`, the shared `@komika/api` client) without a bundler. Import it FIRST, then
 * `await import()` the module under test, so the hooks are live before its graph resolves.
 *
 * Resolution hooks leave the source files unchanged:
 *   - Bundler-style specifiers Node cannot resolve on its own (`$lib/context`,
 *     `$app/navigation`, `@komika/api`, the runes-based `translator-pref.svelte`) are
 *     served as tiny stub modules. The data backend stub reads `globalThis.__komikaTest`,
 *     so a test swaps backend methods per case on one shared object.
 *   - Extensionless (`./load-fetch`) and `.js`-suffixed (`./operations.js`) relative
 *     imports, which Vite maps onto `.ts`, are mapped onto the `.ts` file that exists.
 *   - The shared viewer-cache rune module is compiled with the real Svelte compiler.
 *
 * Deliberately not named `*.test.ts`: the test glob would otherwise run it as a suite.
 */
import { existsSync, readFileSync } from 'node:fs';
import { registerHooks } from 'node:module';
import { fileURLToPath } from 'node:url';
import { compileModule } from 'svelte/compiler';
import ts from 'typescript';

export interface TestGlobals {
	backend: Record<string, unknown>;
	auth: { user: { id: string; username: string; isAdmin: boolean; showNsfw?: boolean } | null };
	config: {
		backendEnabled: boolean;
		backendKind: string;
		nativeEngine: boolean;
		apiEndpoint: string;
	};
	images: { resolvePage: (p: { sourceUrl?: string }) => Promise<string> };
	/** Every key passed to the stubbed `invalidate`, in order. */
	invalidated: string[];
	/** Stand-ins for `$app/state`'s `page` / `navigating` (only the fields source.ts reads). */
	page: { route: { id: string | null } };
	navigating: { to: { url: URL } | null; complete?: Promise<void> };
}

const g = globalThis as unknown as { __komikaTest?: TestGlobals };
g.__komikaTest ??= {
	backend: {},
	auth: { user: null },
	config: {
		backendEnabled: true,
		backendKind: 'komika',
		nativeEngine: false,
		apiEndpoint: 'https://comments.test/graphql',
	},
	images: { resolvePage: async (p) => p.sourceUrl ?? '' },
	invalidated: [],
	page: { route: { id: null } },
	navigating: { to: null },
};
export const testGlobals: TestGlobals = g.__komikaTest;

const STUBS: Record<string, string> = {
	'$lib/context':
		'export const backend = globalThis.__komikaTest.backend;' +
		'export const images = globalThis.__komikaTest.images;',
	'$app/navigation':
		'export const invalidate = async (k) => { globalThis.__komikaTest.invalidated.push(k); };' +
		'export const invalidateAll = async () => { globalThis.__komikaTest.invalidated.push("all"); };' +
		'export const goto = async () => {};',
	// The router state, as plain mutable objects a test points at a route / navigation.
	'$app/state':
		'export const page = globalThis.__komikaTest.page;' +
		'export const navigating = globalThis.__komikaTest.navigating;',
	'$lib/config':
		'export const config = globalThis.__komikaTest.config;' +
		'export const apiAssetSrc = (s) => s;',
	'$lib/auth.svelte': 'export const auth = globalThis.__komikaTest.auth;',
	'@komika/api': 'export const isTauri = () => false;',
	'./translator-pref.svelte':
		'export const getPreferredTranslator = () => undefined;' +
		'export const setPreferredTranslator = () => {};',
};

registerHooks({
	resolve(specifier, context, next) {
		if (specifier === '$lib/viewer-cache.svelte') {
			return {
				url: new URL('../viewer-cache.svelte.ts', import.meta.url).href,
				shortCircuit: true,
			};
		}
		if (Object.hasOwn(STUBS, specifier)) {
			return { url: `komika-stub:${encodeURIComponent(specifier)}`, shortCircuit: true };
		}
		const parent = context.parentURL;
		if (
			(specifier.startsWith('./') || specifier.startsWith('../')) &&
			parent?.startsWith('file:')
		) {
			const url = new URL(specifier, parent);
			const path = fileURLToPath(url);
			if (!existsSync(path)) {
				const ts = path.endsWith('.js') ? path.slice(0, -3) + '.ts' : path + '.ts';
				if (existsSync(ts)) {
					return { url: new URL(ts, 'file://').href, shortCircuit: true };
				}
			}
		}
		return next(specifier, context);
	},
	load(url, context, next) {
		if (url.endsWith('/viewer-cache.svelte.ts')) {
			return {
				format: 'module',
				source: compileModule(
					ts.transpileModule(readFileSync(fileURLToPath(url), 'utf8'), {
						compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
					}).outputText,
					{
						filename: fileURLToPath(url),
						generate: 'client',
					},
				).js.code,
				shortCircuit: true,
			};
		}
		if (url.startsWith('komika-stub:')) {
			const source = STUBS[decodeURIComponent(url.slice('komika-stub:'.length))];
			return { format: 'module', source, shortCircuit: true };
		}
		return next(url, context);
	},
});
