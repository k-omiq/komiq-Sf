/**
 * Persisted GraphQL documents: the ids a GET sends instead of the document text.
 *
 * An id is the SHA-256 (lowercase hex) of the document's normalised text. The build
 * (`packages/api/scripts/persisted-operations.ts`) hashes every document in
 * `operations.ts` the same way and writes the map the server loads
 * (`apps/server/persisted-operations.json`). The client computes the id of the
 * document it is about to send, so a document the server does not know (a newer
 * reader, or one the unknown-field fallback rewrote) is refused by id and sent as
 * text instead. The id can never point at a different document.
 */
import { sha256Hex } from './sha256.ts';

const isIgnored = (c: string) =>
	c === ' ' || c === '\t' || c === '\n' || c === '\r' || c === ',' || c === '﻿';

/** Where a string literal that opens at `start` ends (one past its closing quote). An
 *  unterminated literal runs to the end: the server reports the syntax error. */
function stringEnd(doc: string, start: number): number {
	if (doc.startsWith('"""', start)) {
		let i = start + 3;
		while (i < doc.length) {
			if (doc.startsWith('\\"""', i)) i += 4;
			else if (doc.startsWith('"""', i)) return i + 3;
			else i += 1;
		}
		return doc.length;
	}
	let i = start + 1;
	while (i < doc.length) {
		const c = doc[i];
		if (c === '\\') i += 2;
		else if (c === '"') return i + 1;
		else if (c === '\n' || c === '\r') return i;
		else i += 1;
	}
	return doc.length;
}

/**
 * The document with its ignored tokens (white space, line ends, commas and comments)
 * reduced to single spaces between tokens. String literals are kept byte for byte.
 * GraphQL gives ignored tokens no meaning, so the result is the same operation.
 */
export function normaliseDocument(doc: string): string {
	let out = '';
	let gap = false;
	let i = 0;
	while (i < doc.length) {
		const c = doc[i];
		if (isIgnored(c)) {
			gap = true;
			i += 1;
			continue;
		}
		if (c === '#') {
			while (i < doc.length && doc[i] !== '\n' && doc[i] !== '\r') i += 1;
			gap = true;
			continue;
		}
		if (gap && out) out += ' ';
		gap = false;
		if (c === '"') {
			const end = stringEnd(doc, i);
			out += doc.slice(i, end);
			i = end;
			continue;
		}
		out += c;
		i += 1;
	}
	return out;
}

const ids = new Map<string, string>();
/** Distinct documents are a fixed set plus a few rewritten ones; the cap only guards. */
const ID_CAPACITY = 512;

/** The persisted id of `doc`. Memoized: a document is hashed once per process. */
export function persistedId(doc: string): string {
	let id = ids.get(doc);
	if (id === undefined) {
		id = sha256Hex(normaliseDocument(doc));
		if (ids.size >= ID_CAPACITY) ids.clear();
		ids.set(doc, id);
	}
	return id;
}
