/**
 * Builds the persisted-document map the server loads: every document in
 * `src/operations.ts`, keyed by its persisted id (see `src/persisted.ts`).
 *
 *   node scripts/persisted-operations.ts          write apps/server/persisted-operations.json
 *   node scripts/persisted-operations.ts --check  fail when the file is not current
 *
 * The output depends only on `operations.ts`: ids are sorted and the JSON layout is
 * fixed, so two runs write the same bytes.
 */
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import * as operations from '../src/operations.ts';
import { normaliseDocument, persistedId } from '../src/persisted.ts';

export const MAP_PATH = fileURLToPath(
	new URL('../../../apps/server/persisted-operations.json', import.meta.url),
);

/** The argument lists each exported document BUILDER is called with. A new builder
 *  must be listed here, or the build fails rather than leave its documents out. */
const BUILDER_ARGUMENTS: Record<string, readonly (readonly unknown[])[]> = {
	instanceQuery: [[true], [false]],
	checkCloudflareTokenFor: [['TUNNEL'], ['PUBLIC_IP']],
};

const OPERATION = /(^|[\s}])(query|mutation|subscription)\b/;

/** Every document `operations.ts` can produce, in export order. */
export function operationDocuments(module: Record<string, unknown> = operations): string[] {
	const docs: string[] = [];
	for (const [name, value] of Object.entries(module)) {
		if (typeof value === 'string') {
			if (!OPERATION.test(normaliseDocument(value))) {
				throw new Error(`${name} is exported from operations.ts but holds no operation`);
			}
			docs.push(value);
		} else if (typeof value === 'function') {
			const calls = BUILDER_ARGUMENTS[name];
			if (!calls) throw new Error(`${name}: list its arguments in BUILDER_ARGUMENTS`);
			for (const args of calls) docs.push((value as (...a: unknown[]) => string)(...args));
		}
	}
	return docs;
}

/** The map file's exact text: `{ id: normalised document }`, ids in ascending order. */
export function persistedMapJson(docs: readonly string[] = operationDocuments()): string {
	const map = new Map<string, string>();
	for (const doc of docs) map.set(persistedId(doc), normaliseDocument(doc));
	const sorted = Object.fromEntries([...map].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
	return JSON.stringify(sorted, null, '\t') + '\n';
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
	const text = persistedMapJson();
	if (process.argv.includes('--check')) {
		let current = '';
		try {
			current = readFileSync(MAP_PATH, 'utf8');
		} catch {
			/* missing: reported below */
		}
		if (current !== text) {
			console.error(`${MAP_PATH} is not current. Run: pnpm --filter @komika/api persisted`);
			process.exit(1);
		}
	} else {
		writeFileSync(MAP_PATH, text);
		console.log(`wrote ${Object.keys(JSON.parse(text)).length} documents to ${MAP_PATH}`);
	}
}
