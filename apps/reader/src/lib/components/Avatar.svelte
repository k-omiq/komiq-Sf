<script lang="ts" module>
	// Deterministic fallback colour from a stable key (user id / name), so a user
	// without an uploaded avatar still gets a consistent tinted initial.
	const PALETTE: { bg: string; fg: string }[] = [
		{ bg: '#3a2f4a', fg: '#d9c7f0' },
		{ bg: '#2f4a3a', fg: '#a7e0c0' },
		{ bg: '#4a2f33', fg: '#f0b7bd' },
		{ bg: '#2f3a4a', fg: '#a7c8f0' },
		{ bg: '#4a412f', fg: '#f0dca7' },
		{ bg: '#2f4a48', fg: '#a7f0ea' },
	];
	function colorFor(key: string): { bg: string; fg: string } {
		let h = 0;
		for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) >>> 0;
		return PALETTE[h % PALETTE.length];
	}
	function initialOf(name: string): string {
		return (name.trim().charAt(0) || 'K').toUpperCase();
	}
</script>

<script lang="ts">
	import { avatarSrc } from '$lib/config';

	interface Props {
		/** Stored avatarUrl (a `/avatars/...` path or absolute URL), or null. */
		url?: string | null;
		/** Display name / id — drives the initial and fallback colour. */
		name: string;
		/** Pixel diameter. */
		size?: number;
		/** Colour key (defaults to `name`); pass a stable user id for consistency. */
		colorKey?: string;
	}

	let { url = null, name, size = 36, colorKey }: Props = $props();

	const src = $derived(avatarSrc(url));
	const c = $derived(colorFor(colorKey ?? name));
	const initial = $derived(initialOf(name));
	// Font scales with the circle; ~44% reads well across sizes.
	const fontSize = $derived(Math.round(size * 0.44));
</script>

{#if src}
	<img
		class="avatar-img"
		{src}
		alt={name}
		width={size}
		height={size}
		style="width:{size}px;height:{size}px;"
		loading="lazy"
	/>
{:else}
	<span
		class="avatar-fallback"
		style="width:{size}px;height:{size}px;background:{c.bg};color:{c.fg};font-size:{fontSize}px;"
		aria-label={name}
	>
		{initial}
	</span>
{/if}

<style>
	.avatar-img {
		border-radius: 50%;
		object-fit: cover;
		display: block;
		background: var(--k-avatar, #2a2a2e);
	}
	.avatar-fallback {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		border-radius: 50%;
		font-family: var(--k-font-display, inherit);
		font-weight: 700;
		line-height: 1;
		user-select: none;
	}
</style>
