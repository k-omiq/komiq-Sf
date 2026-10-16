import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { retainImmutableAssets } from './retain-assets.mjs';

test('retains old chunks without replacing current files or copying old HTML', () => {
	const directory = mkdtempSync(join(tmpdir(), 'komiq-retain-'));
	try {
		const previous = join(directory, 'previous');
		const current = join(directory, 'current');
		for (const build of [previous, current])
			mkdirSync(join(build, '_app/immutable/chunks'), { recursive: true });
		writeFileSync(join(previous, '_app/immutable/chunks/old.js'), 'old chunk');
		writeFileSync(join(previous, '_app/immutable/chunks/shared.js'), 'same chunk');
		writeFileSync(join(current, '_app/immutable/chunks/shared.js'), 'same chunk');
		writeFileSync(join(previous, 'index.html'), 'old HTML');
		writeFileSync(join(current, 'index.html'), 'current HTML');
		assert.equal(retainImmutableAssets(previous, current), 1);
		assert.equal(retainImmutableAssets(previous, current), 0);
		assert.equal(readFileSync(join(current, '_app/immutable/chunks/old.js'), 'utf8'), 'old chunk');
		assert.equal(readFileSync(join(current, 'index.html'), 'utf8'), 'current HTML');
		writeFileSync(join(previous, '_app/immutable/chunks/shared.js'), 'different bytes');
		assert.throws(() => retainImmutableAssets(previous, current), /Immutable asset collision/);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});
