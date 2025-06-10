<script lang="ts">
	import Icon from './Icon.svelte';
	import FlagBadge from './FlagBadge.svelte';
	import Cover from './Cover.svelte';
	import TranslatorChips from './TranslatorChips.svelte';
	import { slug, type ComicType } from '$lib/data/types';

	interface Chip {
		name: string;
		lang: string | null;
		iconUrl: string | null;
	}

	let {
		title,
		sub = '',
		subTitle = '',
		rating = '',
		cover = '',
		id = '',
		flagEmoji = '',
		flagType = null,
		status = null,
		fixed = false,
		width = 150,
		translators = [],
		loading = 'lazy',
	}: {
		title: string;
		sub?: string;
		/**
		 * Hover text for the `sub` line. Used to spell out a label that is necessarily
		 * terse — e.g. "· 4h" is the chapter RELEASE time, and this can add when we
		 * detected it — without adding a second visible line to the card.
		 */
		subTitle?: string;
		rating?: string;
		cover?: string;
		id?: string;
		flagEmoji?: string;
		flagType?: ComicType | null;
		status?: { label: string; color: string } | null;
		fixed?: boolean;
		width?: number;
		translators?: Chip[];
		/**
		 * Native lazy-load hint for the cover. Defaults to 'lazy' so cards in grids
		 * and horizontal rows only fetch their cover once scrolled near the viewport —
		 * offscreen covers no longer keep the whole page in a loading state on slow
		 * connections. Pass 'eager' for a known above-the-fold LCP card.
		 */
		loading?: 'eager' | 'lazy';
	} = $props();

	const href = $derived(`/series/${id || slug(title)}`);
</script>

<a {href} class="card" class:fixed style={fixed ? `width:${width}px` : ''}>
	<div class="cover" class:fixed style={fixed ? `height:${Math.round((width * 212) / 150)}px` : ''}>
		<Cover src={cover} alt={title} {loading} />
		{#if flagType}
			<span class="flag-svg"><FlagBadge type={flagType} /></span>
		{:else if flagEmoji}
			<span class="flag-emoji">{flagEmoji}</span>
		{/if}
		{#if rating}
			<span class="rating"><Icon name="star" size={10} fill="var(--k-star)" />{rating}</span>
		{/if}
		{#if status}
			<span class="status" style="color:{status.color}">
				<span class="dot" style="background:{status.color}"></span>{status.label}
			</span>
		{/if}
	</div>
	<div class="meta">
		<div class="title">{title}</div>
		{#if sub}<div class="sub" title={subTitle || undefined}>{sub}</div>{/if}
		{#if translators.length}
			<div class="tchips"><TranslatorChips chips={translators} size={18} max={4} /></div>
		{/if}
	</div>
</a>

<style>
	.card {
		display: flex;
		flex-direction: column;
		gap: 9px;
		text-decoration: none;
		cursor: pointer;
		min-width: 0;
	}
	.card.fixed {
		flex: 0 0 auto;
	}
	.cover {
		position: relative;
		width: 100%;
		aspect-ratio: 2 / 3;
		border-radius: 8px;
		overflow: hidden;
		transition:
			transform 0.18s,
			opacity 0.15s;
	}
	.cover.fixed {
		aspect-ratio: auto;
	}
	.card:hover .cover {
		opacity: 0.82;
	}
	.rating {
		position: absolute;
		top: 9px;
		right: 9px;
		display: inline-flex;
		align-items: center;
		gap: 3px;
		font-weight: 700;
		font-size: 11.5px;
		color: var(--k-text);
		background: rgba(12, 12, 13, 0.72);
		border-radius: 5px;
		padding: 3px 7px;
	}
	.flag-svg {
		position: absolute;
		top: 9px;
		left: 9px;
		width: 30px;
		height: 20px;
	}
	.flag-emoji {
		position: absolute;
		top: 9px;
		left: 9px;
		display: inline-flex;
		font-size: 15px;
		line-height: 1;
		background: rgba(12, 12, 13, 0.74);
		border: 1px solid var(--k-border-3);
		border-radius: 6px;
		padding: 3px 6px;
	}
	.status {
		position: absolute;
		bottom: 9px;
		left: 9px;
		display: inline-flex;
		align-items: center;
		gap: 5px;
		font-size: 10px;
		font-weight: 700;
		letter-spacing: 0.02em;
		background: rgba(12, 12, 13, 0.72);
		border-radius: 5px;
		padding: 3px 7px;
	}
	.dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
	}
	.title {
		font-weight: 600;
		font-size: 13.5px;
		color: var(--k-text-1);
		line-height: 1.25;
	}
	.sub {
		font-size: 12px;
		color: var(--k-text-faint);
		margin-top: 3px;
	}
	.tchips {
		margin-top: 7px;
	}
</style>
