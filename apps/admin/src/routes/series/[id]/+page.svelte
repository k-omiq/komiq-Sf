<script lang="ts">
	import type { Series, WorkSource } from '@komika/types';
	import type { AdminChapter, SeriesAdminMeta, SeriesMetadataInput } from '@komika/api';
	import { page } from '$app/state';
	import { assetSrc } from '$lib/config';
	import {
		COMIC_TYPE_OPTIONS,
		addSeriesAltTitle,
		loadSeriesAdminMeta,
		loadSeriesDetail,
		loadWorkChaptersAdmin,
		loadWorkSources,
		removeSeriesAltTitle,
		rescanWork,
		saveSeriesMetadata,
		setChapterOverride,
		triggerScan,
	} from '$lib/data';

	const id = $derived(page.params.id ?? '');

	let series = $state<Series | null>(null);
	let meta = $state<SeriesAdminMeta | null>(null);
	let sources = $state<WorkSource[]>([]);
	let chapters = $state<AdminChapter[]>([]);
	let loading = $state(true);
	let loadError = $state<string | null>(null);

	// ---- metadata editor form (whole-state; seeded from effective + overrides) ----
	let fTitle = $state('');
	let fDescription = $state('');
	let fType = $state(''); // '' => Auto (clear override), else a ComicType word
	let fNsfw = $state<'auto' | 'sfw' | 'nsfw'>('auto');
	let fTags = $state<string[]>([]);
	// Whether the admin has touched the tag chips this session. Tags are seeded from
	// the EFFECTIVE genres (which may be merely source-derived), so we only send them
	// on save when actually edited — otherwise saving would silently PIN source genres
	// as a curated set.
	let tagsDirty = $state(false);
	let newTag = $state('');
	let saving = $state(false);
	let saveMsg = $state<string | null>(null);

	// Monotonic token so a slow load(A) can't clobber a newer load(B) after fast nav.
	let loadSeq = 0;

	function seedForm(m: SeriesAdminMeta): void {
		fTitle = m.titleOverride ?? '';
		fDescription = m.descriptionOverride ?? '';
		fType = m.contentTypeOverride ?? '';
		fNsfw = m.isNsfwOverride === null ? 'auto' : m.isNsfwOverride ? 'nsfw' : 'sfw';
		fTags = [...m.tags];
		tagsDirty = false;
	}

	async function load(seriesId: string): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		saveMsg = null;
		try {
			const s = await loadSeriesDetail(seriesId);
			const m = await loadSeriesAdminMeta(seriesId);
			const [srcs, chs] = m.workId
				? await Promise.all([loadWorkSources(m.workId), loadWorkChaptersAdmin(m.workId)])
				: [[], []];
			if (seq !== loadSeq) return; // a newer load superseded this one
			series = s;
			meta = m;
			seedForm(m);
			sources = srcs;
			chapters = chs;
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load series.';
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	$effect(() => {
		if (id) load(id);
	});

	function addTag(): void {
		const t = newTag.trim();
		if (t && !fTags.includes(t)) {
			fTags = [...fTags, t];
			tagsDirty = true;
		}
		newTag = '';
	}
	function removeTag(tag: string): void {
		fTags = fTags.filter((t) => t !== tag);
		tagsDirty = true;
	}

	// ---- alt-titles editor (persisted immediately; add can auto-merge duplicates) ----
	let newAltTitle = $state('');
	let altBusy = $state(false);
	let altMsg = $state<string | null>(null);

	async function addAlt(): Promise<void> {
		const t = newAltTitle.trim();
		if (!series || !t || altBusy) return;
		altBusy = true;
		altMsg = null;
		try {
			// Returns the recomputed series — reflects the new title AND any auto-merge of
			// other works that shared it.
			series = await addSeriesAltTitle(series.id, t);
			newAltTitle = '';
			altMsg = 'Added.';
		} catch (err) {
			altMsg = err instanceof Error ? err.message : 'Failed to add alt title.';
		} finally {
			altBusy = false;
		}
	}

	async function removeAlt(title: string): Promise<void> {
		if (!series || altBusy) return;
		altBusy = true;
		altMsg = null;
		try {
			series = await removeSeriesAltTitle(series.id, title);
		} catch (err) {
			altMsg = err instanceof Error ? err.message : 'Failed to remove alt title.';
		} finally {
			altBusy = false;
		}
	}

	async function persistMeta(input: SeriesMetadataInput): Promise<void> {
		if (!series) return;
		saving = true;
		saveMsg = null;
		try {
			series = await saveSeriesMetadata(input);
			// Re-pull the raw override state and RE-SEED the form so the fields reflect
			// exactly what persisted (server-side trims/dedupes included).
			meta = await loadSeriesAdminMeta(series.id);
			seedForm(meta);
			saveMsg = 'Saved.';
		} catch (err) {
			saveMsg = err instanceof Error ? err.message : 'Failed to save.';
		} finally {
			saving = false;
		}
	}

	async function saveMeta(): Promise<void> {
		if (!series) return;
		const input: SeriesMetadataInput = {
			seriesId: series.id,
			// Empty string clears the override (fall back to the source value).
			title: fTitle.trim() === '' ? null : fTitle.trim(),
			description: fDescription.trim() === '' ? null : fDescription,
			// '' => Auto (clear the pin); otherwise pin the chosen type.
			type: fType === '' ? null : (fType as SeriesMetadataInput['type']),
			isNsfw: fNsfw === 'auto' ? null : fNsfw === 'nsfw',
		};
		// Only touch tags when edited (omit key => server leaves them unchanged).
		if (tagsDirty) input.tags = fTags;
		await persistMeta(input);
	}

	async function resetTagsToSource(): Promise<void> {
		if (!series) return;
		// null clears the curated set → genres derive from the source again.
		await persistMeta({ seriesId: series.id, tags: null });
	}

	// ---- per-source rescan ----
	let scanningKey = $state<string | null>(null);
	let rescanMsg = $state<string | null>(null);

	async function scanSource(src: WorkSource): Promise<void> {
		scanningKey = src.sourceKey;
		rescanMsg = null;
		try {
			await triggerScan(src.sourceKey);
			rescanMsg = `Scanned ${extLabel(src)}.`;
			if (meta?.workId) chapters = await loadWorkChaptersAdmin(meta.workId);
		} catch (err) {
			rescanMsg = err instanceof Error ? err.message : 'Scan failed.';
		} finally {
			scanningKey = null;
		}
	}

	let rescanningAll = $state(false);
	async function scanAll(): Promise<void> {
		if (!meta?.workId) return;
		rescanningAll = true;
		rescanMsg = null;
		try {
			const n = await rescanWork(meta.workId);
			rescanMsg = `Re-scanned ${n} source${n === 1 ? '' : 's'}.`;
			chapters = await loadWorkChaptersAdmin(meta.workId);
		} catch (err) {
			rescanMsg = err instanceof Error ? err.message : 'Rescan failed.';
		} finally {
			rescanningAll = false;
		}
	}

	function extLabel(src: WorkSource): string {
		const pkg = src.extension?.pkgName;
		const name = pkg ? (pkg.split('.').pop() ?? pkg) : src.sourceType;
		return src.lang ? `${name} (${src.lang})` : name;
	}

	// ---- chapter overrides (soft-hide / rename) ----
	let editingKey = $state<string | null>(null);
	let editTitle = $state('');
	let chapBusyKey = $state<string | null>(null);
	let chapMsg = $state<string | null>(null);

	function startRename(c: AdminChapter): void {
		editingKey = c.key;
		editTitle = c.effectiveTitle ?? '';
	}

	async function commitChapter(
		c: AdminChapter,
		patch: { hidden?: boolean | null; title?: string | null },
	): Promise<void> {
		if (!meta?.workId) return;
		chapBusyKey = c.key;
		chapMsg = null;
		try {
			await setChapterOverride({ workId: meta.workId, chapterKey: c.key, ...patch });
			chapters = await loadWorkChaptersAdmin(meta.workId);
			editingKey = null;
		} catch (err) {
			chapMsg = err instanceof Error ? err.message : 'Failed to update chapter.';
		} finally {
			chapBusyKey = null;
		}
	}
</script>

<div class="wrap">
	<a class="back" href="/">← Catalog</a>

	{#if loading}
		<p class="status">Loading…</p>
	{:else if loadError}
		<p class="status error">{loadError}</p>
	{:else if series}
		<header class="head">
			{#if series.coverUrl}
				<img
					class="cover"
					src={assetSrc(series.coverUrl)}
					alt=""
					loading="lazy"
					referrerpolicy="no-referrer"
				/>
			{:else}
				<div class="cover placeholder"></div>
			{/if}
			<div class="head-meta">
				<h1>{series.title}</h1>
				<div class="badges">
					<span class="badge">{series.type}</span>
					<span class="badge">{series.status}</span>
					{#if series.isNsfw}<span class="badge nsfw">NSFW</span>{/if}
					<span class="badge muted">#{series.id}</span>
					<span class="badge muted">{series.chapterCount} ch</span>
				</div>
				{#if series.author}<p class="author">{series.author}</p>{/if}
			</div>
		</header>

		{#if !meta?.workId}
			<p class="notice">
				This series isn't catalogued yet, so its metadata can't be edited. Add it to the
				canonical catalogue from the <a href="/">Catalog</a> page first.
			</p>
		{:else}
			<!-- ---- Metadata editor ---- -->
			<section class="card">
				<h2>Metadata</h2>
				<label class="field">
					<span>Title override</span>
					<input type="text" bind:value={fTitle} placeholder={series.title} />
					<small>Empty = use the source title.</small>
				</label>
				<label class="field">
					<span>Description override</span>
					<textarea rows="5" bind:value={fDescription} placeholder="Use source description…"
					></textarea>
					<small>Empty = use the source description.</small>
				</label>
				<div class="row">
					<label class="field">
						<span>Type</span>
						<select bind:value={fType}>
							<option value="">Auto (derived — {series.type})</option>
							{#each COMIC_TYPE_OPTIONS as t (t)}
								<option value={t}>{t}</option>
							{/each}
						</select>
					</label>
					<label class="field">
						<span>NSFW</span>
						<select bind:value={fNsfw}>
							<option value="auto">Auto (derived — {series.isNsfw ? 'NSFW' : 'SFW'})</option>
							<option value="sfw">SFW</option>
							<option value="nsfw">NSFW — requires viewer's Show NSFW</option>
						</select>
					</label>
				</div>
				<div class="field">
					<span>Tags</span>
					<div class="tags">
						{#each fTags as tag (tag)}
							<span class="tag"
								>{tag}<button
									type="button"
									class="tag-x"
									aria-label={`Remove ${tag}`}
									onclick={() => removeTag(tag)}>×</button
								></span
							>
						{/each}
						{#if fTags.length === 0}<span class="muted">No tags</span>{/if}
					</div>
					<div class="tag-add">
						<input
							type="text"
							bind:value={newTag}
							placeholder="Add tag…"
							onkeydown={(e) => e.key === 'Enter' && (e.preventDefault(), addTag())}
						/>
						<button type="button" onclick={addTag}>Add</button>
						{#if meta.hasCuratedTags}
							<button type="button" class="ghost" onclick={resetTagsToSource} disabled={saving}
								>Reset to source</button
							>
						{/if}
					</div>
					<small
						>{meta.hasCuratedTags
							? 'Curated set — “Reset to source” reverts to the source genres.'
							: 'Derived from source — editing then saving pins it as a curated set.'}</small
					>
				</div>
				<div class="field">
					<span>Alternative titles</span>
					<div class="tags">
						{#each series.altTitles as t (t)}
							<span class="tag"
								>{t}<button
									type="button"
									class="tag-x"
									aria-label={`Remove ${t}`}
									disabled={altBusy}
									onclick={() => removeAlt(t)}>×</button
								></span
							>
						{/each}
						{#if series.altTitles.length === 0}<span class="muted">No alternative titles</span
							>{/if}
					</div>
					<div class="tag-add">
						<input
							type="text"
							bind:value={newAltTitle}
							placeholder="Add alt title…"
							disabled={altBusy}
							onkeydown={(e) => e.key === 'Enter' && (e.preventDefault(), addAlt())}
						/>
						<button type="button" onclick={addAlt} disabled={altBusy}>Add</button>
						{#if altMsg}<span class="msg">{altMsg}</span>{/if}
					</div>
					<small
						>Adding a title that exactly matches another entry auto-merges them into this one.</small
					>
				</div>
				<div class="actions">
					<button class="primary" onclick={saveMeta} disabled={saving}>
						{saving ? 'Saving…' : 'Save metadata'}
					</button>
					{#if saveMsg}<span class="msg">{saveMsg}</span>{/if}
				</div>
			</section>

			<!-- ---- Sources / rescan ---- -->
			<section class="card">
				<div class="card-head">
					<h2>Sources</h2>
					<button onclick={scanAll} disabled={rescanningAll}>
						{rescanningAll ? 'Re-scanning…' : 'Re-scan all sources'}
					</button>
				</div>
				{#if rescanMsg}<p class="msg">{rescanMsg}</p>{/if}
				{#if sources.length === 0}
					<p class="muted">No source mappings.</p>
				{:else}
					<ul class="sources">
						{#each sources as src (`${src.sourceType}:${src.sourceId}:${src.sourceKey}`)}
							<li>
								<span class="src-name">{extLabel(src)}</span>
								<span class="muted">{src.sourceType} · {src.sourceKey}</span>
								{#if src.isNsfw}<span class="badge nsfw">NSFW</span>{/if}
								{#if src.sourceType === 'suwayomi'}
									<button onclick={() => scanSource(src)} disabled={scanningKey === src.sourceKey}>
										{scanningKey === src.sourceKey ? 'Scanning…' : 'Scan now'}
									</button>
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</section>

			<!-- ---- Chapters ---- -->
			<section class="card">
				<h2>Chapters <span class="muted">({chapters.length})</span></h2>
				{#if chapMsg}<p class="msg">{chapMsg}</p>{/if}
				{#if chapters.length === 0}
					<p class="muted">No chapters mirrored yet.</p>
				{:else}
					<ul class="chapters">
						{#each chapters as c (c.key)}
							<li class:hidden={c.hidden}>
								<span class="num">#{c.number}</span>
								{#if editingKey === c.key}
									<input class="rename" type="text" bind:value={editTitle} />
									<button
										onclick={() =>
										commitChapter(c, { title: editTitle.trim() === '' ? null : editTitle.trim() })}
										disabled={chapBusyKey === c.key}>Save</button
									>
									<button class="ghost" onclick={() => (editingKey = null)}>Cancel</button>
								{:else}
									<span class="ch-title">
										{c.effectiveTitle ?? '(untitled)'}
										{#if c.titleOverride}<span class="badge muted">renamed</span>{/if}
									</span>
									<span class="muted">{c.sourceCount} src</span>
									<button class="ghost" onclick={() => startRename(c)}>Rename</button>
									<button
										class="ghost"
										onclick={() => commitChapter(c, { hidden: !c.hidden })}
										disabled={chapBusyKey === c.key}
									>
										{c.hidden ? 'Unhide' : 'Hide'}
									</button>
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</section>
		{/if}
	{/if}
</div>

<style>
	.wrap {
		max-width: 900px;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.back {
		color: var(--k-text-fainter);
		text-decoration: none;
		font-size: 0.9rem;
	}
	.back:hover {
		text-decoration: underline;
	}
	.status {
		padding: 2rem;
		text-align: center;
		color: var(--k-text-fainter);
	}
	.status.error {
		color: #e2506a;
	}
	.head {
		display: flex;
		gap: 1rem;
		align-items: flex-start;
	}
	.cover {
		width: 96px;
		height: 136px;
		object-fit: cover;
		border-radius: 8px;
		background: var(--k-surface-2);
		flex-shrink: 0;
	}
	.cover.placeholder {
		display: block;
	}
	.head-meta h1 {
		margin: 0 0 0.5rem;
		font-size: 1.4rem;
	}
	.badges {
		display: flex;
		flex-wrap: wrap;
		gap: 0.4rem;
	}
	.badge {
		font-size: 0.72rem;
		padding: 0.15rem 0.45rem;
		border-radius: 999px;
		background: var(--k-surface-2);
		color: var(--k-text-1);
	}
	.badge.muted {
		color: var(--k-text-fainter);
	}
	.badge.nsfw {
		background: #5a1f2b;
		color: #ffd7de;
	}
	.author {
		margin: 0.5rem 0 0;
		color: var(--k-text-fainter);
		font-size: 0.9rem;
	}
	.notice,
	.msg {
		font-size: 0.9rem;
		color: var(--k-text-fainter);
	}
	.card {
		background: var(--k-surface);
		border: 1px solid var(--k-border);
		border-radius: 10px;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.card h2 {
		margin: 0;
		font-size: 1.05rem;
	}
	.card-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}
	.field > span {
		font-size: 0.85rem;
		font-weight: 600;
	}
	.field small {
		color: var(--k-text-fainter);
		font-size: 0.75rem;
	}
	.field input,
	.field textarea,
	.field select {
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: 6px;
		padding: 0.45rem 0.55rem;
		color: inherit;
		font: inherit;
	}
	.row {
		display: flex;
		gap: 1rem;
		flex-wrap: wrap;
	}
	.row .field {
		flex: 1;
		min-width: 200px;
	}
	.tags {
		display: flex;
		flex-wrap: wrap;
		gap: 0.4rem;
	}
	.tag {
		background: var(--k-surface-2);
		border-radius: 999px;
		padding: 0.15rem 0.2rem 0.15rem 0.55rem;
		font-size: 0.8rem;
		display: inline-flex;
		align-items: center;
		gap: 0.2rem;
	}
	.tag-x {
		background: none;
		border: none;
		color: var(--k-text-fainter);
		cursor: pointer;
		font-size: 1rem;
		line-height: 1;
		padding: 0 0.3rem;
	}
	.tag-add {
		display: flex;
		gap: 0.4rem;
	}
	.tag-add input {
		flex: 1;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: 6px;
		padding: 0.4rem 0.5rem;
		color: inherit;
		font: inherit;
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 0.75rem;
	}
	button {
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: 6px;
		padding: 0.4rem 0.7rem;
		color: inherit;
		cursor: pointer;
		font: inherit;
	}
	button:hover:not(:disabled) {
		border-color: var(--k-primary);
	}
	button:disabled {
		opacity: 0.5;
		cursor: default;
	}
	button.primary {
		background: var(--k-primary);
		color: var(--k-on-primary);
		border-color: transparent;
	}
	button.ghost {
		background: none;
		font-size: 0.8rem;
		padding: 0.25rem 0.5rem;
	}
	.sources,
	.chapters {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}
	.sources li,
	.chapters li {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		padding: 0.4rem 0.5rem;
		border: 1px solid var(--k-border);
		border-radius: 6px;
		font-size: 0.85rem;
	}
	.chapters li.hidden {
		opacity: 0.45;
	}
	.chapters .num {
		font-weight: 600;
		min-width: 3rem;
	}
	.ch-title {
		flex: 1;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.rename {
		flex: 1;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: 6px;
		padding: 0.3rem 0.5rem;
		color: inherit;
		font: inherit;
	}
	.muted {
		color: var(--k-text-fainter);
	}
	.src-name {
		font-weight: 600;
	}
</style>
