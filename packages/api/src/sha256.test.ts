/**
 * Run: node --test packages/api/src/sha256.test.ts
 *
 * The tag id of a non-ASCII tag name is derived from this digest on BOTH sides (the
 * server's `tags::slug` uses the `sha2` crate), and so is a persisted
 * document id, so it must be bit-exact SHA-256.
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { test } from 'node:test';

import { sha256Hex } from './sha256.ts';

test('matches the FIPS 180-4 test vectors', () => {
	assert.equal(sha256Hex(''), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
	assert.equal(
		sha256Hex('abc'),
		'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad',
	);
	assert.equal(
		sha256Hex('abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq'),
		'248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1',
	);
});

test('agrees with node:crypto on UTF-8 input across block boundaries', () => {
	for (const s of ['恋愛', '百合', 'Pokémon', 'x'.repeat(55), 'x'.repeat(56), 'é'.repeat(64)]) {
		assert.equal(sha256Hex(s), createHash('sha256').update(s, 'utf8').digest('hex'), s);
	}
});
