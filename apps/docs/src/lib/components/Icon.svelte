<script lang="ts" module>
	// The docs' slice of the console icon set (apps/admin/src/lib/components/Icon.svelte):
	// same 24px grid, round caps, 2px stroke, so the two read as one product.
	export type IconName =
		| 'book'
		| 'server'
		| 'compass'
		| 'wrench'
		| 'code'
		| 'search'
		| 'file'
		| 'hash'
		| 'chevron-right'
		| 'arrow-left'
		| 'arrow-right'
		| 'external'
		| 'menu'
		| 'sun'
		| 'moon'
		| 'monitor';
</script>

<script lang="ts">
	let {
		name,
		size = 16,
		strokeWidth = 2,
	}: {
		name: IconName;
		size?: number;
		strokeWidth?: number;
	} = $props();
</script>

<svg
	class="k-icon"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	fill="none"
	stroke="currentColor"
	stroke-width={strokeWidth}
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"
>
	{#if name === 'book'}
		<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" /><path
			d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"
		/>
	{:else if name === 'server'}
		<rect x="3" y="4" width="18" height="7" rx="2" /><rect
			x="3"
			y="13"
			width="18"
			height="7"
			rx="2"
		/><path d="M7 7.5h.01M7 16.5h.01" />
	{:else if name === 'compass'}
		<circle cx="12" cy="12" r="9" /><path d="m16.2 7.8-2.1 6.3-6.3 2.1 2.1-6.3 6.3-2.1z" />
	{:else if name === 'wrench'}
		<path
			d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"
		/>
	{:else if name === 'code'}
		<path d="m16 18 6-6-6-6M8 6l-6 6 6 6" />
	{:else if name === 'search'}
		<circle cx="11" cy="11" r="7" /><path d="M21 21l-4.35-4.35" />
	{:else if name === 'file'}
		<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" /><path
			d="M14 2v6h6M8 13h8M8 17h5"
		/>
	{:else if name === 'hash'}
		<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" />
	{:else if name === 'chevron-right'}
		<path d="M9 6l6 6-6 6" />
	{:else if name === 'arrow-left'}
		<path d="M19 12H5M11 18l-6-6 6-6" />
	{:else if name === 'arrow-right'}
		<path d="M5 12h14M13 6l6 6-6 6" />
	{:else if name === 'external'}
		<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" /><path
			d="M15 3h6v6M10 14 21 3"
		/>
	{:else if name === 'menu'}
		<path d="M3 6h18M3 12h18M3 18h18" />
	{:else if name === 'sun'}
		<circle cx="12" cy="12" r="4" /><path
			d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"
		/>
	{:else if name === 'moon'}
		<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
	{:else if name === 'monitor'}
		<rect x="2" y="3" width="20" height="14" rx="2" /><path d="M8 21h8M12 17v4" />
	{/if}
</svg>

<style>
	.k-icon {
		flex: none;
		display: block;
	}
</style>
