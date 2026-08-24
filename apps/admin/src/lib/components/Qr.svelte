<script lang="ts">
	import { encodeQr, qrPath } from '$lib/qr';

	// A scannable QR code of `value`. Always dark modules on a light tile, in either
	// theme: phone cameras read that reliably, inverted codes less so.
	let { value, size = 168, label }: { value: string; size?: number; label: string } = $props();

	const MARGIN = 4;
	const matrix = $derived(encodeQr(value));
	const view = $derived(matrix.size + MARGIN * 2);
	const d = $derived(qrPath(matrix, MARGIN));
</script>

<svg
	class="qr"
	width={size}
	height={size}
	viewBox="0 0 {view} {view}"
	role="img"
	aria-label={label}
	shape-rendering="crispEdges"
>
	<rect width={view} height={view} fill="#fff" />
	<path {d} fill="#0c0c0d" />
</svg>

<style>
	.qr {
		display: block;
		flex: none;
		border-radius: var(--k-radius);
	}
</style>
