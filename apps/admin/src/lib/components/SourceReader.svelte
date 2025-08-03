<script lang="ts">
	import { onMount } from 'svelte';
	import { modal } from '$lib/modal';
	import { loadSourceReader, type SourceReaderView } from '$lib/data';

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
		<header class="reader-toolbar">
			<div class="reader-title">
				<span class="eyebrow">SOURCE PREVIEW</span>
				<strong>{title}</strong>
			</div>
			<div class="reader-controls">
				{#if chapters.length > 0 && chapter}
					<select value={String(chapter.id)} onchange={selectChapter} aria-label="Choose chapter">
						{#each chapters as option (option.id)}
							<option value={String(option.id)}>
								Ch. {option.label || option.number}{option.title ? ` · ${option.title}` : ''}
							</option>
						{/each}
					</select>
				{/if}
				<div class="mode-switch" aria-label="Reading mode">
					<button class:on={mode === 'strip'} onclick={() => (mode = 'strip')}>Long strip</button>
					<button class:on={mode === 'page'} onclick={() => (mode = 'page')}>Single page</button>
				</div>
				<label class="width-control">
					<span>Width {width}px</span>
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
				<button class="close" onclick={onclose} aria-label="Close reader">×</button>
			</div>
		</header>

		<div class="reader-stage" style={`--reader-width: ${width}px`}>
			{#if loading}
				<div class="reader-state">
					<span class="spinner"></span>
					<p>Loading chapter…</p>
				</div>
			{:else if error}
				<div class="reader-state error">
					<strong>Couldn’t open this comic</strong>
					<p>{error}</p>
					<button onclick={() => load(chapter ? String(chapter.id) : null)}>Retry</button>
				</div>
			{:else if !chapter}
				<div class="reader-state">
					<strong>No chapters available</strong>
					<p>This source has not returned a readable chapter for this comic.</p>
				</div>
			{:else if view?.externalUrl}
				<div class="reader-state">
					<strong>This chapter is hosted off-site</strong>
					<p>The publisher serves this chapter on its own site.</p>
					<a href={view.externalUrl} target="_blank" rel="noopener noreferrer">Open chapter ↗</a>
				</div>
			{:else if pages.length === 0}
				<div class="reader-state">
					<strong>No pages available</strong>
					<p>Try another chapter or retry after the source recovers.</p>
				</div>
			{:else if mode === 'strip'}
				<div class="chapter-heading">
					<span>CHAPTER {chapter.label || chapter.number}</span>
					<h2>{chapter.title || title}</h2>
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
					<span>END OF CHAPTER {chapter.label || chapter.number}</span>
					{#if nextChapter}
						<button onclick={() => load(String(nextChapter.id))}>Next chapter →</button>
					{:else}
						<strong>You’re at the latest chapter.</strong>
					{/if}
					<button class="quiet" onclick={() => (retryNonce += 1)}>Reload images</button>
				</div>
			{:else}
				<div class="paged">
					<div class="chapter-heading compact">
						<span>CHAPTER {chapter.label || chapter.number}</span>
						<h2>{chapter.title || title}</h2>
					</div>
					{#key `${chapter.id}:${currentPage?.index}:${retryNonce}`}
						<div class="page-frame">
							{#if currentPage}
								<img src={currentPage.url} alt={`Page ${currentPage.index + 1}`} decoding="async" />
							{/if}
							<button class="turn left" onclick={previousPage} aria-label="Previous page"></button>
							<button class="turn right" onclick={nextPage} aria-label="Next page"></button>
						</div>
					{/key}
					<div class="page-nav">
						<button onclick={previousPage} disabled={!previousChapter && pageIndex === 0}
							>← Prev</button
						>
						<span>{pageIndex + 1} / {pages.length}</span>
						<button onclick={nextPage} disabled={!nextChapter && pageIndex === pages.length - 1}
							>Next →</button
						>
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
		z-index: 100;
		display: grid;
		place-items: center;
		padding: 18px;
		background: rgba(0, 0, 0, 0.78);
		backdrop-filter: blur(8px);
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
		height: min(920px, calc(100vh - 36px));
		display: flex;
		flex-direction: column;
		overflow: hidden;
		background: #0c0c0d;
		color: #f2f1ee;
		border: 1px solid rgba(255, 255, 255, 0.14);
		border-radius: 16px;
		box-shadow: 0 24px 80px rgba(0, 0, 0, 0.55);
	}
	.reader-toolbar {
		position: relative;
		z-index: 2;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 18px;
		min-height: 70px;
		padding: 12px 16px 12px 20px;
		background: rgba(12, 12, 13, 0.94);
		border-bottom: 1px solid rgba(255, 255, 255, 0.08);
	}
	.reader-title {
		display: flex;
		flex-direction: column;
		min-width: 150px;
		max-width: 320px;
	}
	.reader-title strong {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
		font-size: 14px;
	}
	.eyebrow,
	.chapter-heading span,
	.chapter-end > span {
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.12em;
		color: #87857f;
	}
	.reader-controls {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: 10px;
		min-width: 0;
	}
	.reader-controls select,
	.mode-switch,
	.close,
	.page-nav button,
	.chapter-end button,
	.reader-state button,
	.reader-state a {
		border: 1px solid rgba(255, 255, 255, 0.14);
		background: #1a1a1b;
		color: #e7e5e0;
		border-radius: 8px;
	}
	.reader-controls select {
		width: min(260px, 25vw);
		height: 36px;
		padding: 0 10px;
		font-size: 12px;
	}
	.mode-switch {
		display: flex;
		overflow: hidden;
	}
	.mode-switch button {
		height: 34px;
		padding: 0 10px;
		border: 0;
		background: transparent;
		color: #87857f;
		font-size: 11px;
		font-weight: 700;
		cursor: pointer;
	}
	.mode-switch button.on {
		background: #f2f1ee;
		color: #0c0c0d;
	}
	.width-control {
		display: flex;
		flex-direction: column;
		gap: 2px;
		width: 120px;
		font-size: 9px;
		font-weight: 700;
		color: #87857f;
	}
	.width-control input {
		width: 100%;
	}
	.close {
		width: 36px;
		height: 36px;
		padding: 0;
		font-size: 24px;
		line-height: 1;
		cursor: pointer;
	}
	.reader-stage {
		flex: 1;
		overflow: auto;
		overscroll-behavior: contain;
		background: #080809;
	}
	.chapter-heading {
		width: min(var(--reader-width), calc(100% - 32px));
		margin: 34px auto 20px;
	}
	.chapter-heading h2 {
		margin-top: 4px;
		font-size: 20px;
		color: #f7f6f3;
	}
	.chapter-heading.compact {
		margin-top: 22px;
		margin-bottom: 12px;
	}
	.strip {
		width: min(var(--reader-width), 100%);
		margin: 0 auto;
		background: #111;
	}
	.strip img {
		width: 100%;
		height: auto;
		min-height: 180px;
		object-fit: contain;
		background: #141415;
	}
	.chapter-end {
		width: min(var(--reader-width), calc(100% - 32px));
		margin: 0 auto;
		padding: 26px 0 36px;
		display: flex;
		align-items: center;
		gap: 12px;
	}
	.chapter-end strong {
		font-size: 12px;
		color: #9c9a94;
	}
	.chapter-end button,
	.reader-state button,
	.reader-state a,
	.page-nav button {
		padding: 8px 13px;
		font-size: 12px;
		font-weight: 700;
		cursor: pointer;
	}
	.chapter-end .quiet {
		margin-left: auto;
		background: transparent;
		color: #9c9a94;
	}
	.reader-state {
		min-height: 440px;
		display: grid;
		place-content: center;
		justify-items: center;
		gap: 10px;
		padding: 32px;
		text-align: center;
		color: #9c9a94;
	}
	.reader-state strong {
		font-size: 18px;
		color: #f2f1ee;
	}
	.reader-state p {
		max-width: 480px;
		font-size: 13px;
	}
	.reader-state.error p {
		color: #f0808a;
	}
	.spinner {
		width: 24px;
		height: 24px;
		border: 2px solid rgba(255, 255, 255, 0.15);
		border-top-color: #f2f1ee;
		border-radius: 50%;
		animation: spin 0.7s linear infinite;
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	.paged {
		min-height: 100%;
		padding-bottom: 24px;
	}
	.page-frame {
		position: relative;
		width: min(var(--reader-width), calc(100% - 32px));
		height: calc(100vh - 250px);
		min-height: 420px;
		margin: 0 auto;
		display: grid;
		place-items: center;
	}
	.page-frame img {
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
	.page-nav {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 16px;
		margin-top: 14px;
		font-size: 12px;
		color: #9c9a94;
	}
	.page-nav button:disabled {
		opacity: 0.35;
		cursor: default;
	}
	@media (max-width: 800px) {
		.reader-backdrop {
			padding: 0;
		}
		.reader-shell {
			height: 100vh;
			border: 0;
			border-radius: 0;
		}
		.reader-toolbar {
			align-items: flex-start;
			padding: 10px;
		}
		.reader-title,
		.width-control {
			display: none;
		}
		.reader-controls {
			width: 100%;
		}
		.reader-controls select {
			flex: 1;
			width: auto;
		}
		.mode-switch button {
			padding: 0 7px;
		}
	}
</style>
