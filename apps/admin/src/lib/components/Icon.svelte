<script lang="ts" module>
	// Inline SVG icon set for the console. Shapes follow the reader's Icon set
	// (same 24px grid, round caps, 2px stroke) so both apps read as one product.
	export type IconName =
		| 'book'
		| 'clock'
		| 'layers'
		| 'image'
		| 'package'
		| 'compass'
		| 'activity'
		| 'users'
		| 'flag'
		| 'gear'
		| 'search'
		| 'plus'
		| 'check'
		| 'x'
		| 'alert'
		| 'info'
		| 'chevron-right'
		| 'chevron-left'
		| 'chevron-down'
		| 'arrow-left'
		| 'arrow-right'
		| 'external'
		| 'refresh'
		| 'pause'
		| 'play'
		| 'trash'
		| 'upload'
		| 'download'
		| 'menu'
		| 'sun'
		| 'moon'
		| 'monitor'
		| 'logout'
		| 'merge'
		| 'edit'
		| 'eye'
		| 'eye-off'
		| 'shield'
		| 'wrench'
		| 'database'
		| 'command'
		| 'server'
		| 'copy'
		| 'globe';
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
	{:else if name === 'clock'}
		<circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" />
	{:else if name === 'layers'}
		<path d="M12 2 2 7l10 5 10-5-10-5z" /><path d="m2 17 10 5 10-5" /><path d="m2 12 10 5 10-5" />
	{:else if name === 'image'}
		<rect x="3" y="3" width="18" height="18" rx="2" /><circle cx="8.5" cy="8.5" r="1.5" /><path
			d="M21 15l-5-5L5 21"
		/>
	{:else if name === 'package'}
		<path
			d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"
		/><path d="M3.3 7 12 12l8.7-5" /><path d="M12 22V12" />
	{:else if name === 'compass'}
		<circle cx="12" cy="12" r="9" /><path d="m16.2 7.8-2.1 6.3-6.3 2.1 2.1-6.3 6.3-2.1z" />
	{:else if name === 'activity'}
		<path d="M22 12h-4l-3 9L9 3l-3 9H2" />
	{:else if name === 'users'}
		<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path
			d="M22 21v-2a4 4 0 0 0-3-3.87"
		/><path d="M16 3.13a4 4 0 0 1 0 7.75" />
	{:else if name === 'flag'}
		<path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z" /><path d="M4 22v-7" />
	{:else if name === 'gear'}
		<circle cx="12" cy="12" r="3" /><path
			d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"
		/>
	{:else if name === 'search'}
		<circle cx="11" cy="11" r="7" /><path d="M21 21l-4.35-4.35" />
	{:else if name === 'plus'}
		<path d="M12 5v14M5 12h14" />
	{:else if name === 'check'}
		<path d="M20 6L9 17l-5-5" />
	{:else if name === 'x'}
		<path d="M18 6 6 18M6 6l12 12" />
	{:else if name === 'alert'}
		<path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z" /><path
			d="M12 9v4M12 17h.01"
		/>
	{:else if name === 'info'}
		<circle cx="12" cy="12" r="9" /><path d="M12 16v-4M12 8h.01" />
	{:else if name === 'chevron-right'}
		<path d="M9 6l6 6-6 6" />
	{:else if name === 'chevron-left'}
		<path d="M15 18l-6-6 6-6" />
	{:else if name === 'chevron-down'}
		<path d="M6 9l6 6 6-6" />
	{:else if name === 'arrow-left'}
		<path d="M19 12H5M11 18l-6-6 6-6" />
	{:else if name === 'arrow-right'}
		<path d="M5 12h14M13 6l6 6-6 6" />
	{:else if name === 'external'}
		<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" /><path
			d="M15 3h6v6M10 14 21 3"
		/>
	{:else if name === 'refresh'}
		<path d="M21 2v6h-6" /><path d="M3 12a9 9 0 0 1 15-6.7L21 8" /><path d="M3 22v-6h6" /><path
			d="M21 12a9 9 0 0 1-15 6.7L3 16"
		/>
	{:else if name === 'pause'}
		<path d="M8 5v14M16 5v14" />
	{:else if name === 'play'}
		<path d="M7 4l12 8-12 8z" />
	{:else if name === 'trash'}
		<path d="M3 6h18" /><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /><path
			d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"
		/>
	{:else if name === 'upload'}
		<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><path d="M17 8l-5-5-5 5M12 3v12" />
	{:else if name === 'download'}
		<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" /><path d="M7 10l5 5 5-5M12 15V3" />
	{:else if name === 'menu'}
		<path d="M3 6h18M3 12h18M3 18h18" />
	{:else if name === 'sun'}
		<circle cx="12" cy="12" r="4" /><path
			d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"
		/>
	{:else if name === 'moon'}
		<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
	{:else if name === 'server'}
		<rect x="3" y="4" width="18" height="7" rx="2" /><rect
			x="3"
			y="13"
			width="18"
			height="7"
			rx="2"
		/><path d="M7 7.5h.01M7 16.5h.01" />
	{:else if name === 'copy'}
		<rect x="9" y="9" width="12" height="12" rx="2" /><path
			d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"
		/>
	{:else if name === 'globe'}
		<circle cx="12" cy="12" r="9" /><path
			d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18"
		/>
	{:else if name === 'monitor'}
		<rect x="2" y="3" width="20" height="14" rx="2" /><path d="M8 21h8M12 17v4" />
	{:else if name === 'logout'}
		<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="M16 17l5-5-5-5M21 12H9" />
	{:else if name === 'merge'}
		<circle cx="18" cy="18" r="3" /><circle cx="6" cy="6" r="3" /><path d="M6 21V9a9 9 0 0 0 9 9" />
	{:else if name === 'edit'}
		<path d="M12 20h9" /><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z" />
	{:else if name === 'eye'}
		<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z" /><circle cx="12" cy="12" r="3" />
	{:else if name === 'eye-off'}
		<path d="M9.9 5.2A10.4 10.4 0 0 1 12 5c6.5 0 10 7 10 7a17.6 17.6 0 0 1-2.6 3.6" /><path
			d="M6.6 6.6A17.3 17.3 0 0 0 2 12s3.5 7 10 7a9.7 9.7 0 0 0 5.4-1.6"
		/><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2" /><path d="M2 2l20 20" />
	{:else if name === 'shield'}
		<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" />
	{:else if name === 'wrench'}
		<path
			d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"
		/>
	{:else if name === 'database'}
		<ellipse cx="12" cy="5" rx="9" ry="3" /><path d="M3 5v14c0 1.7 4 3 9 3s9-1.3 9-3V5" /><path
			d="M3 12c0 1.7 4 3 9 3s9-1.3 9-3"
		/>
	{:else if name === 'command'}
		<path
			d="M18 3a3 3 0 0 0-3 3v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V6a3 3 0 1 0-3 3h12a3 3 0 0 0 0-6z"
		/>
	{/if}
</svg>

<style>
	.k-icon {
		flex: none;
		display: block;
	}
</style>
