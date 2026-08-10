/**
 * QR codes for short text such as a reader address, without a dependency.
 *
 * Byte mode, error correction level M, versions 1 to 10 (up to 213 bytes, far more
 * than an address needs), with the mask chosen by the standard penalty score. The
 * structure follows ISO/IEC 18004 as laid out in Project Nayuki's reference encoder.
 */

/** Error-correction codewords per block and block count, level M, versions 1..10. */
const ECC_PER_BLOCK = [10, 16, 26, 18, 24, 16, 18, 22, 22, 26];
const BLOCKS = [1, 1, 1, 2, 2, 4, 4, 4, 5, 5];
const MAX_VERSION = 10;
/** Format bits of level M. */
const LEVEL_M = 0;

export interface QrMatrix {
	size: number;
	/** `dark[y][x]` */
	dark: boolean[][];
}

/** Encode `text` (UTF-8). Throws when it does not fit version 10. */
export function encodeQr(text: string): QrMatrix {
	const bytes = Array.from(new TextEncoder().encode(text));
	let version = 1;
	for (; version <= MAX_VERSION; version++) {
		if (headerBits(version) + bytes.length * 8 <= dataCodewords(version) * 8) break;
	}
	if (version > MAX_VERSION) throw new Error('Text too long for a QR code');

	// Data bits: mode, length, payload, terminator, byte padding, pad codewords.
	const bits: number[] = [];
	const put = (value: number, length: number) => {
		for (let i = length - 1; i >= 0; i--) bits.push((value >>> i) & 1);
	};
	put(0b0100, 4);
	put(bytes.length, version < 10 ? 8 : 16);
	for (const b of bytes) put(b, 8);
	const capacity = dataCodewords(version) * 8;
	put(0, Math.min(4, capacity - bits.length));
	put(0, (8 - (bits.length % 8)) % 8);
	for (let pad = 0xec; bits.length < capacity; pad ^= 0xec ^ 0x11) put(pad, 8);
	const data: number[] = [];
	for (let i = 0; i < bits.length; i += 8) {
		data.push(bits.slice(i, i + 8).reduce((acc, bit) => (acc << 1) | bit, 0));
	}

	const qr = new Grid(version);
	qr.drawFunctionPatterns();
	qr.drawCodewords(withErrorCorrection(version, data));

	// The mask with the lowest penalty wins.
	let best = 0;
	let bestScore = Infinity;
	for (let mask = 0; mask < 8; mask++) {
		qr.applyMask(mask);
		qr.drawFormatBits(mask);
		const score = qr.penalty();
		if (score < bestScore) {
			best = mask;
			bestScore = score;
		}
		qr.applyMask(mask); // XOR again: undo
	}
	qr.applyMask(best);
	qr.drawFormatBits(best);
	return { size: qr.size, dark: qr.modules };
}

/** An SVG path (one unit per module) drawing the dark modules, offset by `margin`. */
export function qrPath(matrix: QrMatrix, margin = 4): string {
	let d = '';
	matrix.dark.forEach((row, y) => {
		row.forEach((on, x) => {
			if (on) d += `M${x + margin} ${y + margin}h1v1h-1z`;
		});
	});
	return d;
}

function headerBits(version: number): number {
	return 4 + (version < 10 ? 8 : 16);
}

function rawDataModules(version: number): number {
	let result = (16 * version + 128) * version + 64;
	if (version >= 2) {
		const align = Math.floor(version / 7) + 2;
		result -= (25 * align - 10) * align - 55;
		if (version >= 7) result -= 36;
	}
	return result;
}

function dataCodewords(version: number): number {
	return Math.floor(rawDataModules(version) / 8) - ECC_PER_BLOCK[version - 1] * BLOCKS[version - 1];
}

// ---- Reed-Solomon over GF(256), polynomial 0x11D ------------------------------------

function gfMultiply(x: number, y: number): number {
	let z = 0;
	for (let i = 7; i >= 0; i--) {
		z = (z << 1) ^ ((z >>> 7) * 0x11d);
		z ^= ((y >>> i) & 1) * x;
	}
	return z;
}

function rsDivisor(degree: number): number[] {
	const result: number[] = new Array(degree).fill(0);
	result[degree - 1] = 1;
	let root = 1;
	for (let i = 0; i < degree; i++) {
		for (let j = 0; j < result.length; j++) {
			result[j] = gfMultiply(result[j], root);
			if (j + 1 < result.length) result[j] ^= result[j + 1];
		}
		root = gfMultiply(root, 0x02);
	}
	return result;
}

function rsRemainder(data: number[], divisor: number[]): number[] {
	const result: number[] = divisor.map(() => 0);
	for (const b of data) {
		const factor = b ^ (result.shift() as number);
		result.push(0);
		divisor.forEach((coef, i) => (result[i] ^= gfMultiply(coef, factor)));
	}
	return result;
}

/** Split into blocks, add each block's ECC, interleave. */
function withErrorCorrection(version: number, data: number[]): number[] {
	const numBlocks = BLOCKS[version - 1];
	const eccLen = ECC_PER_BLOCK[version - 1];
	const rawCodewords = Math.floor(rawDataModules(version) / 8);
	const numShort = numBlocks - (rawCodewords % numBlocks);
	const shortLen = Math.floor(rawCodewords / numBlocks);
	const divisor = rsDivisor(eccLen);
	const blocks: number[][] = [];
	for (let i = 0, k = 0; i < numBlocks; i++) {
		const dat = data.slice(k, k + shortLen - eccLen + (i < numShort ? 0 : 1));
		k += dat.length;
		const ecc = rsRemainder(dat, divisor);
		if (i < numShort) dat.push(0);
		blocks.push(dat.concat(ecc));
	}
	const result: number[] = [];
	for (let i = 0; i < blocks[0].length; i++) {
		blocks.forEach((block, j) => {
			if (i !== shortLen - eccLen || j >= numShort) result.push(block[i]);
		});
	}
	return result;
}

// ---- the module grid ---------------------------------------------------------------

class Grid {
	readonly size: number;
	readonly modules: boolean[][];
	private readonly isFunction: boolean[][];
	private readonly version: number;

	constructor(version: number) {
		this.version = version;
		this.size = version * 4 + 17;
		this.modules = Array.from({ length: this.size }, () => new Array(this.size).fill(false));
		this.isFunction = Array.from({ length: this.size }, () => new Array(this.size).fill(false));
	}

	private set(x: number, y: number, dark: boolean): void {
		this.modules[y][x] = dark;
		this.isFunction[y][x] = true;
	}

	drawFunctionPatterns(): void {
		for (let i = 0; i < this.size; i++) {
			this.set(6, i, i % 2 === 0);
			this.set(i, 6, i % 2 === 0);
		}
		this.finder(3, 3);
		this.finder(this.size - 4, 3);
		this.finder(3, this.size - 4);
		const align = this.alignmentPositions();
		const last = align.length - 1;
		align.forEach((x, i) => {
			align.forEach((y, j) => {
				const corner = (i === 0 && j === 0) || (i === 0 && j === last) || (i === last && j === 0);
				if (!corner) this.alignment(x, y);
			});
		});
		this.drawFormatBits(0); // reserve; redrawn per mask
		this.drawVersion();
	}

	private finder(cx: number, cy: number): void {
		for (let dy = -4; dy <= 4; dy++) {
			for (let dx = -4; dx <= 4; dx++) {
				const x = cx + dx;
				const y = cy + dy;
				if (x < 0 || x >= this.size || y < 0 || y >= this.size) continue;
				const dist = Math.max(Math.abs(dx), Math.abs(dy));
				this.set(x, y, dist !== 2 && dist !== 4);
			}
		}
	}

	private alignment(cx: number, cy: number): void {
		for (let dy = -2; dy <= 2; dy++) {
			for (let dx = -2; dx <= 2; dx++) {
				this.set(cx + dx, cy + dy, Math.max(Math.abs(dx), Math.abs(dy)) !== 1);
			}
		}
	}

	private alignmentPositions(): number[] {
		if (this.version === 1) return [];
		const count = Math.floor(this.version / 7) + 2;
		const step = Math.floor((this.version * 8 + count * 3 + 5) / (count * 4 - 4)) * 2;
		const result = [6];
		for (let pos = this.size - 7; result.length < count; pos -= step) result.splice(1, 0, pos);
		return result;
	}

	drawFormatBits(mask: number): void {
		const data = (LEVEL_M << 3) | mask;
		let rem = data;
		for (let i = 0; i < 10; i++) rem = (rem << 1) ^ ((rem >>> 9) * 0x537);
		const bits = ((data << 10) | rem) ^ 0x5412;
		const bit = (i: number) => ((bits >>> i) & 1) !== 0;
		for (let i = 0; i <= 5; i++) this.set(8, i, bit(i));
		this.set(8, 7, bit(6));
		this.set(8, 8, bit(7));
		this.set(7, 8, bit(8));
		for (let i = 9; i < 15; i++) this.set(14 - i, 8, bit(i));
		for (let i = 0; i < 8; i++) this.set(this.size - 1 - i, 8, bit(i));
		for (let i = 8; i < 15; i++) this.set(8, this.size - 15 + i, bit(i));
		this.set(8, this.size - 8, true);
	}

	private drawVersion(): void {
		if (this.version < 7) return;
		let rem = this.version;
		for (let i = 0; i < 12; i++) rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
		const bits = (this.version << 12) | rem;
		for (let i = 0; i < 18; i++) {
			const dark = ((bits >>> i) & 1) !== 0;
			const a = this.size - 11 + (i % 3);
			const b = Math.floor(i / 3);
			this.set(a, b, dark);
			this.set(b, a, dark);
		}
	}

	drawCodewords(data: number[]): void {
		let i = 0;
		for (let right = this.size - 1; right >= 1; right -= 2) {
			if (right === 6) right = 5;
			for (let vert = 0; vert < this.size; vert++) {
				for (let j = 0; j < 2; j++) {
					const x = right - j;
					const upward = ((right + 1) & 2) === 0;
					const y = upward ? this.size - 1 - vert : vert;
					if (!this.isFunction[y][x] && i < data.length * 8) {
						this.modules[y][x] = ((data[i >>> 3] >>> (7 - (i & 7))) & 1) !== 0;
						i++;
					}
				}
			}
		}
	}

	applyMask(mask: number): void {
		for (let y = 0; y < this.size; y++) {
			for (let x = 0; x < this.size; x++) {
				if (this.isFunction[y][x]) continue;
				let invert: boolean;
				switch (mask) {
					case 0:
						invert = (x + y) % 2 === 0;
						break;
					case 1:
						invert = y % 2 === 0;
						break;
					case 2:
						invert = x % 3 === 0;
						break;
					case 3:
						invert = (x + y) % 3 === 0;
						break;
					case 4:
						invert = (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0;
						break;
					case 5:
						invert = ((x * y) % 2) + ((x * y) % 3) === 0;
						break;
					case 6:
						invert = (((x * y) % 2) + ((x * y) % 3)) % 2 === 0;
						break;
					default:
						invert = (((x + y) % 2) + ((x * y) % 3)) % 2 === 0;
				}
				if (invert) this.modules[y][x] = !this.modules[y][x];
			}
		}
	}

	/** The four penalty rules of the standard (runs, boxes, finder look-alikes, balance). */
	penalty(): number {
		const n = this.size;
		const m = this.modules;
		let score = 0;
		const lines: boolean[][] = [];
		for (let i = 0; i < n; i++) {
			lines.push(m[i]);
			lines.push(m.map((row) => row[i]));
		}
		const finderLike = [
			[true, false, true, true, true, false, true, false, false, false, false],
			[false, false, false, false, true, false, true, true, true, false, true],
		];
		for (const line of lines) {
			let run = 1;
			for (let i = 1; i <= n; i++) {
				if (i < n && line[i] === line[i - 1]) {
					run++;
				} else {
					if (run >= 5) score += 3 + (run - 5);
					run = 1;
				}
			}
			for (let i = 0; i + 11 <= n; i++) {
				if (finderLike.some((p) => p.every((v, k) => line[i + k] === v))) score += 40;
			}
		}
		for (let y = 0; y < n - 1; y++) {
			for (let x = 0; x < n - 1; x++) {
				const c = m[y][x];
				if (c === m[y][x + 1] && c === m[y + 1][x] && c === m[y + 1][x + 1]) score += 3;
			}
		}
		const dark = m.reduce((sum, row) => sum + row.filter(Boolean).length, 0);
		const total = n * n;
		score += (Math.ceil(Math.abs(dark * 20 - total * 10) / total) - 1) * 10;
		return score;
	}
}
