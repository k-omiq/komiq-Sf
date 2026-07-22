<script lang="ts">
	import { onMount } from 'svelte';
	import { modal } from '$lib/modal';
	import { loadSourceReader, type SourceReaderView } from '$lib/data';
	import Button from './Button.svelte';
	import Icon from './Icon.svelte';
	import Segmented from './Segmented.svelte';

	let { seriesId, title, onclose } = $props<{
		seriesId: string;
		title: string;
		onclose: () => void;
	}>();

	let view = $state<SourceReaderView | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let mode = $state<'strip' | 'page'>('strip');
	let pageIndex = $state(0);
	let width = $state(720);
	let retryNonce = $state(0);
	let requestGeneration = 0;

	const MODES: { value: 'strip' | 'page'; label: string }[] = [
		{ value: 'strip', label: 'Long strip' },
		{ value: 'page', label: 'Single page' },
	];

	const chapters = $derived(view?.chapters ?? []);
	const chapter = $derived(view?.chapter ?? null);
	const pages = $derived(view?.pages ?? []);
	const currentPage = $derived(pages[Math.min(pageIndex, Math.max(0, pages.length - 1))]);
	const chapterIndex = $derived(
		chapter ? chapters.findIndex((candidate) => String(candidate.id) === String(chapter.id)) : -1,
	);
	const previousChapter = $derived(chapterIndex > 0 ? chapters[chapterIndex - 1] : null);
	const nextChapter = $derived(
		chapterIndex >= 0 && chapterIndex < chapters.length - 1 ? chapters[chapterIndex + 1] : null,
	);

	async function load(chapterId?: string | null): Promise<void> {
		const generation = ++requestGeneration;
		loading = true;
		error = null;
		pageIndex = 0;
		try {
			const next = await loadSourceReader(seriesId, title, chapterId);
			if (generation !== requestGeneration) return;
			view = next;
		} catch (err) {
			if (generation !== requestGeneration) return;
			error = err instanceof Error ? err.message : 'Failed to open this comic.';
			view = null;
		} finally {
			if (generation === requestGeneration) loading = false;
		}
	}

	function selectChapter(event: Event): void {
		void load((event.currentTarget as HTMLSelectElement).value);
	}

	function previousPage(): void {
		if (pageIndex > 0) pageIndex -= 1;
		else if (previousChapter) void load(String(previousChapter.id));
	}

	function nextPage(): void {
		if (pageIndex < pages.length - 1) pageIndex += 1;
		else if (nextChapter) void load(String(nextChapter.id));
	}

	function onKeydown(event: KeyboardEvent): void {
		if (event.key === 'Escape') {
			event.preventDefault();
			onclose();
			return;
		}
		if (
			event.target instanceof HTMLElement &&
			event.target.closest('input, select, textarea, [contenteditable="true"]')
		)
			return;
		if (
			event.key === ' ' &&
			event.target instanceof HTMLElement &&
			event.target.closest('button, a')
		)
			return;
		if (mode !== 'page' || loading) return;
		if (event.key === 'ArrowLeft' || event.key === 'PageUp') {
			event.preventDefault();
			previousPage();
		} else if (event.key === 'ArrowRight' || event.key === 'PageDown' || event.key === ' ') {
			event.preventDefault();
			nextPage();
		}
	}

	onMount(() => {
		void load();
		return () => {
			requestGeneration += 1;
		};
	});
</script>

<svelte:window onkeydown={onKeydown} />

<div class="reader-backdrop">
	<button
		data-modal-backdrop
		tabindex="-1"
		class="reader-scrim"
		onclick={onclose}
		aria-label="Close reader"
	></button>
	<div
		class="reader-shell"
		role="dialog"
		aria-modal="true"
		aria-label={`Read ${title}`}
		tabindex="-1"
		use:modal={onclose}
	>
		<header class="bar">
			<div class="heading">
				<h2 class="k-truncate">{title}</h2>
				<p class="k-truncate">Source preview. Nothing is added to the catalogue.</p>
			</div>
			<div class="controls">
				{#if chapters.length > 0 && chapter}
					<select
						class="k-select chapter-select"
						value={String(chapter.id)}
						onchange={selectChapter}
						aria-label="Choose chapter"
					>
						{#each chapters as option (option.id)}
							<option value={String(option.id)}>
								Ch. {option.label || option.number}{option.title ? `: ${option.title}` : ''}
							</option>
						{/each}
					</select>
				{/if}
				<Segmented
					value={mode}
					options={MODES}
					label="Reading mode"
					onchange={(next) => (mode = next)}
				/>
				<label class="width-control">
					<span class="k-label">Width <span class="k-num">{width}</span> px</span>
					<input
						autocomplete="off"
						name="width"
						type="range"
						min="420"
						max="1000"
						step="20"
						bind:value={width}
					/>
				</label>
			</div>
			<Button
				class="close"
				variant="ghost"
				icon="x"
				iconOnly
				aria-label="Close reader"
				onclick={onclose}
			/>
		</header>

		<div class="stage" style={`--reader-width: ${width}px`}>
			{#if loading}
				<div class="state" role="status">
					<span class="k-spin"><Icon name="refresh" size={22} /></span>
					<p>Loading chapter…</p>
				</div>
			{:else if error}
				<div class="state" role="alert">
					<strong>Could not open this comic</strong>
					<p>{error}</p>
					<Button icon="refresh" onclick={() => load(chapter ? String(chapter.id) : null)}
						>Retry</Button
					>
				</div>
			{:else if !chapter}
				<div class="state">
					<strong>No chapters available</strong>
					<p>This source has not returned a readable chapter for this comic.</p>
				</div>
			{:else if view?.externalUrl}
				<div class="state">
					<strong>This chapter is hosted off-site</strong>
					<p>The publisher serves this chapter on its own site.</p>
					<Button href={view.externalUrl} target="_blank" rel="noopener noreferrer" icon="external">
						Open chapter
					</Button>
				</div>
			{:else if pages.length === 0}
				<div class="state">
					<strong>No pages available</strong>
					<p>Try another chapter, or retry after the source recovers.</p>
				</div>
			{:else if mode === 'strip'}
				<div class="chapter-heading">
					<h3>Chapter {chapter.label || chapter.number}</h3>
					<p>{chapter.title || title}</p>
				</div>
				{#key `${chapter.id}:${retryNonce}`}
					<div class="strip">
						{#each pages as page (page.index)}
							<img
								src={page.url}
								alt={`Page ${page.index + 1}`}
								loading={page.index < 3 ? 'eager' : 'lazy'}
								fetchpriority={page.index < 3 ? 'high' : 'auto'}
								decoding="async"
							/>
						{/each}
					</div>
				{/key}
				<div class="chapter-end">
					<span class="end-note">End of chapter {chapter.label || chapter.number}</span>
					{#if nextChapter}
						<Button onclick={() => load(String(nextChapter.id))}>
							Next chapter <Icon name="chevron-right" />
						</Button>
					{:else}
						<span class="end-note">You are at the latest chapter.</span>
					{/if}
					<Button class="reload" icon="refresh" onclick={() => (retryNonce += 1)}>
						Reload images
					</Button>
				</div>
			{:else}
				<div class="paged">
					<div class="chapter-heading compact">
						<h3>Chapter {chapter.label || chapter.number}</h3>
						<p>{chapter.title || title}</p>
					</div>
					{#key `${chapter.id}:${currentPage?.index}:${retryNonce}`}
						<div class="leaf-frame">
							{#if currentPage}
								<img src={currentPage.url} alt={`Page ${currentPage.index + 1}`} decoding="async" />
							{/if}
							<button
								type="button"
								class="turn left"
								onclick={previousPage}
								aria-label="Previous page"
							></button>
							<button type="button" class="turn right" onclick={nextPage} aria-label="Next page"
							></button>
						</div>
					{/key}
					<div class="leaf-nav">
						<Button
							icon="chevron-left"
							onclick={previousPage}
							disabled={!previousChapter && pageIndex === 0}>Prev</Button
						>
						<span class="k-num">Page {pageIndex + 1} of {pages.length}</span>
						<Button onclick={nextPage} disabled={!nextChapter && pageIndex === pages.length - 1}>
							Next <Icon name="chevron-right" />
						</Button>
					</div>
				</div>
			{/if}
		</div>
	</div>
</div>

<style>
	.reader-backdrop {
		position: fixed;
		inset: 0;
		z-index: 200;
		display: grid;
		place-items: center;
		padding: var(--k-space-4);
		background: var(--k-scrim);
	}
	.reader-scrim {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
		padding: 0;
		border: 0;
		background: transparent;
		cursor: default;
	}
	.reader-shell {
		position: relative;
		width: min(1180px, 100%);
		height: min(920px, calc(100dvh - 2 * var(--k-space-4)));
		display: flex;
		flex-direction: column;
		overflow: hidden;
		overscroll-behavior: contain;
		background: var(--k-surface-2);
		color: var(--k-text);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-xl);
		box-shadow: var(--k-shadow-3);
	}
	.reader-shell:focus {
		outline: none;
	}

	/* ---- toolbar: follows the theme ---- */
	.bar {
		position: relative;
		z-index: 2;
		display: flex;
		align-items: center;
		gap: var(--k-space-4);
		padding: var(--k-space-3) var(--k-space-3) var(--k-space-3) var(--k-space-5);
		background: var(--k-surface-2);
		border-bottom: 1px solid var(--k-border-1);
	}
	.heading {
		flex: 1 1 160px;
		min-width: 0;
	}
	.heading h2 {
		font-size: var(--k-text-md);
		font-weight: 700;
	}
	.heading p {
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
	}
	.controls {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		flex-wrap: wrap;
		gap: var(--k-space-3);
		min-width: 0;
	}
	.chapter-select {
		width: min(260px, 25vw);
	}
	.width-control {
		display: grid;
		gap: 2px;
		width: 132px;
	}
	.width-control input {
		width: 100%;
		accent-color: var(--k-accent);
	}

	/* ---- reading canvas: dark in both themes, like any image viewer ----
	 * The two tokens swap roles in the light theme, so the canvas stays dark
	 * without a hard-coded colour. */
	.stage {
		--canvas: var(--k-bg);
		--ink: var(--k-text-bright);
		--ink-dim: color-mix(in srgb, var(--ink) 62%, var(--canvas));
		flex: 1;
		overflow: auto;
		overscroll-behavior: contain;
		background: var(--canvas);
		color: var(--ink);
	}
	:global(:root[data-theme='light']) .stage {
		--canvas: var(--k-text-bright);
		--ink: var(--k-bg);
	}
	.chapter-heading {
		width: min(var(--reader-width), calc(100% - 32px));
		margin: var(--k-space-8) auto var(--k-space-5);
	}
	.chapter-heading h3 {
		color: var(--ink);
		font-size: var(--k-text-xl);
		font-weight: 700;
	}
	.chapter-heading p {
		margin-top: 2px;
		color: var(--ink-dim);
		font-size: var(--k-text-sm);
		overflow-wrap: anywhere;
	}
	.chapter-heading.compact {
		margin-top: var(--k-space-5);
		margin-bottom: var(--k-space-3);
	}
	.strip {
		width: min(var(--reader-width), 100%);
		margin: 0 auto;
	}
	.strip img {
		display: block;
		width: 100%;
		height: auto;
		min-height: 180px;
		object-fit: contain;
		background: color-mix(in srgb, var(--ink) 6%, var(--canvas));
	}
	.chapter-end {
		width: min(var(--reader-width), calc(100% - 32px));
		margin: 0 auto;
		padding: var(--k-space-6) 0 var(--k-space-10);
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-3);
	}
	.end-note {
		color: var(--ink-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.chapter-end :global(.reload) {
		margin-left: auto;
	}
	.state {
		min-height: 440px;
		display: grid;
		place-content: center;
		justify-items: center;
		gap: var(--k-space-3);
		padding: var(--k-space-8);
		text-align: center;
		color: var(--ink-dim);
	}
	.state strong {
		color: var(--ink);
		font-family: var(--k-font-display);
		font-size: var(--k-text-xl);
	}
	.state p {
		max-width: 480px;
		font-size: var(--k-text-sm);
		line-height: 1.6;
		overflow-wrap: anywhere;
	}
	.paged {
		min-height: 100%;
		padding-bottom: var(--k-space-6);
	}
	.leaf-frame {
		position: relative;
		width: min(var(--reader-width), calc(100% - 32px));
		height: calc(100dvh - 250px);
		min-height: 420px;
		margin: 0 auto;
		display: grid;
		place-items: center;
	}
	.leaf-frame img {
		width: 100%;
		height: 100%;
		object-fit: contain;
	}
	.turn {
		position: absolute;
		top: 0;
		bottom: 0;
		width: 36%;
		border: 0;
		background: transparent;
		cursor: pointer;
	}
	.turn.left {
		left: 0;
	}
	.turn.right {
		right: 0;
	}
	.leaf-nav {
		display: flex;
		align-items: center;
		justify-content: center;
		flex-wrap: wrap;
		gap: var(--k-space-4);
		margin-top: var(--k-space-4);
		color: var(--ink-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	@media (max-width: 800px) {
		.reader-backdrop {
			padding: 0;
		}
		.reader-shell {
			height: 100dvh;
			border: 0;
			border-radius: 0;
		}
		/* Title and close on the first line, the controls on a line of their own. */
		.bar {
			display: grid;
			grid-template-columns: minmax(0, 1fr) auto;
			gap: var(--k-space-2) var(--k-space-3);
			padding: var(--k-space-2) var(--k-space-2) var(--k-space-3) var(--k-space-4);
		}
		.controls {
			grid-column: 1 / -1;
			grid-row: 2;
			justify-content: flex-start;
			padding-right: var(--k-space-2);
		}
		.width-control {
			display: none;
		}
		.chapter-select {
			flex: 1 1 120px;
			width: auto;
			/* Otherwise the longest chapter title sets the width and pushes the mode switch down. */
			min-width: 0;
		}
	}
</style>
