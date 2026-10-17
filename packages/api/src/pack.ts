import type { ExpectedObject } from './media-routes.js';

export const MAX_PACK_INDEX_BYTES = 1024 * 1024;
/** Experimental prefetch holds at most this much unique page data per chapter. */
export const MAX_PACK_BYTES = 32 * 1024 * 1024;

/** Trusted coordinates and metadata, derived from the owner's pages answer. */
export interface PackPage {
	url: string;
	copyId: string;
	revision: number;
	expect: ExpectedObject;
}

export interface ExpectedPack {
	copyId: string;
	revision: number;
	pages: readonly ExpectedObject[];
}

export class PackError extends Error {
	constructor(reason: string) {
		super(`Media pack check failed: ${reason}`);
		this.name = 'PackError';
	}
}

/** Canonical ranges come from trusted metadata, never from the unsigned index. */
function ranges(pages: readonly ExpectedObject[], maxBytes: number) {
	const unique = new Map<string, { expect: ExpectedObject; offset: number; indices: number[] }>();
	let length = 0;
	const ordered = pages.map((expect, i) => {
		if (
			!/^[0-9a-f]{64}$/.test(expect.sha256) ||
			!Number.isSafeInteger(expect.length) ||
			expect.length <= 0 ||
			!/^image\/[a-z0-9!#$%&'*+.^_`|~-]+$/.test(expect.mime)
		)
			throw new PackError('metadata');
		const first = unique.get(expect.sha256);
		if (first) {
			if (first.expect.length !== expect.length || first.expect.mime !== expect.mime) {
				throw new PackError('duplicate metadata');
			}
			first.indices.push(i);
			return first;
		}
		const range = { expect, offset: length, indices: [i] };
		length += expect.length;
		if (!Number.isSafeInteger(length) || length > maxBytes) throw new PackError('size limit');
		unique.set(expect.sha256, range);
		return range;
	});
	return { ordered, unique: [...unique.values()] };
}

/** Preflight the memory bound before making any request. */
export function packFits(pages: readonly ExpectedObject[], maxBytes = MAX_PACK_BYTES): boolean {
	try {
		ranges(pages, maxBytes);
		return pages.length > 0;
	} catch {
		return false;
	}
}

/**
 * Incrementally parse KPK1 and publish each range immediately after length/hash checks.
 * A later failure never retracts verified pages. The index is unsigned: every field and
 * canonical offset must agree with the trusted pages answer before any range is used.
 */
export async function verifyPackStream(
	body: ReadableStream<Uint8Array>,
	expected: ExpectedPack,
	opts: {
		signal?: AbortSignal;
		subtle?: SubtleCrypto;
		maxBytes?: number;
		onPage: (index: number, blob: Blob) => void;
		/** Called after every non-empty chunk read from `body` (an idle-timeout heartbeat). */
		onProgress?: () => void;
	},
): Promise<void> {
	const { ordered, unique } = ranges(expected.pages, opts.maxBytes ?? MAX_PACK_BYTES);
	const reader = body.getReader();
	let chunk: Uint8Array = new Uint8Array(0);
	let at = 0;
	let complete = false;
	const checkAbort = () => opts.signal?.throwIfAborted();
	const abort = () => {
		void reader.cancel(opts.signal?.reason).catch(() => {});
	};
	opts.signal?.addEventListener('abort', abort, { once: true });
	async function next(): Promise<boolean> {
		while (at === chunk.length) {
			checkAbort();
			const read = await reader.read();
			checkAbort();
			if (read.done) return false;
			chunk = read.value;
			if (chunk.length) opts.onProgress?.();
			at = 0;
		}
		return true;
	}
	async function exact(length: number): Promise<Uint8Array<ArrayBuffer>> {
		const bytes = new Uint8Array(length);
		let filled = 0;
		while (filled < length) {
			if (!(await next())) throw new PackError('short body');
			const take = Math.min(length - filled, chunk.length - at);
			bytes.set(chunk.subarray(at, at + take), filled);
			at += take;
			filled += take;
		}
		return bytes;
	}
	try {
		checkAbort();
		const prefix = await exact(8);
		if (new TextDecoder().decode(prefix.subarray(0, 4)) !== 'KPK1') throw new PackError('magic');
		const indexLength = new DataView(prefix.buffer).getUint32(4);
		if (indexLength > MAX_PACK_INDEX_BYTES) throw new PackError('index limit');
		const raw = await exact(indexLength);
		const index = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(raw));
		if (
			index?.v !== 1 ||
			index.copy_id !== expected.copyId ||
			index.revision !== expected.revision ||
			!Number.isSafeInteger(index.revision) ||
			!Array.isArray(index.pages) ||
			index.pages.length !== ordered.length
		)
			throw new PackError('identity');
		index.pages.forEach((entry: Record<string, unknown> | null, i: number) => {
			const { expect, offset } = ordered[i];
			if (
				!entry ||
				entry.sha256 !== expect.sha256 ||
				entry.length !== expect.length ||
				entry.mime !== expect.mime ||
				!Number.isSafeInteger(entry.offset) ||
				entry.offset !== offset ||
				!Number.isSafeInteger(offset + expect.length)
			)
				throw new PackError('range metadata');
		});
		const subtle = opts.subtle ?? globalThis.crypto?.subtle;
		if (!subtle) throw new PackError('crypto');
		for (const { expect, indices } of unique) {
			const bytes = await exact(expect.length);
			const digest = await subtle.digest('SHA-256', bytes);
			checkAbort();
			const sha = Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join(
				'',
			);
			if (sha !== expect.sha256) throw new PackError('hash');
			const blob = new Blob([bytes], { type: expect.mime });
			for (const i of indices) {
				checkAbort();
				opts.onPage(i, blob);
			}
		}
		if (await next()) throw new PackError('trailing data');
		complete = true;
	} finally {
		opts.signal?.removeEventListener('abort', abort);
		if (!complete) void reader.cancel().catch(() => {});
		reader.releaseLock();
	}
}
