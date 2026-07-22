<script lang="ts">
	import type { Series, WorkSource } from '@komika/types';
	import type { AdminChapter, SeriesAdminMeta, SeriesMetadataInput } from '@komika/api';
	import { page } from '$app/state';
	import { afterNavigate, beforeNavigate } from '$app/navigation';
	import { assetSrc } from '$lib/config';
	import { confirm } from '$lib/confirm.svelte';
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
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';

	const id = $derived(page.params.id ?? '');
	let series = $state<Series | null>(null);

	// Where the admin came from (usually the Catalog with its search and page), so the
	// breadcrumb returns there instead of resetting the list. A full page load has no
	// `from`, and moving between two series keeps the earlier answer.
	let backHref = $state('/');
	afterNavigate(({ from }) => {
		if (!from?.route.id || from.route.id === page.route.id) return;
		backHref = from.url.pathname + from.url.search;
	});
	const backIsCatalog = $derived(backHref.split('?')[0] === '/');

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

	/** `ONGOING` reads as "Ongoing". Display only; the stored value is unchanged. */
	function enumLabel(value: string): string {
		return value.charAt(0) + value.slice(1).toLowerCase().replaceAll('_', ' ');
	}
	const statusTones: Record<string, 'success' | 'info' | 'warning' | 'danger'> = {
		ONGOING: 'success',
		COMPLETED: 'info',
		HIATUS: 'warning',
		CANCELLED: 'danger',
	};
	/** `ja` reads as "Japanese"; an unknown code is shown as it came. */
	function languageName(code: string): string {
		try {
			return new Intl.DisplayNames(['en'], { type: 'language' }).of(code) ?? code;
		} catch {
			return code;
		}
	}

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
	// The reset shares `saving` with the form save; this keeps the spinner on the
	// button that was actually pressed.
	let resettingTags = $state(false);

	// Monotonic token so a slow load(A) can't clobber a newer load(B) after fast nav.
	let loadSeq = 0;

	let formBaseline = $state('');
	const formSnapshot = () => JSON.stringify([fTitle, fDescription, fType, fNsfw, fTags]);
	const metadataDirty = $derived(!!meta && formBaseline !== formSnapshot());
	beforeNavigate(({ cancel }) => {
		// Must answer synchronously, so this one stays a native prompt.
		if (
			(metadataDirty || saving || editingKey !== null || newAltTitle.trim() || newTag.trim()) &&
			!window.confirm(
				'Leave this page? Unsaved edits will be lost. A save already in progress may still complete.',
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
		if (!id) return;
		// Going from one series to another keeps this component alive, so drafts typed
		// for the previous series must not show up (and save) under the new one.
		editingKey = null;
		editTitle = '';
		newAltTitle = '';
		newTag = '';
		load(id);
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

	async function addAlt(): Promise<void> {
		const t = newAltTitle.trim();
		if (!series || !t || altBusy) return;
		// The series this action was started on. Every await below can outlive a jump to
		// another series, and must then neither write to it nor paint over it.
		const here = id;
		const target = series.id;
		const ok = await confirm({
			title: 'Add this alternative title?',
			message:
				`"${t}" is saved right away.\n\n` +
				'If another work has the same title and its author names are at least 90% similar, the two works are merged into one. A merge cannot be undone.',
			confirmLabel: 'Add title',
		});
		if (!ok || id !== here || altBusy) return;
		altBusy = true;
		try {
			// Returns the recomputed series — reflects the new title AND any auto-merge of
			// other works that shared it.
			const updated = await addSeriesAltTitle(target, t);
			const fresh = await loadSeriesDetail(updated.id);
			toast.success('Alternative title added.');
			if (id !== here) return;
			series = fresh;
			newAltTitle = '';
		} catch (err) {
			toast.fromError(err, 'Failed to add the alternative title.');
		} finally {
			altBusy = false;
		}
	}

	async function removeAlt(title: string): Promise<void> {
		if (!series || altBusy) return;
		const here = id;
		const target = series.id;
		const ok = await confirm({
			title: 'Remove this alternative title?',
			message: `"${title}" is removed from this series right away. You can add it again later.`,
			confirmLabel: 'Remove title',
			danger: true,
		});
		if (!ok || id !== here || altBusy) return;
		altBusy = true;
		try {
			const updated = await removeSeriesAltTitle(target, title);
			const fresh = await loadSeriesDetail(updated.id);
			toast.success('Alternative title removed.');
			if (id !== here) return;
			series = fresh;
		} catch (err) {
			toast.fromError(err, 'Failed to remove the alternative title.');
		} finally {
			altBusy = false;
		}
	}

	async function persistMeta(input: SeriesMetadataInput, done: string): Promise<void> {
		if (!series) return;
		const here = id;
		saving = true;
		try {
			const updated = await saveSeriesMetadata(input);
			const fresh = await loadSeriesDetail(updated.id);
			// Re-pull the raw override state and RE-SEED the form so the fields reflect
			// exactly what persisted (server-side trims/dedupes included).
			const freshMeta = await loadSeriesAdminMeta(fresh.id);
			toast.success(done);
			// A late answer for the series we left must not replace the one now open.
			if (id !== here) return;
			series = fresh;
			meta = freshMeta;
			seedForm(freshMeta);
		} catch (err) {
			toast.fromError(err, 'Failed to save.');
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
		await persistMeta(input, 'Metadata saved.');
	}

	async function resetTagsToSource(): Promise<void> {
		if (!series || saving) return;
		const here = id;
		const target = series.id;
		const ok = await confirm({
			title: 'Reset tags to the source?',
			message:
				'The tag list saved for this series is discarded. The tags the source sends are used again.' +
				// The save re-seeds the whole form, so edits in the other fields go with it.
				(metadataDirty ? '\n\nYour unsaved changes in the other fields are discarded too.' : ''),
			confirmLabel: 'Reset tags',
			danger: true,
		});
		if (!ok || id !== here || saving) return;
		resettingTags = true;
		try {
			// null clears the curated set → genres derive from the source again.
			await persistMeta({ seriesId: target, tags: null }, 'Tags reset to the source.');
		} finally {
			resettingTags = false;
		}
	}

	// ---- per-source rescan ----
	let scanningKey = $state<string | null>(null);

	async function scanSource(src: WorkSource): Promise<void> {
		const here = id;
		const workId = meta?.workId;
		scanningKey = src.sourceKey;
		try {
			await triggerScan(src.sourceKey);
			toast.success(`Scanned ${extLabel(src)}.`);
			if (!workId) return;
			const fresh = await loadWorkChaptersAdmin(workId);
			if (id === here) chapters = fresh;
		} catch (err) {
			toast.fromError(err, 'Scan failed.');
		} finally {
			scanningKey = null;
		}
	}

	let rescanningAll = $state(false);
	async function scanAll(): Promise<void> {
		const workId = meta?.workId;
		if (!workId) return;
		const here = id;
		rescanningAll = true;
		try {
			const n = await rescanWork(workId);
			toast.success(`Re-scanned ${n} source${n === 1 ? '' : 's'}.`);
			const fresh = await loadWorkChaptersAdmin(workId);
			if (id === here) chapters = fresh;
		} catch (err) {
			toast.fromError(err, 'Rescan failed.');
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

	function startRename(c: AdminChapter): void {
		editingKey = c.key;
		editTitle = c.effectiveTitle ?? '';
	}

	async function commitChapter(
		c: AdminChapter,
		patch: { hidden?: boolean | null; title?: string | null },
	): Promise<void> {
		const workId = meta?.workId;
		if (!workId) return;
		const here = id;
		chapBusyKey = c.key;
		try {
			await setChapterOverride({ workId, chapterKey: c.key, ...patch });
			const fresh = await loadWorkChaptersAdmin(workId);
			if (id === here) {
				chapters = fresh;
				editingKey = null;
			}
			toast.success(
				'hidden' in patch
					? patch.hidden
						? `Chapter ${c.number} is hidden from readers.`
						: `Chapter ${c.number} is visible again.`
					: patch.title
						? `Chapter ${c.number} renamed.`
						: `Chapter ${c.number} uses the source title again.`,
			);
		} catch (err) {
			toast.fromError(err, 'Failed to update chapter.');
		} finally {
			chapBusyKey = null;
		}
	}

	function saveRename(c: AdminChapter): void {
		void commitChapter(c, { title: editTitle.trim() === '' ? null : editTitle.trim() });
	}
	function onRenameKey(e: KeyboardEvent, c: AdminChapter): void {
		if (e.key === 'Enter') {
			e.preventDefault();
			if (chapBusyKey !== c.key) saveRename(c);
		} else if (e.key === 'Escape') {
			editingKey = null;
		}
	}
</script>

<div class="k-page editor">
	<PageHeader
		title={series?.title ?? 'Series'}
		crumbs={[{ label: backIsCatalog ? 'Catalog' : 'Back', href: backHref }]}
	/>

	{#if loadError}
		<div class="k-notice" data-tone="danger" role="alert">
			<Icon name="alert" />
			<div class="k-notice__body load-error">
				<p class="load-error-title">This series could not be opened.</p>
				<p>{loadError}</p>
				{#if id.startsWith('w_')}
					<p>
						The editor opens a catalogue work only when it has a MangaDex source. A work that exists
						only on other sources cannot be edited here.
					</p>
				{/if}
				<div class="load-error-actions">
					<Button size="sm" icon="arrow-left" href={backHref}>
						{backIsCatalog ? 'Back to Catalog' : 'Go back'}
					</Button>
					<Button size="sm" icon="refresh" onclick={() => load(id)}>Retry</Button>
				</div>
			</div>
		</div>
	{:else}
		<LoadState {loading} loadingLabel="Loading series…" skeletonRows={5}>
			{#if series}
				<div class="summary">
					<Cover src={series.coverUrl ? assetSrc(series.coverUrl) : null} width={72} />
					<div class="summary-text">
						<div class="summary-badges">
							<span class="k-badge" data-tone="accent">{enumLabel(series.type)}</span>
							<span class="k-badge" data-tone={statusTones[series.status]}
								>{enumLabel(series.status)}</span
							>
							{#if series.isNsfw}<span class="k-badge" data-tone="danger">NSFW</span>{/if}
						</div>
						{#if series.author}<p class="summary-author">By {series.author}</p>{/if}
						<dl class="summary-ids">
							<div>
								<dt>Series ID</dt>
								<dd class="k-mono">{series.id}</dd>
							</div>
							{#if meta?.workId && meta.workId !== series.id}
								<div>
									<dt>Work ID</dt>
									<dd class="k-mono">{meta.workId}</dd>
								</div>
							{/if}
							<div>
								<dt>Chapters</dt>
								<dd class="k-num">{series.chapterCount}</dd>
							</div>
						</dl>
					</div>
				</div>

				<div class="sections">
					{#if series.metadata && (series.metadata.year || series.metadata.originalLanguage || series.metadata.demographic || series.metadata.contentRating || tagGroups.length || importedTrackers.length)}
						<section class="group" aria-labelledby="source-details-title">
							<div class="group-head">
								<div>
									<h2 id="source-details-title" class="group-title">Details from the source</h2>
									<p class="group-desc">
										Read only. This is what the source sent. Anything you set under Metadata is used
										instead.
									</p>
								</div>
							</div>
							<div class="group-body k-stack">
								<dl class="facts">
									{#if series.metadata.year}
										<dt>Publication year</dt>
										<dd class="k-num">{series.metadata.year}</dd>
									{/if}
									{#if series.metadata.originalLanguage}
										<dt>Original language</dt>
										<dd>{languageName(series.metadata.originalLanguage)}</dd>
									{/if}
									{#if series.metadata.demographic}
										<dt>Demographic</dt>
										<dd class="cap">{series.metadata.demographic}</dd>
									{/if}
									{#if series.metadata.contentRating}
										<dt>Content rating</dt>
										<dd class="cap">{series.metadata.contentRating}</dd>
									{/if}
									{#each tagGroups as group (group.label)}
										<dt>{group.label}</dt>
										<dd>{group.tags.join(', ')}</dd>
									{/each}
								</dl>
								{#if importedTrackers.length}
									<nav class="tracker-links" aria-label="External trackers">
										{#each importedTrackers as link (`${link.provider}:${link.externalId}`)}
											<Button
												size="sm"
												icon="external"
												href={link.url}
												target="_blank"
												rel="noopener noreferrer"
												aria-label={`${trackerProviders[link.provider].label} (opens in a new tab)`}
												>{trackerProviders[link.provider].label}</Button
											>
										{/each}
									</nav>
								{/if}
							</div>
						</section>
					{/if}

					{#if !meta?.workId}
						<section class="group">
							<div class="k-notice" data-tone="info">
								<Icon name="info" />
								<div class="k-notice__body">
									This series is not in the catalogue yet, so its metadata cannot be edited. Add it
									to the catalogue from the <a class="k-link" href="/">Catalog</a> page first.
								</div>
							</div>
						</section>
					{:else}
						<!-- ---- Metadata editor ---- -->
						<section class="group" aria-labelledby="metadata-title">
							<div class="group-head">
								<div>
									<h2 id="metadata-title" class="group-title">Metadata</h2>
									<p class="group-desc">
										An override is a value you type here. It replaces what the source sends. Leave a
										field empty, or choose Auto, to use the source value.
									</p>
								</div>
							</div>
							<!-- Locked while saving: the answer re-seeds every field, so anything typed
							     in the meantime would be thrown away without a warning. -->
							<div class="group-body form" inert={saving}>
								<label class="k-field">
									<span class="k-label">Title override</span>
									<input
										class="k-input"
										autocomplete="off"
										name="fTitle"
										type="text"
										bind:value={fTitle}
										placeholder={series.title}
									/>
								</label>
								<label class="k-field">
									<span class="k-label">Description override</span>
									<textarea
										class="k-textarea"
										autocomplete="off"
										name="fDescription"
										rows="6"
										bind:value={fDescription}
										placeholder="Leave empty to use the source description"
									></textarea>
								</label>

								<div class="setting">
									<div class="setting-text">
										<label class="setting-name" for="f-type">Type</label>
										<p class="k-hint">Auto works the type out from the source.</p>
									</div>
									<select
										id="f-type"
										class="k-select"
										autocomplete="off"
										name="fType"
										bind:value={fType}
									>
										<option value="">Auto (now {enumLabel(series.type)})</option>
										{#each COMIC_TYPE_OPTIONS as t (t)}
											<option value={t}>{enumLabel(t)}</option>
										{/each}
									</select>
								</div>
								<div class="setting">
									<div class="setting-text">
										<label class="setting-name" for="f-nsfw">NSFW</label>
										<p class="k-hint">
											An NSFW series is shown only to readers who turned on Show NSFW.
										</p>
									</div>
									<select
										id="f-nsfw"
										class="k-select"
										autocomplete="off"
										name="fNsfw"
										bind:value={fNsfw}
									>
										<option value="auto">Auto (now {series.isNsfw ? 'NSFW' : 'SFW'})</option>
										<option value="sfw">SFW</option>
										<option value="nsfw">NSFW</option>
									</select>
								</div>

								<div class="block" role="group" aria-labelledby="tags-title">
									<h3 id="tags-title" class="block-title">Tags</h3>
									{#if fTags.length === 0}
										<p class="k-hint">No tags.</p>
									{:else}
										<ul class="chips" aria-label="Tags">
											{#each fTags as tag (tag)}
												<li class="chip">
													<span>{tag}</span>
													<button
														type="button"
														class="chip-x"
														aria-label={`Remove tag ${tag}`}
														onclick={() => removeTag(tag)}><Icon name="x" size={12} /></button
													>
												</li>
											{/each}
										</ul>
									{/if}
									<div class="add-line">
										<label class="k-field">
											<span class="k-label">New tag</span>
											<input
												class="k-input"
												type="text"
												name="tag"
												autocomplete="off"
												bind:value={newTag}
												onkeydown={(e) => e.key === 'Enter' && (e.preventDefault(), addTag())}
											/>
										</label>
										<Button icon="plus" onclick={addTag}>Add tag</Button>
										{#if meta.hasCuratedTags}
											<Button
												variant="ghost"
												busy={resettingTags}
												disabled={saving}
												onclick={resetTagsToSource}>Reset to source</Button
											>
										{/if}
									</div>
									<p class="k-hint">
										{meta.hasCuratedTags
											? 'This list was saved by an admin. Tag changes are kept when you save metadata. Reset to source brings back the tags the source sends.'
											: 'These tags come from the source. If you change them and save metadata, your list replaces the source tags.'}
									</p>
								</div>

								<div class="form-foot">
									<Button
										variant="primary"
										busy={saving && !resettingTags}
										disabled={saving}
										onclick={saveMeta}
									>
										{saving && !resettingTags ? 'Saving…' : 'Save metadata'}
									</Button>
									{#if metadataDirty}<span class="k-hint">You have unsaved changes.</span>{/if}
								</div>

								<div class="block" role="group" aria-labelledby="alt-titles-title">
									<h3 id="alt-titles-title" class="block-title">Alternative titles</h3>
									<p class="k-hint">
										Other names this series is known by. Changes here are saved right away, not with
										Save metadata.
									</p>
									{#if series.altTitles.length === 0}
										<p class="k-hint">No alternative titles.</p>
									{:else}
										<ul class="chips" aria-label="Alternative titles">
											{#each series.altTitles as t (t)}
												<li class="chip">
													<span>{t}</span>
													<button
														type="button"
														class="chip-x"
														aria-label={`Remove alternative title ${t}`}
														disabled={altBusy}
														onclick={() => removeAlt(t)}><Icon name="x" size={12} /></button
													>
												</li>
											{/each}
										</ul>
									{/if}
									<div class="add-line">
										<label class="k-field">
											<span class="k-label">Alternative title</span>
											<input
												class="k-input"
												type="text"
												name="alternative-title"
												autocomplete="off"
												bind:value={newAltTitle}
												disabled={altBusy}
												onkeydown={(e) => e.key === 'Enter' && (e.preventDefault(), addAlt())}
											/>
										</label>
										<Button icon="plus" busy={altBusy} onclick={addAlt}>Add title</Button>
									</div>
									<p class="k-hint">
										If another work has the same title and its author names are at least 90%
										similar, adding the title merges the two works.
									</p>
								</div>
							</div>
						</section>

						<!-- ---- Sources / rescan ---- -->
						<section class="group" aria-labelledby="sources-title">
							<div class="group-head">
								<div>
									<h2 id="sources-title" class="group-title">Sources</h2>
									<p class="group-desc">
										The places this series is read from. A scan checks a source for new chapters now
										instead of waiting for the next scheduled check.
									</p>
								</div>
								<Button icon="refresh" busy={rescanningAll} onclick={scanAll}>
									{rescanningAll ? 'Re-scanning…' : 'Re-scan all sources'}
								</Button>
							</div>
							<div class="group-body">
								{#if sources.length === 0}
									<p class="k-hint">No source is linked to this series.</p>
								{:else}
									<ul class="source-list" aria-label="Sources">
										{#each sources as src (`${src.sourceType}:${src.sourceId}:${src.sourceKey}`)}
											<li>
												<div class="source-text">
													<span class="source-name">{extLabel(src)}</span>
													<span class="source-key">
														<!-- Without an extension the name above already is the source type. -->
														{#if src.extension}<span class="k-muted">{src.sourceType}</span>{/if}
														<span class="k-mono k-truncate" title={src.sourceKey}
															>{src.sourceKey}</span
														>
													</span>
												</div>
												{#if src.isNsfw}<span class="k-badge" data-tone="danger">NSFW</span>{/if}
												{#if src.sourceType === 'suwayomi'}
													<Button
														size="sm"
														busy={scanningKey === src.sourceKey}
														onclick={() => scanSource(src)}
													>
														{scanningKey === src.sourceKey ? 'Scanning…' : 'Scan now'}
													</Button>
												{/if}
											</li>
										{/each}
									</ul>
								{/if}
							</div>
						</section>

						<!-- ---- Chapters ---- -->
						<section class="group" aria-labelledby="chapters-title">
							<div class="group-head">
								<div>
									<h2 id="chapters-title" class="group-title">
										Chapters <span class="count k-num">{chapters.length}</span>
									</h2>
									<p class="group-desc">
										Rename a chapter or hide it from readers. A hidden chapter is not deleted and
										can be shown again. Save an empty title to use the source title.
									</p>
								</div>
							</div>
							<div class="group-body">
								<LoadState
									empty={chapters.length === 0}
									emptyTitle="No chapters yet"
									emptyHint="Chapters appear here after a source has been scanned."
									emptyIcon="book"
								>
									<div class="k-table" role="table" aria-label="Chapters">
										<div class="k-tr k-tr--head" role="row">
											<span role="columnheader">Chapter</span>
											<span role="columnheader">Title</span>
											<span role="columnheader">Sources</span>
											<span role="columnheader" class="k-td--end">Actions</span>
										</div>
										{#each chapters as c (c.key)}
											<div class="k-tr" class:is-hidden={c.hidden} role="row">
												<span class="ch-num k-num" role="cell" data-label="Chapter">{c.number}</span
												>
												{#if editingKey === c.key}
													<span class="ch-title" role="cell">
														<input
															class="k-input k-input--sm"
															aria-label="Chapter title"
															name="chapter-title"
															autocomplete="off"
															type="text"
															placeholder={c.sourceTitle ?? ''}
															bind:value={editTitle}
															onkeydown={(e) => onRenameKey(e, c)}
														/>
													</span>
													<span class="ch-src k-muted k-num" role="cell" data-label="Sources"
														>{c.sourceCount}</span
													>
													<span class="k-td--end" role="cell">
														<Button
															size="sm"
															variant="primary"
															busy={chapBusyKey === c.key}
															onclick={() => saveRename(c)}>Save</Button
														>
														<Button size="sm" variant="ghost" onclick={() => (editingKey = null)}
															>Cancel</Button
														>
													</span>
												{:else}
													<span class="ch-title" role="cell">
														<span class="ch-name" class:k-muted={!c.effectiveTitle}
															>{c.effectiveTitle ?? 'Untitled'}</span
														>
														{#if c.hidden}<span class="k-badge" data-tone="warning">Hidden</span
															>{/if}
														{#if c.titleOverride}<span class="k-badge">Renamed</span>{/if}
													</span>
													<span class="ch-src k-muted k-num" role="cell" data-label="Sources"
														>{c.sourceCount}</span
													>
													<span class="k-td--end ch-actions" role="cell">
														<Button
															size="sm"
															variant="ghost"
															icon="edit"
															onclick={() => startRename(c)}>Rename</Button
														>
														<Button
															size="sm"
															variant="ghost"
															icon={c.hidden ? 'eye' : 'eye-off'}
															busy={chapBusyKey === c.key}
															onclick={() => commitChapter(c, { hidden: !c.hidden })}
														>
															{c.hidden ? 'Unhide' : 'Hide'}
														</Button>
													</span>
												{/if}
											</div>
										{/each}
									</div>
								</LoadState>
							</div>
						</section>
					{/if}
				</div>
			{/if}
		</LoadState>
	{/if}
</div>

<style>
	/* A form page: a reading width, left-aligned in the content area. */
	.editor {
		max-width: 920px;
	}

	.load-error {
		display: grid;
		gap: var(--k-space-2);
	}
	.load-error-title {
		font-weight: 700;
	}
	.load-error-actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
		margin-top: var(--k-space-1);
	}

	/* ---- summary row under the header ---- */
	.summary {
		display: flex;
		align-items: flex-start;
		gap: var(--k-space-4);
		min-width: 0;
	}
	.summary-text {
		display: grid;
		gap: var(--k-space-2);
		min-width: 0;
	}
	.summary-badges {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.summary-author {
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
		overflow-wrap: anywhere;
	}
	.summary-ids {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-1) var(--k-space-5);
		font-size: var(--k-text-xs);
	}
	.summary-ids div {
		display: flex;
		align-items: baseline;
		gap: 6px;
		min-width: 0;
	}
	.summary-ids dt {
		color: var(--k-text-dim);
	}
	.summary-ids dd {
		color: var(--k-text-1);
		overflow-wrap: anywhere;
	}

	/* Sections are separated by a rule and space, as on Settings: the page reads as
	   one form and only the chapters table is boxed. */
	.sections {
		display: grid;
		min-width: 0;
	}
	.group {
		padding: var(--k-space-8) 0;
		border-top: 1px solid var(--k-border-1);
		min-width: 0;
	}
	.group:last-child {
		padding-bottom: 0;
	}
	.group-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3);
	}
	.group-title {
		font-size: var(--k-text-xl);
		font-weight: 700;
	}
	.group-title .count {
		margin-left: 4px;
		color: var(--k-text-dim);
		font-family: var(--k-font-sans);
		font-size: var(--k-text-md);
		font-weight: 600;
		letter-spacing: 0;
	}
	.group-desc {
		margin-top: 6px;
		max-width: 62ch;
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.group-body {
		margin-top: var(--k-space-5);
	}

	/* ---- read-only facts ---- */
	.facts {
		display: grid;
		grid-template-columns: 148px minmax(0, 1fr);
		gap: var(--k-space-2) var(--k-space-4);
		font-size: var(--k-text-sm);
	}
	.facts dt {
		color: var(--k-text-dim);
	}
	.facts dd {
		color: var(--k-text-1);
		overflow-wrap: anywhere;
	}
	.facts .cap {
		text-transform: capitalize;
	}
	.tracker-links {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}

	/* ---- metadata form ---- */
	.form {
		display: grid;
		gap: var(--k-space-5);
		min-width: 0;
	}
	.setting {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3) var(--k-space-6);
	}
	.setting-text {
		flex: 1 1 280px;
		min-width: 0;
		display: grid;
		gap: 4px;
	}
	.setting-name {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.setting .k-select {
		flex: 0 1 240px;
		width: auto;
	}
	.block {
		display: grid;
		gap: var(--k-space-3);
		min-width: 0;
		padding-top: var(--k-space-5);
		border-top: 1px solid var(--k-border);
	}
	.block-title {
		font-family: var(--k-font-sans);
		font-size: var(--k-text-md);
		font-weight: 700;
		letter-spacing: 0;
		color: var(--k-text-1);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		padding: 0;
		list-style: none;
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: 2px;
		max-width: 100%;
		min-height: 28px;
		padding: 2px 3px 2px 10px;
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface-4);
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		overflow-wrap: anywhere;
	}
	.chip-x {
		flex: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		border: 0;
		border-radius: var(--k-radius-sm);
		background: transparent;
		color: var(--k-text-dim);
		cursor: pointer;
		transition:
			background var(--k-dur) var(--k-ease),
			color var(--k-dur) var(--k-ease);
	}
	.chip-x:hover:not(:disabled) {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	.chip-x:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.add-line {
		display: flex;
		align-items: flex-end;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
	}
	.add-line .k-field {
		flex: 0 1 320px;
	}
	.form-foot {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-3);
		padding-top: var(--k-space-5);
		border-top: 1px solid var(--k-border);
	}

	/* ---- sources ---- */
	.source-list {
		padding: 0;
		list-style: none;
		border-top: 1px solid var(--k-border);
	}
	.source-list li {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		min-height: 56px;
		padding: var(--k-space-2) 0;
		border-bottom: 1px solid var(--k-border);
	}
	/* The section rule below closes the list. */
	.source-list li:last-child {
		border-bottom: 0;
	}
	.source-text {
		flex: 1 1 auto;
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.source-name {
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		font-weight: 700;
		overflow-wrap: anywhere;
	}
	.source-key {
		display: flex;
		align-items: baseline;
		gap: var(--k-space-2);
		min-width: 0;
		font-size: var(--k-text-xs);
	}

	/* ---- chapters ---- */
	.k-table {
		--cols: 64px minmax(0, 1fr) 64px 196px;
	}
	.ch-num {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.ch-title {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px var(--k-space-2);
	}
	.ch-name {
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.ch-title .k-input {
		flex: 1 1 auto;
	}
	.k-tr.is-hidden .ch-num,
	.k-tr.is-hidden .ch-name {
		color: var(--k-text-dim);
		font-weight: 400;
	}
	/* "Hide" and "Unhide" differ in width; a fixed slot keeps Rename in one column. */
	.ch-actions :global(.k-btn:last-child) {
		min-width: 88px;
		justify-content: flex-start;
	}
	.block > .k-hint {
		max-width: 72ch;
	}

	@media (max-width: 720px) {
		.facts {
			grid-template-columns: 124px minmax(0, 1fr);
		}
		.setting .k-select,
		.add-line .k-field {
			flex-basis: 100%;
		}
		/* A series can have hundreds of chapters, so the stacked phone row is kept
		   short: number and source count share the first line, then title, then actions. */
		.k-tr:not(.k-tr--head) {
			grid-template-columns: auto minmax(0, 1fr);
			column-gap: var(--k-space-5);
		}
		.k-tr > .ch-num,
		.k-tr > .ch-src {
			justify-content: flex-start;
			gap: var(--k-space-2);
		}
		.k-tr > .ch-src {
			grid-area: 1 / 2;
		}
		.k-tr > .ch-title,
		.k-tr > .k-td--end {
			grid-column: 1 / -1;
		}
	}
	@media (pointer: coarse) {
		.chip {
			min-height: 40px;
		}
		.chip-x {
			width: 36px;
			height: 36px;
		}
	}
</style>
