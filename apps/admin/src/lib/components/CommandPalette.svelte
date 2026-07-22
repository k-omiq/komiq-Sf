<script lang="ts">
	import type { Series } from '@komika/types';
	import { goto } from '$app/navigation';
	import { assetSrc } from '$lib/config';
	import { loadCatalog } from '$lib/data';
	import { modal } from '$lib/modal';
	import Cover from './Cover.svelte';
	import Icon, { type IconName } from './Icon.svelte';

	let {
		pages,
		onclose,
	}: {
		/** Every place the admin can jump to: nav items and settings sections. */
		pages: { href: string; label: string; icon: IconName; hint?: string }[];
		onclose: () => void;
	} = $props();

	let query = $state('');
	let active = $state(0);
	let series = $state<Series[]>([]);
	let searching = $state(false);
	let searchError = $state<string | null>(null);

	const q = $derived(query.trim().toLowerCase());
	const pageHits = $derived(
		q ? pages.filter((p) => `${p.label} ${p.hint ?? ''}`.toLowerCase().includes(q)) : pages,
	);

	// Catalogue search, debounced. A sequence number drops answers that arrive
	// after the admin has typed something newer.
	let seq = 0;
	$effect(() => {
		const term = query.trim();
		const mine = ++seq;
		if (term.length < 2) {
			series = [];
			searching = false;
			searchError = null;
			return;
		}
		searching = true;
		const timer = setTimeout(async () => {
			try {
				const res = await loadCatalog(term, 1);
				if (mine !== seq) return;
				series = res.items.slice(0, 6);
				searchError = null;
			} catch (err) {
				if (mine !== seq) return;
				series = [];
				searchError = err instanceof Error ? err.message : 'Search failed.';
			} finally {
				if (mine === seq) searching = false;
			}
		}, 220);
		return () => clearTimeout(timer);
	});

	type Hit = { key: string; href: string };
	const hits = $derived<Hit[]>([
		...pageHits.map((p) => ({ key: `p:${p.href}`, href: p.href })),
		...series.map((s) => ({ key: `s:${s.id}`, href: `/series/${s.id}` })),
	]);

	// Keep the highlighted row inside the list as results change.
	$effect(() => {
		if (active >= hits.length) active = Math.max(0, hits.length - 1);
	});

	function open(href: string): void {
		onclose();
		void goto(href);
	}

	function onkeydown(e: KeyboardEvent): void {
		if (e.key === 'ArrowDown') {
			e.preventDefault();
			active = hits.length ? (active + 1) % hits.length : 0;
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			active = hits.length ? (active - 1 + hits.length) % hits.length : 0;
		} else if (e.key === 'Enter') {
			const hit = hits[active];
			if (hit) {
				e.preventDefault();
				open(hit.href);
			}
		}
	}

	const uid = $props.id();
</script>

<div class="scrim" data-modal-backdrop onclick={onclose} aria-hidden="true"></div>
<div class="palette" role="dialog" aria-modal="true" aria-label="Search" use:modal={onclose}>
	<div class="field">
		<Icon name="search" size={18} />
		<input
			type="text"
			role="combobox"
			aria-expanded="true"
			aria-controls="{uid}-list"
			aria-activedescendant={hits[active] ? `${uid}-${active}` : undefined}
			aria-label="Search pages and series"
			placeholder="Go to a page or find a series"
			autocomplete="off"
			spellcheck="false"
			bind:value={query}
			oninput={() => (active = 0)}
			{onkeydown}
		/>
		<span class="k-kbd">Esc</span>
	</div>

	<div class="results" id="{uid}-list" role="listbox" aria-label="Results">
		{#if pageHits.length > 0}
			<p class="heading">Pages</p>
			{#each pageHits as p, i (p.href)}
				<a
					id="{uid}-{i}"
					class="hit"
					role="option"
					aria-selected={active === i}
					href={p.href}
					onmousemove={() => (active = i)}
					onclick={(e) => {
						e.preventDefault();
						open(p.href);
					}}
				>
					<Icon name={p.icon} size={17} />
					<span class="k-truncate">{p.label}</span>
					{#if p.hint}<span class="hint k-truncate">{p.hint}</span>{/if}
				</a>
			{/each}
		{/if}

		{#if series.length > 0}
			<p class="heading">Series</p>
			{#each series as s, j (s.id)}
				{@const i = pageHits.length + j}
				<a
					id="{uid}-{i}"
					class="hit"
					role="option"
					aria-selected={active === i}
					href="/series/{s.id}"
					onmousemove={() => (active = i)}
					onclick={(e) => {
						e.preventDefault();
						open(`/series/${s.id}`);
					}}
				>
					<Cover src={assetSrc(s.coverUrl)} width={22} />
					<span class="k-truncate">{s.title}</span>
					<span class="hint k-num">{s.chapterCount} ch</span>
				</a>
			{/each}
		{/if}

		{#if searchError}
			<p class="note bad" role="alert">{searchError}</p>
		{:else if searching}
			<p class="note">Searching the catalog…</p>
		{:else if hits.length === 0}
			<p class="note">Nothing matches "{query.trim()}". Try a page name or a series title.</p>
		{/if}
	</div>
</div>

<style>
	.scrim {
		position: fixed;
		inset: 0;
		z-index: 300;
		background: var(--k-scrim);
		animation: fade var(--k-dur) var(--k-ease);
	}
	.palette {
		position: fixed;
		top: min(18vh, 140px);
		left: 50%;
		z-index: 301;
		display: flex;
		flex-direction: column;
		width: min(calc(100vw - 24px), 600px);
		max-height: min(70dvh, 520px);
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-2);
		box-shadow: var(--k-shadow-3);
		transform: translateX(-50%);
		overflow: hidden;
		animation: drop var(--k-dur) var(--k-ease);
	}
	.palette:focus {
		outline: none;
	}
	.field {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		padding: 0 var(--k-space-4);
		border-bottom: 1px solid var(--k-border-1);
		color: var(--k-text-dim);
	}
	input {
		flex: 1 1 auto;
		min-width: 0;
		height: 52px;
		border: 0;
		background: transparent;
		color: var(--k-text-bright);
		font-size: var(--k-text-lg);
	}
	input:focus-visible {
		outline: none;
	}
	.results {
		flex: 1 1 auto;
		min-height: 0;
		overflow-y: auto;
		overscroll-behavior: contain;
		padding: var(--k-space-2);
	}
	.heading {
		padding: var(--k-space-2) var(--k-space-3) 6px;
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 700;
	}
	.hit {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		min-height: 40px;
		padding: 6px var(--k-space-3);
		border-radius: var(--k-radius);
		color: var(--k-text-2);
		font-size: var(--k-text-md);
		font-weight: 600;
	}
	.hit[aria-selected='true'] {
		background: var(--k-surface-5);
		color: var(--k-text-bright);
	}
	.hit[aria-selected='true'] :global(svg) {
		color: var(--k-accent);
	}
	.hint {
		margin-left: auto;
		flex: none;
		max-width: 45%;
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
		font-weight: 500;
	}
	.note {
		padding: var(--k-space-3);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
	}
	.note.bad {
		color: var(--k-danger);
	}
	@keyframes fade {
		from {
			opacity: 0;
		}
	}
	@keyframes drop {
		from {
			opacity: 0;
			transform: translate(-50%, -8px);
		}
	}
</style>
