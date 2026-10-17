<script lang="ts">
	import { modalFocus } from '$lib/modal-focus';
	// Admin-only picker for a work's default cover and title, opened from the series page.
	//
	// The cover is set with `setSeriesDefaultCover` (a gallery key, or null for the
	// server's source ranking); the title reuses `updateSeriesMetadata` (a string pins
	// it, null goes back to the source title). Both are optional backend methods, and
	// the page only mounts this dialog when they exist.
	//
	// The dialog stays open after a change so the admin can set both in one visit; the
	// page refetches the series through `onchanged` so the hero shows the result at once.
	import Icon from '$lib/components/Icon.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import { backend } from '$lib/context';
	import type { SeriesDefaults } from '@komika/api';

	let {
		open = $bindable(false),
		workId,
		workTitle,
		onchanged,
	}: {
		open?: boolean;
		workId: string;
		workTitle: string;
		/** Fired after a cover or title change lands, so the page can refetch. */
		onchanged?: () => void | Promise<void>;
	} = $props();

	const AUTO = '\u0000auto';

	let defaults = $state<SeriesDefaults | null>(null);
	let loading = $state(false);
	let loadError = $state('');
	let coverBusy = $state<string | null>(null);
	let coverError = $state('');
	let titleBusy = $state<string | null>(null);
	let titleError = $state('');
	let notice = $state('');
	// Bumped on reset: a late answer from a closed (or re-targeted) dialog is dropped.
	let generation = 0;

	const canTitle = $derived(!!backend.updateSeriesMetadata);
	const titleAuto = $derived(!!defaults && defaults.titleOverride === null);
	const coverAuto = $derived(
		!!defaults && defaults.pickedCover === null && !defaults.uploadedCover,
	);
	// One row per distinct title text, with every source that gives it.
	const titleRows = $derived.by(() => {
		const rows = new Map<string, string[]>();
		for (const t of defaults?.titles ?? []) {
			const names = rows.get(t.title) ?? [];
			if (t.sourceName && !names.includes(t.sourceName)) names.push(t.sourceName);
			rows.set(t.title, names);
		}
		return [...rows].map(([title, names]) => ({ title, sources: names.join(', ') }));
	});
	const customTitle = $derived(
		defaults?.titleOverride != null && !titleRows.some((r) => r.title === defaults?.titleOverride)
			? defaults.titleOverride
			: null,
	);

	function coverLabel(c: { volume: string | null; lang: string | null }): string {
		const parts: string[] = [];
		if (c.volume) parts.push(`Vol. ${c.volume}`);
		if (c.lang) parts.push(c.lang.toUpperCase());
		return parts.join(' · ');
	}

	function reset(): void {
		generation += 1;
		defaults = null;
		loading = false;
		loadError = '';
		coverBusy = null;
		coverError = '';
		titleBusy = null;
		titleError = '';
		notice = '';
	}

	function close(): void {
		open = false;
		reset();
	}

	$effect(() => {
		if (!open) return;
		const onkey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') close();
		};
		window.addEventListener('keydown', onkey);
		return () => window.removeEventListener('keydown', onkey);
	});

	// A different work (navigation) starts from scratch.
	let lastWork = '';
	$effect(() => {
		if (workId === lastWork) return;
		lastWork = workId;
		reset();
	});

	// Load on open: another admin (or the console) may have changed either default.
	$effect(() => {
		if (!open || defaults || loading || loadError) return;
		void load();
	});

	async function load(): Promise<void> {
		const gen = generation;
		loading = true;
		loadError = '';
		try {
			const d = await backend.seriesDefaults!(workId);
			if (gen === generation) defaults = d;
		} catch (e) {
			if (gen === generation)
				loadError = e instanceof Error ? e.message : 'Could not load the cover and title choices.';
		} finally {
			if (gen === generation) loading = false;
		}
	}

	async function pickCover(key: string | null): Promise<void> {
		if (!defaults || coverBusy || titleBusy) return;
		const gen = generation;
		coverBusy = key ?? AUTO;
		coverError = '';
		notice = '';
		try {
			const d = await backend.setSeriesDefaultCover!(workId, key);
			// The change landed even if the dialog closed meanwhile: refresh the page.
			void onchanged?.();
			if (gen !== generation) return;
			defaults = d;
			notice = d.notice ?? 'Cover updated.';
		} catch (e) {
			if (gen === generation)
				coverError = e instanceof Error ? e.message : 'Could not change the cover.';
		} finally {
			if (gen === generation) coverBusy = null;
		}
	}

	async function pickTitle(title: string | null): Promise<void> {
		if (!defaults || coverBusy || titleBusy || !backend.updateSeriesMetadata) return;
		const gen = generation;
		titleBusy = title ?? AUTO;
		titleError = '';
		notice = '';
		try {
			await backend.updateSeriesMetadata({ seriesId: workId, title });
			void onchanged?.();
			if (gen !== generation) return;
			defaults = { ...defaults, titleOverride: title };
			notice = 'Title updated.';
			// Re-read so the list matches what the server stored.
			try {
				const d = await backend.seriesDefaults!(workId);
				if (gen === generation) defaults = d;
			} catch {
				// The write succeeded; the local copy above is good enough.
			}
		} catch (e) {
			if (gen === generation)
				titleError = e instanceof Error ? e.message : 'Could not change the title.';
		} finally {
			if (gen === generation) titleBusy = null;
		}
	}
</script>

{#if open}
	<div
		class="scrim-wrap"
		use:modalFocus
		role="dialog"
		aria-modal="true"
		aria-labelledby="series-defaults-title"
	>
		<button class="scrim" onclick={close} tabindex="-1" aria-hidden="true"></button>
		<div class="panel">
			<header>
				<div class="head-text">
					<h2 id="series-defaults-title">Cover &amp; title</h2>
					<p class="sub">Choose what every reader sees for “{workTitle}”.</p>
				</div>
				<button class="icon-btn k-hit" onclick={close} aria-label="Close">
					<Icon name="x" size={18} />
				</button>
			</header>

			<div class="body">
				{#if loadError}
					<div class="msg error" role="alert">
						<Icon name="alert" size={15} />{loadError}
						<button class="ghost small" onclick={() => void load()}>Retry</button>
					</div>
				{:else if loading || !defaults}
					<div class="msg">Loading choices…</div>
				{:else}
					<section aria-labelledby="sd-cover-h">
						<h3 id="sd-cover-h">Cover</h3>
						{#if defaults.uploadedCover}
							<p class="hint">An uploaded cover is in use. Pick Automatic to drop it.</p>
						{/if}
						<div class="cover-grid" role="group" aria-labelledby="sd-cover-h">
							<button
								class="tile"
								class:on={coverAuto}
								aria-pressed={coverAuto}
								aria-busy={coverBusy === AUTO}
								disabled={!!coverBusy || !!titleBusy}
								onclick={() => pickCover(null)}
							>
								<span class="thumb auto">
									<Icon name="arrow-up-down" size={20} />
									{#if coverAuto}<span class="badge"
											><Icon name="check" size={12} strokeWidth={3} /></span
										>{/if}
									{#if coverBusy === AUTO}<span class="busy">Saving…</span>{/if}
								</span>
								<span class="tile-name">Automatic</span>
								<span class="tile-sub">Top-ranked source picks it</span>
							</button>
							{#each defaults.covers as c (c.key)}
								{@const on = defaults.pickedCover === c.key}
								{@const label = coverLabel(c)}
								<button
									class="tile"
									class:on
									aria-pressed={on}
									aria-busy={coverBusy === c.key}
									disabled={!!coverBusy || !!titleBusy}
									onclick={() => pickCover(c.key)}
								>
									<span class="thumb">
										<Cover
											src={c.thumbnailUrl}
											alt={`Cover from ${c.sourceName}${label ? `, ${label}` : ''}`}
											loading="lazy"
											sizing="card"
										/>
										{#if on}<span class="badge"
												><Icon name="check" size={12} strokeWidth={3} /></span
											>{/if}
										{#if c.current}<span class="shown">Shown</span>{/if}
										{#if coverBusy === c.key}<span class="busy">Saving…</span>{/if}
									</span>
									<span class="tile-name">{c.sourceName}</span>
									{#if label}<span class="tile-sub">{label}</span>{/if}
								</button>
							{/each}
						</div>
						{#if !defaults.covers.length}
							<p class="hint">No other covers found for this series.</p>
						{/if}
						{#if coverError}
							<div class="msg error" role="alert"><Icon name="alert" size={15} />{coverError}</div>
						{/if}
					</section>

					{#if canTitle}
						<section aria-labelledby="sd-title-h">
							<h3 id="sd-title-h">Title</h3>
							<div class="title-list" role="group" aria-labelledby="sd-title-h">
								<button
									class="title-row"
									class:on={titleAuto}
									aria-pressed={titleAuto}
									aria-busy={titleBusy === AUTO}
									disabled={!!coverBusy || !!titleBusy}
									onclick={() => pickTitle(null)}
								>
									<span class="title-text">
										<span class="t">{defaults.sourceTitle}</span>
										<span class="s"
											>{titleBusy === AUTO ? 'Saving…' : 'Source title · automatic'}</span
										>
									</span>
									{#if titleAuto}<span class="check"
											><Icon name="check" size={14} strokeWidth={3} /></span
										>{/if}
								</button>
								{#if customTitle !== null}
									<div class="title-row on static" aria-current="true">
										<span class="title-text">
											<span class="t">{customTitle}</span>
											<span class="s">Custom title</span>
										</span>
										<span class="check"><Icon name="check" size={14} strokeWidth={3} /></span>
									</div>
								{/if}
								{#each titleRows as row (row.title)}
									{@const on = defaults.titleOverride === row.title}
									<button
										class="title-row"
										class:on
										aria-pressed={on}
										aria-busy={titleBusy === row.title}
										disabled={!!coverBusy || !!titleBusy}
										onclick={() => pickTitle(row.title)}
									>
										<span class="title-text">
											<span class="t">{row.title}</span>
											{#if titleBusy === row.title}
												<span class="s">Saving…</span>
											{:else if row.sources}
												<span class="s">{row.sources}</span>
											{/if}
										</span>
										{#if on}<span class="check"
												><Icon name="check" size={14} strokeWidth={3} /></span
											>{/if}
									</button>
								{/each}
							</div>
							{#if titleError}
								<div class="msg error" role="alert">
									<Icon name="alert" size={15} />{titleError}
								</div>
							{/if}
						</section>
					{/if}
				{/if}
			</div>

			<footer>
				<span class="note" role="status">{notice}</span>
				<button class="ghost k-hit" onclick={close}>Done</button>
			</footer>
		</div>
	</div>
{/if}

<style>
	.scrim-wrap {
		position: fixed;
		inset: 0;
		z-index: 220;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 24px;
	}
	.scrim {
		position: fixed;
		inset: 0;
		border: none;
		background: rgba(6, 6, 7, 0.82);
		backdrop-filter: blur(6px);
	}
	.panel {
		overscroll-behavior: contain;
		position: relative;
		display: flex;
		flex-direction: column;
		width: min(680px, 100%);
		max-height: min(760px, 100%);
		background: var(--k-surface);
		border: 1px solid var(--k-border-2);
		border-radius: 16px;
		overflow: hidden;
	}
	header {
		display: flex;
		align-items: flex-start;
		gap: 12px;
		padding: 18px 18px 14px;
		border-bottom: 1px solid var(--k-border-1);
	}
	.head-text {
		flex: 1;
		min-width: 0;
	}
	h2 {
		margin: 0;
		font-family: var(--k-font-display);
		font-size: 1.05rem;
		color: var(--k-text-bright);
	}
	.sub {
		margin: 4px 0 0;
		font-size: 0.82rem;
		color: var(--k-text-dim);
		overflow-wrap: anywhere;
	}
	.icon-btn {
		display: grid;
		place-items: center;
		width: 30px;
		height: 30px;
		border: 1px solid var(--k-border-2);
		border-radius: 8px;
		background: transparent;
		color: var(--k-text-2);
		cursor: pointer;
	}
	.ghost {
		padding: 8px 14px;
		border-radius: 9px;
		border: 1px solid var(--k-border-2);
		background: var(--k-surface-2);
		color: var(--k-text-1);
		font: inherit;
		font-weight: 600;
		cursor: pointer;
	}
	.ghost.small {
		margin-left: auto;
		padding: 5px 10px;
		font-size: 0.8rem;
	}
	.body {
		flex: 1;
		overflow-y: auto;
		padding: 14px 18px 18px;
	}
	section + section {
		margin-top: 22px;
	}
	h3 {
		margin: 0 0 10px;
		font-size: 0.78rem;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-dim);
	}
	.hint {
		margin: -4px 0 10px;
		font-size: 0.82rem;
		color: var(--k-text-2);
	}
	.msg {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 12px 2px;
		font-size: 0.85rem;
		color: var(--k-text-dim);
	}
	.msg.error {
		color: #ff8a80;
	}

	/* ---- cover tiles ---- */
	.cover-grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(96px, 1fr));
		gap: 14px 12px;
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
		padding: 0;
		border: none;
		background: none;
		color: inherit;
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.tile:disabled {
		cursor: progress;
	}
	/* `position: relative` is load-bearing: Cover positions its image absolutely. */
	.thumb {
		position: relative;
		display: grid;
		place-items: center;
		width: 100%;
		aspect-ratio: 2 / 3;
		margin-bottom: 4px;
		border-radius: var(--k-radius-md);
		overflow: hidden;
		background: var(--k-surface-3);
		transition: box-shadow var(--k-dur, 0.15s) var(--k-ease, ease);
	}
	.thumb.auto {
		background: var(--k-hover-fill);
		color: var(--k-text-2);
	}
	.tile:not(:disabled):hover .thumb {
		box-shadow: 0 0 0 3px var(--k-hover-fill);
	}
	.tile.on .thumb {
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--k-accent) 45%, transparent);
	}
	.tile:disabled:not(.on) .thumb {
		opacity: 0.6;
	}
	.badge {
		position: absolute;
		top: 6px;
		right: 6px;
		z-index: 1;
		display: grid;
		place-items: center;
		width: 20px;
		height: 20px;
		border-radius: 50%;
		background: var(--k-accent);
		color: var(--k-on-accent);
	}
	.shown,
	.busy {
		position: absolute;
		z-index: 1;
		font-size: 0.66rem;
		font-weight: 700;
		border-radius: var(--k-radius-pill);
		padding: 2px 7px;
		background: color-mix(in srgb, var(--k-bg) 72%, transparent);
		color: var(--k-text-1);
	}
	.shown {
		left: 6px;
		bottom: 6px;
	}
	.busy {
		inset: auto 6px 50% 6px;
		transform: translateY(50%);
		text-align: center;
	}
	.tile-name,
	.tile-sub {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.tile-name {
		font-size: 0.8rem;
		font-weight: 600;
		color: var(--k-text-1);
	}
	.tile-sub {
		font-size: 0.72rem;
		color: var(--k-text-dim);
	}

	/* ---- title rows ---- */
	.title-list {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.title-row {
		display: flex;
		align-items: center;
		gap: 10px;
		width: 100%;
		padding: 9px 10px;
		border: none;
		border-radius: 10px;
		background: transparent;
		color: inherit;
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.title-row:not(:disabled):not(.static):hover {
		background: var(--k-hover-fill);
	}
	.title-row.on {
		background: var(--k-accent-soft);
	}
	.title-row:disabled {
		cursor: progress;
	}
	.title-row:disabled:not(.on) {
		opacity: 0.6;
	}
	.title-text {
		flex: 1;
		min-width: 0;
	}
	.title-text .t {
		display: block;
		font-size: 0.9rem;
		font-weight: 600;
		color: var(--k-text-1);
		overflow-wrap: anywhere;
	}
	.title-text .s {
		display: block;
		margin-top: 1px;
		font-size: 0.74rem;
		color: var(--k-text-dim);
	}
	.check {
		display: grid;
		place-items: center;
		flex: 0 0 auto;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: var(--k-accent);
		color: var(--k-on-accent);
	}

	footer {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 12px 16px;
		border-top: 1px solid var(--k-border-1);
	}
	.note {
		flex: 1;
		min-width: 0;
		font-size: 0.8rem;
		color: var(--k-text-dim);
	}
	@media (max-width: 480px) {
		.scrim-wrap {
			padding: 12px;
		}
		.body {
			padding: 12px 14px 16px;
		}
	}
</style>
