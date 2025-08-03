<script lang="ts">
	import type { Series, WorkSource } from '@komika/types';
	import type { AdminChapter, SeriesAdminMeta, SeriesMetadataInput } from '@komika/api';
	import { page } from '$app/state';
	import { beforeNavigate } from '$app/navigation';
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
	const trackerProviders: Record<string, { label: string; hosts: string[] }> = {
		mangadex: { label: 'MangaDex', hosts: ['mangadex.org'] },
		al: { label: 'AniList', hosts: ['anilist.co'] },
		mal: { label: 'MyAnimeList', hosts: ['myanimelist.net'] },
		mu: { label: 'MangaUpdates', hosts: ['www.mangaupdates.com', 'mangaupdates.com'] },
		kt: { label: 'Kitsu', hosts: ['kitsu.io', 'kitsu.app'] },
		ap: { label: 'Anime-Planet', hosts: ['www.anime-planet.com', 'anime-planet.com'] },
		nu: { label: 'Novel Updates', hosts: ['www.novelupdates.com', 'novelupdates.com'] },
	};
	function trustedTracker(provider: string, href: string): boolean {
		try {
			const url = new URL(href);
			return (
				url.protocol === 'https:' &&
				!url.username &&
				!url.password &&
				!url.port &&
				(trackerProviders[provider]?.hosts.includes(url.hostname) ?? false)
			);
		} catch {
			return false;
		}
	}
	const importedTrackers = $derived(
		(series?.metadata?.trackers ?? []).filter((link) => trustedTracker(link.provider, link.url)),
	);
	const tagGroups = $derived.by(() => {
		const labels: Record<string, string> = {
			genre: 'Genres',
			theme: 'Themes',
			format: 'Format',
			content: 'Content',
			custom: 'Curated tags',
		};
		const grouped = new Map<string, string[]>();
		for (const tag of series?.metadata?.tags ?? []) {
			const label = labels[tag.group] ?? 'Other tags';
			grouped.set(label, [...(grouped.get(label) ?? []), tag.name]);
		}
		return [...grouped].map(([label, tags]) => ({ label, tags }));
	});

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

	let formBaseline = $state('');
	const formSnapshot = () => JSON.stringify([fTitle, fDescription, fType, fNsfw, fTags]);
	const metadataDirty = $derived(!!meta && formBaseline !== formSnapshot());
	beforeNavigate(({ cancel }) => {
		if (
			(metadataDirty || saving || editingKey !== null || newAltTitle.trim() || newTag.trim()) &&
			!confirm(
				'Leave this page? Unsaved edits will be lost; a save already in progress may still complete.',
			)
		)
			cancel();
	});
	function seedForm(m: SeriesAdminMeta): void {
		fTitle = m.titleOverride ?? '';
		fDescription = m.descriptionOverride ?? '';
		fType = m.contentTypeOverride ?? '';
		fNsfw = m.isNsfwOverride === null ? 'auto' : m.isNsfwOverride ? 'nsfw' : 'sfw';
		fTags = [...m.tags];
		tagsDirty = false;
		formBaseline = formSnapshot();
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
			const updated = await addSeriesAltTitle(series.id, t);
			series = await loadSeriesDetail(updated.id);
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
			const updated = await removeSeriesAltTitle(series.id, title);
			series = await loadSeriesDetail(updated.id);
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
			const updated = await saveSeriesMetadata(input);
			series = await loadSeriesDetail(updated.id);
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
		<p class="status" role="status">Loading…</p>
	{:else if loadError}
		<p class="status error" role="alert">{loadError}</p>
	{:else if series}
		<header class="head">
			{#if series.coverUrl}
				<img
					width="80"
					height="112"
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

		{#if series.metadata && (series.metadata.year || series.metadata.originalLanguage || series.metadata.demographic || series.metadata.contentRating || tagGroups.length || importedTrackers.length)}
			<section class="card source-details" aria-labelledby="source-details-title">
				<h2 id="source-details-title">Source details</h2>
				<p class="muted">
					Imported publication details and tracker links. Your metadata overrides take priority.
				</p>
				<dl class="source-facts">
					{#if series.metadata.year}<div>
							<dt>Publication year</dt>
							<dd>{series.metadata.year}</dd>
						</div>{/if}
					{#if series.metadata.originalLanguage}<div>
							<dt>Original language</dt>
							<dd>{series.metadata.originalLanguage}</dd>
						</div>{/if}
					{#if series.metadata.demographic}<div>
							<dt>Demographic</dt>
							<dd>{series.metadata.demographic}</dd>
						</div>{/if}
					{#if series.metadata.contentRating}<div>
							<dt>Content rating</dt>
							<dd>{series.metadata.contentRating}</dd>
						</div>{/if}
				</dl>
				{#each tagGroups as group (group.label)}
					<div class="source-tags">
						<h3>{group.label}</h3>
						<ul>
							{#each group.tags as tag (tag)}<li>{tag}</li>{/each}
						</ul>
					</div>
				{/each}
				{#if importedTrackers.length}
					<nav class="tracker-links" aria-label="External trackers">
						{#each importedTrackers as link (`${link.provider}:${link.externalId}`)}
							<a
								href={link.url}
								target="_blank"
								rel="noopener noreferrer"
								aria-label={`${trackerProviders[link.provider].label} (opens in a new tab)`}
								>{trackerProviders[link.provider].label} <span aria-hidden="true">↗</span></a
							>
						{/each}
					</nav>
				{/if}
			</section>
		{/if}

		{#if !meta?.workId}
			<p class="notice">
				This series isn't catalogued yet, so its metadata can't be edited. Add it to the canonical
				catalogue from the <a href="/">Catalog</a> page first.
			</p>
		{:else}
			<!-- ---- Metadata editor ---- -->
			<section class="card">
				<h2>Metadata</h2>
				<label class="field">
					<span>Title override</span>
					<input
						autocomplete="off"
						name="fTitle"
						type="text"
						bind:value={fTitle}
						placeholder={series.title}
					/>
					<small>Empty = use the source title.</small>
				</label>
				<label class="field">
					<span>Description override</span>
					<textarea
						autocomplete="off"
						name="fDescription"
						rows="5"
						bind:value={fDescription}
						placeholder="Use source description…"
					></textarea>
					<small>Empty = use the source description.</small>
				</label>
				<div class="row">
					<label class="field">
						<span>Type</span>
						<select autocomplete="off" name="fType" bind:value={fType}>
							<option value="">Auto (derived — {series.type})</option>
							{#each COMIC_TYPE_OPTIONS as t (t)}
								<option value={t}>{t}</option>
							{/each}
						</select>
					</label>
					<label class="field">
						<span>NSFW</span>
						<select autocomplete="off" name="fNsfw" bind:value={fNsfw}>
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
							aria-label="New tag"
							name="tag"
							autocomplete="off"
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
						{#if series.altTitles.length === 0}<span class="muted">No alternative titles</span>{/if}
					</div>
					<div class="tag-add">
						<input
							type="text"
							aria-label="Alternative title"
							name="alternative-title"
							autocomplete="off"
							bind:value={newAltTitle}
							placeholder="Add alt title…"
							disabled={altBusy}
							onkeydown={(e) => e.key === 'Enter' && (e.preventDefault(), addAlt())}
						/>
						<button type="button" onclick={addAlt} disabled={altBusy}>Add</button>
						{#if altMsg}<span class="msg">{altMsg}</span>{/if}
					</div>
					<small
						>A matching title and at least 90% similar author names can auto-merge duplicate
						entries.</small
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
									<input
										class="rename"
										aria-label="Chapter title"
										name="chapter-title"
										autocomplete="off"
										type="text"
										bind:value={editTitle}
									/>
									<button
										onclick={() =>
											commitChapter(c, {
												title: editTitle.trim() === '' ? null : editTitle.trim(),
											})}
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
		flex-wrap: wrap;
	}
	.tag-add input {
		flex: 1;
		min-width: 160px;
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

	@media (max-width: 720px) {
		.wrap {
			padding: 0.75rem;
		}
		.cover {
			width: 72px;
			height: 102px;
		}
		.head-meta h1 {
			font-size: 1.15rem;
			overflow-wrap: anywhere;
		}
		.head-meta {
			min-width: 0;
		}
		.card-head {
			flex-wrap: wrap;
			gap: 0.5rem;
		}
		.sources li,
		.chapters li {
			flex-wrap: wrap;
		}
		.ch-title {
			flex: 1 1 100%;
			min-width: 0;
			white-space: normal;
			overflow-wrap: anywhere;
		}
		.rename {
			flex: 1 1 100%;
			min-width: 0;
		}
		.actions {
			flex-wrap: wrap;
		}
		button {
			min-height: 40px;
		}
		.tag-x {
			min-height: 0;
			min-width: 32px;
			padding: 4px 8px;
		}
		.row .field {
			min-width: 0;
			flex: 1 1 100%;
		}
	}
	.source-details {
		gap: 18px;
	}
	.source-facts {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: 18px;
	}
	.source-facts dt,
	.source-tags h3 {
		color: var(--k-text-dim);
		font-size: 12px;
		font-weight: 600;
	}
	.source-facts dd {
		margin: 4px 0 0;
		overflow-wrap: anywhere;
		text-transform: capitalize;
	}
	.source-tags ul {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		margin: 8px 0 0;
		padding: 0;
		list-style: none;
	}
	.source-tags li {
		padding: 4px 9px;
		background: var(--k-surface-3);
		border-radius: var(--k-radius-sm);
		font-size: 12px;
	}
	.tracker-links {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
	}
	.tracker-links a {
		padding: 8px 12px;
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-md);
		font-size: 13px;
	}
	.tracker-links a:hover {
		background: var(--k-hover-fill);
		border-color: var(--k-border-strong);
	}
	@media (max-width: 640px) {
		.source-facts {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
