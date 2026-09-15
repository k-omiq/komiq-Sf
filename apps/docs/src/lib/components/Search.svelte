<script lang="ts">
	import { goto } from '$app/navigation';
	import { search, type SearchPage } from '$lib/search';
	import type { HandbookInfo } from '$lib/server/content';
	import Icon from './Icon.svelte';

	let {
		open = $bindable(false),
		handbooks,
	}: {
		open: boolean;
		/** Shown before anything is typed, as places to jump to. */
		handbooks: HandbookInfo[];
	} = $props();

	let dialog = $state<HTMLDialogElement>();
	let query = $state('');
	let active = $state(0);
	let pages = $state<SearchPage[] | null>(null);
	let failed = $state(false);

	// The index is one static file; it is fetched the first time search opens.
	async function loadIndex(): Promise<void> {
		if (pages) return;
		failed = false;
		try {
			const res = await fetch('/search.json');
			if (!res.ok) throw new Error(String(res.status));
			pages = await res.json();
		} catch {
			failed = true;
		}
	}

	$effect(() => {
		if (!dialog) return;
		if (open && !dialog.open) {
			query = '';
			active = 0;
			dialog.showModal();
			void loadIndex();
		} else if (!open && dialog.open) {
			dialog.close();
		}
	});

	const term = $derived(query.trim());
	const hits = $derived(
		term
			? search(pages ?? [], term)
			: handbooks.map((book) => ({
					key: book.href,
					href: book.href,
					kind: 'page' as const,
					title: book.title,
					where: `${book.chapters.length} chapters`,
					snippet: book.description,
					score: 0,
				})),
	);

	$effect(() => {
		if (active >= hits.length) active = Math.max(0, hits.length - 1);
	});

	function go(href: string): void {
		open = false;
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
				go(hit.href);
			}
		}
	}

	// Keep the highlighted row in view while the arrow keys move it.
	$effect(() => {
		dialog?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: 'nearest' });
	});

	const uid = $props.id();

	/** `text` cut at every match of a search term, so the matches can be marked. */
	function pieces(text: string): { text: string; hit: boolean }[] {
		const terms = term.toLowerCase().split(/\s+/).filter(Boolean);
		if (!terms.length) return [{ text, hit: false }];
		const pattern = new RegExp(
			`(${terms.map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})`,
			'gi',
		);
		return text
			.split(pattern)
			.filter(Boolean)
			.map((part) => ({ text: part, hit: terms.includes(part.toLowerCase()) }));
	}
</script>

{#snippet marked(text: string)}
	{#each pieces(text) as piece, n (n)}{#if piece.hit}<mark>{piece.text}</mark
			>{:else}{piece.text}{/if}{/each}
{/snippet}

<!-- A native dialog: the browser owns the focus trap, Escape and the backdrop. -->
<dialog
	bind:this={dialog}
	class="palette"
	aria-label="Search the docs"
	onclose={() => (open = false)}
	onclick={(e) => {
		if (e.target === dialog) open = false;
	}}
>
	<div class="field">
		<Icon name="search" size={18} />
		<input
			type="text"
			role="combobox"
			aria-expanded="true"
			aria-controls="{uid}-list"
			aria-activedescendant={hits[active] ? `${uid}-${active}` : undefined}
			aria-label="Search the docs"
			placeholder="Search pages, sections and commands"
			autocomplete="off"
			spellcheck="false"
			bind:value={query}
			oninput={() => (active = 0)}
			{onkeydown}
		/>
		<span class="k-kbd">Esc</span>
	</div>

	<div class="results" id="{uid}-list" role="listbox" aria-label="Results">
		{#if !term}<p class="heading">Handbooks</p>{/if}
		{#each hits as hit, i (hit.key)}
			<a
				id="{uid}-{i}"
				data-index={i}
				class="hit"
				role="option"
				aria-selected={active === i}
				href={hit.href}
				onmousemove={() => (active = i)}
				onclick={(e) => {
					e.preventDefault();
					go(hit.href);
				}}
			>
				<Icon name={hit.kind === 'page' ? 'file' : 'hash'} size={16} />
				<span class="text">
					<span class="line">
						<span class="title k-truncate">{@render marked(hit.title)}</span>
						<span class="where k-truncate">{hit.where}</span>
					</span>
					{#if hit.snippet}<span class="snippet">{@render marked(hit.snippet)}</span>{/if}
				</span>
			</a>
		{/each}

		{#if failed}
			<p class="note bad" role="alert">
				The search index did not load. Check your connection, then
				<button type="button" class="retry" onclick={loadIndex}>try again</button>.
			</p>
		{:else if term && !pages}
			<p class="note" role="status">Loading the index…</p>
		{:else if term && hits.length === 0}
			<p class="note" role="status">
				Nothing matches "{term}". Try one word, a command or a setting name.
			</p>
		{/if}
	</div>
</dialog>

<style>
	.palette {
		top: min(16vh, 130px);
		width: min(calc(100vw - 24px), 640px);
		max-height: min(72dvh, 560px);
		margin: 0 auto;
		padding: 0;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-2);
		color: var(--k-text);
		box-shadow: var(--k-shadow-3);
		overflow: hidden;
	}
	.palette[open] {
		display: flex;
		flex-direction: column;
		animation: drop var(--k-dur) var(--k-ease);
	}
	.palette::backdrop {
		background: var(--k-scrim);
		animation: fade var(--k-dur) var(--k-ease);
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
		height: 54px;
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
		align-items: flex-start;
		gap: var(--k-space-3);
		padding: 9px var(--k-space-3);
		border-radius: var(--k-radius);
		color: var(--k-text-dim);
	}
	.hit :global(svg) {
		margin-top: 3px;
	}
	.hit[aria-selected='true'] {
		background: var(--k-surface-5);
	}
	.hit[aria-selected='true'] :global(svg) {
		color: var(--k-accent);
	}
	.text {
		flex: 1 1 auto;
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.line {
		display: flex;
		align-items: baseline;
		gap: var(--k-space-3);
		min-width: 0;
	}
	.title {
		color: var(--k-text-1);
		font-size: var(--k-text-md);
		font-weight: 600;
	}
	.hit[aria-selected='true'] .title {
		color: var(--k-text-bright);
	}
	.where {
		flex: 0 1 auto;
		margin-left: auto;
		max-width: 50%;
		font-size: var(--k-text-xs);
	}
	.snippet {
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		overflow: hidden;
		font-size: var(--k-text-sm);
		line-height: 1.5;
	}
	mark {
		border-radius: 3px;
		background: var(--k-accent-soft);
		color: var(--k-text-bright);
	}
	.note {
		padding: var(--k-space-3);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
	}
	.note.bad {
		color: var(--k-danger);
	}
	.retry {
		padding: 0;
		border: 0;
		background: none;
		color: inherit;
		font-weight: 700;
		text-decoration: underline;
		text-underline-offset: 3px;
		cursor: pointer;
	}
	@keyframes fade {
		from {
			opacity: 0;
		}
	}
	@keyframes drop {
		from {
			opacity: 0;
			transform: translateY(-8px);
		}
	}
</style>
