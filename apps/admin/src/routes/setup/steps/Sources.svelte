<script lang="ts">
	import { onMount } from 'svelte';
	import type { ExtensionInfo } from '@komika/types';
	import type { ExtensionRepo } from '@komika/api';
	import {
		addExtensionRepo,
		installExtension,
		isEngineUnreachable,
		loadExtensionRepos,
		loadExtensions,
		loadMangadexSetting,
		setMangadexEnabled,
	} from '$lib/data';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import Switch from '$lib/components/Switch.svelte';
	import { back, labels, next, stepNumber, wizard } from '../wizard.svelte';

	// B5: where the library comes from. Three optional parts, all skippable: the
	// MangaDex switch (applied on Continue), an extension repo (added at once), and
	// a picker to install a few extensions from it (installed at once).

	/** Rows shown before a search narrows the list; keeps a 1,000-extension repo light. */
	const LIST_LIMIT = 40;

	let mangadexPinned = $state(false);
	let mangadex = $state((wizard.extra.mangadex as boolean | undefined) ?? false);

	let repos = $state<ExtensionRepo[]>([]);
	let exts = $state<ExtensionInfo[]>([]);
	let loading = $state(true);
	/** The engine is down or still starting: say so once, calmly, and let them skip. */
	let engineDown = $state(false);

	let repoUrl = $state('');
	let repoBusy = $state(false);
	let repoError = $state<string | null>(null);

	let query = $state('');
	let lang = $state('all');
	let installing = $state<string | null>(null);
	let rowError = $state<{ pkg: string; message: string } | null>(null);
	let failedIcons = $state<Record<string, boolean>>({});

	let saveBusy = $state(false);
	let saveError = $state<string | null>(null);

	async function loadSources(): Promise<void> {
		try {
			repos = await loadExtensionRepos();
			exts = repos.length ? await loadExtensions(false) : [];
			engineDown = false;
			pickDefaultLang();
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else repoError = err instanceof Error ? err.message : 'Sources could not be loaded.';
		}
	}

	onMount(async () => {
		try {
			const s = await loadMangadexSetting();
			mangadexPinned = s.pinned;
			if (s.pinned || wizard.extra.mangadex === undefined) mangadex = s.enabled;
		} catch {
			/* An older server: the switch simply is not offered. */
			mangadexPinned = true;
		}
		await loadSources();
		loading = false;
	});

	/** Start the picker on the browser's language when the repo has it. */
	function pickDefaultLang(): void {
		if (lang !== 'all') return;
		const mine = (typeof navigator !== 'undefined' ? navigator.language : 'en').split('-')[0];
		if (exts.some((e) => e.lang === mine)) lang = mine;
	}

	const names = (() => {
		try {
			return new Intl.DisplayNames(['en'], { type: 'language' });
		} catch {
			return null;
		}
	})();

	function langName(code: string): string {
		if (code === 'all' || code === 'multi') return 'Several languages';
		try {
			return names?.of(code) ?? code;
		} catch {
			return code;
		}
	}

	const langs = $derived([...new Set(exts.map((e) => e.lang))].sort());
	const matching = $derived(
		exts.filter((e) => {
			if (lang !== 'all' && e.lang !== lang) return false;
			const q = query.trim().toLowerCase();
			return !q || e.name.toLowerCase().includes(q) || e.pkgName.toLowerCase().includes(q);
		}),
	);
	const shown = $derived(matching.slice(0, LIST_LIMIT));
	const repoCount = (indexUrl: string) => exts.filter((e) => e.repo === indexUrl).length;
	const installedCount = $derived(exts.filter((e) => e.isInstalled).length);

	async function addRepo(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		if (repoBusy) return;
		const url = repoUrl.trim();
		repoError = null;
		if (!/^https?:\/\//i.test(url)) {
			repoError = 'Enter the repo’s index address, starting with https://';
			return;
		}
		repoBusy = true;
		try {
			await addExtensionRepo(url);
			repoUrl = '';
			await loadSources();
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else repoError = err instanceof Error ? err.message : 'The repo could not be added.';
		} finally {
			repoBusy = false;
		}
	}

	async function install(ext: ExtensionInfo): Promise<void> {
		if (installing) return;
		installing = ext.pkgName;
		rowError = null;
		try {
			const updated = await installExtension(ext.pkgName);
			exts = exts.map((x) => (x.pkgName === updated.pkgName ? updated : x));
		} catch (err) {
			rowError = {
				pkg: ext.pkgName,
				message: err instanceof Error ? err.message : 'It could not be installed.',
			};
		} finally {
			installing = null;
		}
	}

	async function save(): Promise<void> {
		if (saveBusy) return;
		saveError = null;
		wizard.extra = { ...wizard.extra, mangadex, extensions: installedCount };
		if (mangadexPinned) {
			next();
			return;
		}
		saveBusy = true;
		try {
			mangadex = await setMangadexEnabled(mangadex);
			wizard.extra = { ...wizard.extra, mangadex };
			next();
		} catch (err) {
			saveError = err instanceof Error ? err.message : 'The setting could not be saved.';
		} finally {
			saveBusy = false;
		}
	}

	function skip(): void {
		wizard.extra = { ...wizard.extra, extensions: installedCount };
		next();
	}

	// Back never returns to the owner form: that account exists now.
	const previous = $derived(wizard.order[stepNumber('sources') - 2]?.id);
	const canGoBack = $derived(!!previous && previous !== 'owner');
</script>

<Onboarding
	step={stepNumber('sources')}
	steps={labels()}
	title="Sources"
	lead="No sources come with komiq. Pick where your library comes from, or skip and do it later in Settings."
>
	<div class="row">
		<div class="row-text">
			<p class="row-title">MangaDex</p>
			<p class="row-detail">
				When on, the server fetches the MangaDex catalogue, covers and chapter pages. When off, it
				never contacts MangaDex.
			</p>
			{#if mangadexPinned}
				<p class="row-detail">Set by this server’s configuration.</p>
			{/if}
		</div>
		<Switch
			checked={mangadex}
			label="Use MangaDex"
			disabled={mangadexPinned || saveBusy}
			onchange={(v) => (mangadex = v)}
		/>
	</div>

	<div class="ob-section">
		<p class="ob-section-title">Extension repo <span>Optional</span></p>
		{#if engineDown}
			<p class="k-hint ob-hint-row" role="status">
				<Icon name="info" size={14} />The source engine is not running yet. You can add repos and
				extensions later in Settings.
			</p>
		{:else}
			<form class="repo" onsubmit={addRepo} novalidate>
				<input
					class="k-input"
					type="url"
					inputmode="url"
					placeholder="https://example.org/index.min.json"
					aria-label="Repo index address"
					autocapitalize="none"
					spellcheck={false}
					disabled={loading}
					bind:value={repoUrl}
				/>
				<Button type="submit" variant="ghost" icon="plus" busy={repoBusy} disabled={loading}
					>Add</Button
				>
			</form>
			{#if repoError}<p class="k-error" role="alert">{repoError}</p>{/if}
			{#if repos.length}
				<ul class="ob-rows">
					{#each repos as r (r.indexUrl)}
						<li>
							<span class="ob-truncate" title={r.indexUrl}>{r.name}</span>
							{#if repoCount(r.indexUrl)}<span class="k-hint k-num"
									>{repoCount(r.indexUrl)} extensions</span
								>{/if}
						</li>
					{/each}
				</ul>
			{:else if !loading}
				<p class="k-hint">Paste the index address a repo publishes.</p>
			{/if}
		{/if}
	</div>

	{#if exts.length}
		<div class="ob-section">
			<p class="ob-section-title">
				Install extensions <span>{installedCount ? `${installedCount} installed` : 'Optional'}</span
				>
			</p>
			<div class="filters">
				<span class="k-search">
					<Icon name="search" />
					<input
						class="k-input"
						type="search"
						placeholder="Search extensions"
						aria-label="Search extensions"
						bind:value={query}
					/>
				</span>
				<select class="k-select" aria-label="Language" bind:value={lang}>
					<option value="all">All languages</option>
					{#each langs as l (l)}
						<option value={l}>{langName(l)}</option>
					{/each}
				</select>
			</div>
			{#if shown.length}
				<ul class="picker" aria-label="Extensions">
					{#each shown as e (e.pkgName)}
						<li>
							{#if e.iconUrl && !failedIcons[e.pkgName]}
								<img
									class="icon"
									src={e.iconUrl}
									alt=""
									width="28"
									height="28"
									loading="lazy"
									referrerpolicy="no-referrer"
									onerror={() => (failedIcons = { ...failedIcons, [e.pkgName]: true })}
								/>
							{:else}
								<span class="icon placeholder"><Icon name="package" size={16} /></span>
							{/if}
							<span class="ext">
								<span class="ob-truncate name">{e.name}</span>
								<span class="meta">
									{langName(e.lang)}{#if e.isNsfw}<span class="nsfw">NSFW</span>{/if}
								</span>
								{#if rowError?.pkg === e.pkgName}
									<span class="k-error row-error" role="alert">{rowError.message}</span>
								{/if}
							</span>
							{#if e.isInstalled}
								<span class="done"><Icon name="check" size={14} />Installed</span>
							{:else}
								<Button
									variant="ghost"
									size="sm"
									icon="download"
									busy={installing === e.pkgName}
									disabled={!!installing && installing !== e.pkgName}
									onclick={() => install(e)}>Install</Button
								>
							{/if}
						</li>
					{/each}
				</ul>
				{#if matching.length > shown.length}
					<p class="k-hint">
						Showing {shown.length} of {matching.length}. Search to find the rest.
					</p>
				{/if}
			{:else}
				<p class="k-hint">No extension matches. Try another name or language.</p>
			{/if}
		</div>
	{/if}

	{#if saveError}<p class="k-error" role="alert">{saveError}</p>{/if}

	{#snippet footer()}
		{#if canGoBack}
			<Button variant="ghost" icon="arrow-left" onclick={back} disabled={saveBusy}>Back</Button>
		{:else}
			<span></span>
		{/if}
		<span class="actions">
			<Button variant="ghost" onclick={skip} disabled={saveBusy}>Skip</Button>
			<Button variant="primary" busy={saveBusy} onclick={save}>Continue</Button>
		</span>
	{/snippet}
</Onboarding>

<style>
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-4);
		padding: 14px 16px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.row-text {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.row-text p {
		margin: 0;
	}
	.row-title {
		font-size: var(--k-text-md);
		font-weight: 600;
		color: var(--k-text-1);
	}
	.row-detail {
		font-size: var(--k-text-sm);
		line-height: 1.5;
		color: var(--k-text-dim);
	}
	.repo {
		display: grid;
		grid-template-columns: 1fr auto;
		gap: var(--k-space-2);
	}
	.filters {
		display: grid;
		grid-template-columns: 1fr auto;
		gap: var(--k-space-2);
	}
	.filters .k-select {
		max-width: 180px;
	}
	.picker {
		display: grid;
		gap: 2px;
		max-height: 320px;
		margin: 0;
		padding: 0;
		overflow-y: auto;
		list-style: none;
		border-radius: var(--k-radius);
	}
	.picker li {
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) auto;
		align-items: center;
		gap: var(--k-space-3);
		min-height: 48px;
		padding: 6px 10px 6px 12px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
	}
	.icon {
		width: 28px;
		height: 28px;
		border-radius: 6px;
		object-fit: cover;
	}
	.placeholder {
		display: grid;
		place-items: center;
		background: var(--k-surface-4);
		color: var(--k-text-dim);
	}
	.ext {
		display: grid;
		gap: 1px;
		min-width: 0;
	}
	.name {
		font-size: var(--k-text-sm);
		font-weight: 600;
		color: var(--k-text-1);
	}
	.meta {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: var(--k-text-xs);
		color: var(--k-text-dim);
	}
	.nsfw {
		color: var(--k-danger);
		font-weight: 600;
	}
	.row-error {
		font-size: var(--k-text-xs);
	}
	.done {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		padding-right: 6px;
		font-size: var(--k-text-xs);
		color: var(--k-text-dim);
	}
	.actions {
		display: inline-flex;
		gap: var(--k-space-2);
	}
	@media (max-width: 420px) {
		.filters {
			grid-template-columns: 1fr;
		}
		.filters .k-select {
			max-width: none;
		}
	}
</style>
