import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { MAP_PATH, operationDocuments, persistedMapJson } from '../scripts/persisted-operations.ts';
import { normaliseDocument, persistedId } from '../src/persisted.ts';

test('normalising drops ignored tokens and keeps string literals byte for byte', () => {
	const doc = `
		# a comment with "quotes" and a # sign
		query Q($a: Int, $b: String = "x,  # not a comment") {
			field(a: $a, b: """ block,
			  # kept """) { id }
		}
	`;
	assert.equal(
		normaliseDocument(doc),
		'query Q($a: Int $b: String = "x,  # not a comment") { field(a: $a b: """ block,\n\t\t\t  # kept """) { id } }',
	);
	assert.equal(normaliseDocument('﻿  {a,b}  '), '{a b}');
	assert.equal(normaliseDocument('{ a(s: "esc \\" quote") }'), '{ a(s: "esc \\" quote") }');
});

test('an id is the SHA-256 of the normalised text, so layout does not change it', () => {
	const a = 'query Q {\n\tseries { id }\n}';
	const b = '  query Q { series { id } } # trailing';
	assert.equal(persistedId(a), persistedId(b));
	assert.equal(
		persistedId(a),
		createHash('sha256').update('query Q { series { id } }', 'utf8').digest('hex'),
	);
	assert.notEqual(persistedId(a), persistedId('query Q { series { id title } }'));
});

test('the build is deterministic and the committed map is current', () => {
	const first = persistedMapJson();
	assert.equal(persistedMapJson(), first, 'two builds give the same bytes');
	assert.equal(
		readFileSync(MAP_PATH, 'utf8'),
		first,
		'apps/server/persisted-operations.json is stale: run pnpm --filter @komika/api persisted',
	);
	const map = JSON.parse(first) as Record<string, string>;
	const ids = Object.keys(map);
	assert.deepEqual(ids, [...ids].sort(), 'ids are sorted');
	for (const [id, doc] of Object.entries(map)) {
		assert.match(id, /^[0-9a-f]{64}$/);
		assert.equal(normaliseDocument(doc), doc, 'stored text is normalised');
		assert.equal(createHash('sha256').update(doc, 'utf8').digest('hex'), id);
	}
});

test('every document of operations.ts is in the map, builder variants included', () => {
	const map = JSON.parse(persistedMapJson()) as Record<string, string>;
	const docs = operationDocuments();
	for (const doc of docs) assert.ok(map[persistedId(doc)], doc.slice(0, 80));
	assert.equal(Object.keys(map).length, new Set(docs.map(persistedId)).size);
	assert.ok(
		Object.values(map).some((d) => d.includes('appsFetchThemselves')) &&
			Object.values(map).some((d) => d.startsWith('query Instance') && !d.includes('appsFetch')),
		'both instanceQuery variants',
	);
});

test('a document builder without listed arguments fails the build', () => {
	assert.throws(
		() => operationDocuments({ NEW_QUERY: 'query N { a }', newBuilder: () => 'query B { b }' }),
		/newBuilder: list its arguments/,
	);
	assert.throws(() => operationDocuments({ NOT_A_DOCUMENT: 'just text' }), /holds no operation/);
});
