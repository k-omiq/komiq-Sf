<script lang="ts">
	import type { ComicType } from '$lib/data/types';

	let {
		type,
		width = 30,
		height = 20,
	}: { type: ComicType; width?: number; height?: number } = $props();

	function starPts(cx: number, cy: number, r: number): string {
		const pts: [number, number][] = [];
		for (let i = 0; i < 5; i++) {
			const ao = ((-90 + i * 72) * Math.PI) / 180;
			pts.push([cx + r * Math.cos(ao), cy + r * Math.sin(ao)]);
			const ai = ((-90 + i * 72 + 36) * Math.PI) / 180;
			pts.push([cx + r * 0.382 * Math.cos(ai), cy + r * 0.382 * Math.sin(ai)]);
		}
		return pts.map((p) => `${p[0].toFixed(2)},${p[1].toFixed(2)}`).join(' ');
	}
</script>

<svg {width} {height} viewBox="0 0 30 20" preserveAspectRatio="none" role="img" aria-label={type}>
	{#if type === 'Manga'}
		<rect width="30" height="20" fill="#fff" />
		<circle cx="15" cy="10" r="5.6" fill="#bc002d" />
	{:else if type === 'Manhwa'}
		<rect width="30" height="20" fill="#fff" />
		<circle cx="15" cy="10" r="5.6" fill="#cd2e3a" />
		<path d="M9.4 10 A5.6 5.6 0 0 0 20.6 10 Z" fill="#0047a0" />
		<g stroke="#111" stroke-width="0.7">
			<line x1="5" y1="4.5" x2="8" y2="6" />
			<line x1="25" y1="4.5" x2="22" y2="6" />
			<line x1="5" y1="15.5" x2="8" y2="14" />
			<line x1="25" y1="15.5" x2="22" y2="14" />
		</g>
	{:else}
		<rect width="30" height="20" fill="#de2910" />
		<polygon points={starPts(6, 6, 3.2)} fill="#ffde00" />
		<polygon points={starPts(11, 2.6, 1.1)} fill="#ffde00" />
		<polygon points={starPts(13.2, 5, 1.1)} fill="#ffde00" />
		<polygon points={starPts(13.2, 8.4, 1.1)} fill="#ffde00" />
		<polygon points={starPts(11, 10.8, 1.1)} fill="#ffde00" />
	{/if}
</svg>

<style>
	svg {
		display: block;
		border-radius: 4px;
		border: 1px solid rgba(255, 255, 255, 0.3);
		box-shadow: 0 1px 5px rgba(0, 0, 0, 0.45);
	}
</style>
