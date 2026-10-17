import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { test } from 'node:test';
import { MAX_PACK_BYTES, packFits, verifyPackStream } from './pack.ts';

const bytes = (s: string) => new TextEncoder().encode(s);
const object = (s: string) => ({
	sha256: createHash('sha256').update(s).digest('hex'),
	length: bytes(s).length,
	mime: 'image/png',
});
const expected = {
	copyId: 'mc_copy',
	revision: 2,
	pages: [object('first'), object('last'), object('first')],
};
const index = () => ({
	v: 1,
	copy_id: expected.copyId,
	revision: expected.revision,
	pages: expected.pages.map((p, i) => ({ ...p, offset: i === 1 ? 5 : 0 })),
});
function header(value: unknown): Uint8Array {
	const json = bytes(JSON.stringify(value));
	const out = new Uint8Array(8 + json.length);
	out.set(bytes('KPK1'));
	new DataView(out.buffer).setUint32(4, json.length);
	out.set(json, 8);
	return out;
}
const pack = (value: unknown = index(), data = 'firstlast') =>
	new Uint8Array([...header(value), ...bytes(data)]);
function stream(data: Uint8Array, chunk = 1): ReadableStream<Uint8Array> {
	let at = 0;
	return new ReadableStream({
		pull(controller) {
			if (at === data.length) return controller.close();
			controller.enqueue(data.slice(at, at + chunk));
			at = Math.min(at + chunk, data.length);
		},
	});
}

test('good pack: byte-at-a-time header/data, duplicates share the first range', async () => {
	const received: [number, Blob][] = [];
	await verifyPackStream(stream(pack()), expected, {
		onPage: (i, b) => {
			received.push([i, b]);
		},
	});
	assert.deepEqual(
		received.map(([i]) => i),
		[0, 2, 1],
	);
	assert.deepEqual(await Promise.all(received.map(async ([i, b]) => [i, await b.text(), b.type])), [
		[0, 'first', 'image/png'],
		[2, 'first', 'image/png'],
		[1, 'last', 'image/png'],
	]);
	assert.equal(received[0][1], received[1][1]);
});

test('completed ranges are published before the next range or EOF arrives', async () => {
	let controller!: ReadableStreamDefaultController<Uint8Array>;
	let first!: () => void;
	const ready = new Promise<void>((resolve) => {
		first = resolve;
	});
	const input = new ReadableStream<Uint8Array>({
		start(c) {
			controller = c;
		},
	});
	const seen: number[] = [];
	const done = verifyPackStream(input, expected, {
		onPage: (i) => {
			seen.push(i);
			first();
		},
	});
	controller.enqueue(new Uint8Array([...header(index()), ...bytes('first')]));
	await ready;
	assert.deepEqual(seen, [0, 2]);
	controller.enqueue(bytes('last'));
	controller.close();
	await done;
	assert.deepEqual(seen, [0, 2, 1]);
});

test('hostile headers: short prefix/index, wrong magic, huge index, malformed JSON', async () => {
	for (const data of [
		new Uint8Array(),
		bytes('KPK1'),
		bytes('NOPE0000'),
		new Uint8Array([...bytes('KPK1'), 255, 255, 255, 255]),
		header(index()).slice(0, -1),
		new Uint8Array([...bytes('KPK1'), 0, 0, 0, 1, 255]),
	]) {
		await assert.rejects(
			verifyPackStream(stream(data), expected, { onPage: () => assert.fail('untrusted page') }),
		);
	}
});

test('unsigned index must match identity, every trusted field, and canonical bounded offsets', async () => {
	for (const change of [
		(i: ReturnType<typeof index>) => {
			i.v = 2;
		},
		(i: ReturnType<typeof index>) => {
			i.copy_id = 'mc_other';
		},
		(i: ReturnType<typeof index>) => {
			i.revision++;
		},
		(i: ReturnType<typeof index>) => {
			i.pages.pop();
		},
		(i: ReturnType<typeof index>) => {
			i.pages[0].sha256 = 'a'.repeat(64);
		},
		(i: ReturnType<typeof index>) => {
			i.pages[0].length++;
		},
		(i: ReturnType<typeof index>) => {
			i.pages[0].mime = 'image/jpeg';
		},
		...[-1, 0.5, 999, Number.MAX_SAFE_INTEGER, Number.MAX_SAFE_INTEGER + 1].map(
			(offset) => (i: ReturnType<typeof index>) => {
				i.pages[1].offset = offset;
			},
		),
		(i: ReturnType<typeof index>) => {
			i.pages[2].offset = 9;
		},
	]) {
		const value = index();
		change(value);
		await assert.rejects(
			verifyPackStream(stream(pack(value)), expected, {
				onPage: () => assert.fail('untrusted page'),
			}),
		);
	}
	assert.equal(packFits([{ ...object('x'), length: Number.MAX_SAFE_INTEGER + 1 }]), false);
	assert.equal(packFits([{ ...object('x'), length: MAX_PACK_BYTES + 1 }]), false);
});

test('sha mismatch and truncated stream retain only earlier verified ranges', async () => {
	for (const data of ['firstlost', 'firstla']) {
		const seen: number[] = [];
		await assert.rejects(
			verifyPackStream(stream(pack(index(), data)), expected, {
				onPage: (i) => {
					seen.push(i);
				},
			}),
			/hash|short body/,
		);
		assert.deepEqual(seen, [0, 2]);
	}
});

test('network error after a verified range preserves it and cancels the reader', async () => {
	let controller!: ReadableStreamDefaultController<Uint8Array>;
	const seen: number[] = [];
	const input = new ReadableStream<Uint8Array>({
		start(c) {
			controller = c;
		},
	});
	const done = verifyPackStream(input, expected, {
		onPage: (i) => {
			seen.push(i);
			if (i === 2) controller.error(new Error('producer failed'));
		},
	});
	controller.enqueue(new Uint8Array([...header(index()), ...bytes('first')]));
	await assert.rejects(done, /producer failed/);
	assert.deepEqual(seen, [0, 2]);
	assert.equal(input.locked, false);
});

test('abort mid-stream cancels a stalled read and does not publish unverified bytes', async () => {
	const controller = new AbortController();
	let cancelled = false;
	const input = new ReadableStream<Uint8Array>({
		start(c) {
			c.enqueue(new Uint8Array([...header(index()), ...bytes('fir')]));
		},
		cancel() {
			cancelled = true;
		},
	});
	const done = verifyPackStream(input, expected, {
		signal: controller.signal,
		onPage: () => assert.fail('aborted page'),
	});
	await new Promise((resolve) => setImmediate(resolve));
	controller.abort();
	await assert.rejects(done, { name: 'AbortError' });
	assert.equal(cancelled, true);
	assert.equal(input.locked, false);
});

test('pre-aborted streams and trailing bytes fail', async () => {
	const abort = new AbortController();
	abort.abort();
	await assert.rejects(
		verifyPackStream(stream(pack()), expected, {
			signal: abort.signal,
			onPage: () => assert.fail('aborted'),
		}),
		{ name: 'AbortError' },
	);
	await assert.rejects(
		verifyPackStream(stream(pack(index(), 'firstlastx')), expected, { onPage: () => {} }),
		/trailing data/,
	);
});

test('onProgress fires once per non-empty chunk read', async () => {
	const data = pack();
	let progress = 0;
	await verifyPackStream(stream(data, 7), expected, {
		onPage: () => {},
		onProgress: () => {
			progress++;
		},
	});
	assert.equal(progress, Math.ceil(data.length / 7));
});
