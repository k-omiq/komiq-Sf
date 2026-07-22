<script lang="ts">
	import type { ExtensionInfo } from '@komika/types';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { auth } from '$lib/auth.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import {
		installExtension,
		loadExtensions,
		setExtensionSubscription,
		uninstallExtension,
		updateExtension,
	} from '$lib/data';
	import { toast } from '$lib/toast.svelte';
	import { queueNavigate } from '$lib/url-state';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Segmented from '$lib/components/Segmented.svelte';
	import Switch from '$lib/components/Switch.svelte';

	// Auth redirect is centralized in +layout.svelte.

	type ExtFilter = 'all' | 'installed' | 'available';
	type ExtVerb = 'install' | 'uninstall' | 'update' | 'sync';

	let exts = $state<ExtensionInfo[]>([]);
	let extsLoading = $state(false);
	let extsLoaded = $state(false);
	let extsError = $state<string | null>(null);
	const extFilter = $derived<ExtFilter>(
		page.url.searchParams.get('extFilter') === 'installed'
			? 'installed'
			: page.url.searchParams.get('extFilter') === 'available'
				? 'available'
				: 'all',
	);
	const extLang = $derived(page.url.searchParams.get('extLang') ?? 'all');
	const extQuery = $derived(page.url.searchParams.get('extQuery') ?? '');
	/** pkgName of the row with an in-flight action (one at a time). */
	let busyPkg = $state<string | null>(null);
	/** Which action that row is running, so only its own control shows progress. */
	let busyVerb = $state<ExtVerb | null>(null);

	/** Extensions whose store icon failed to load — fall back to the placeholder. */
	let failedIcons = $state<Record<string, boolean>>({});

	function iconFailed(pkgName: string): void {
		failedIcons = { ...failedIcons, [pkgName]: true };
	}

	/** Bumped by every list load and every row change. A list that was requested before
	 * a row changed is older than that row, and must not be painted over it. */
	let extsSeq = 0;

	async function refreshExtensions(refresh = false): Promise<void> {
		const seq = ++extsSeq;
		extsLoading = true;
		extsError = null;
		try {
			const list = await loadExtensions(refresh);
			if (seq !== extsSeq) return;
			exts = list;
			extsLoaded = true;
		} catch (err) {
			extsError = err instanceof Error ? err.message : 'Failed to load extensions.';
			// Keep any already-listed extensions on a failed on-demand refresh; only
			// a failed initial load leaves the list empty. With rows still on screen
			// the failure is the outcome of pressing Refresh, so it goes to a toast.
			if (exts.length > 0) toast.error(extsError);
		} finally {
			extsLoaded = true; // A failed attempt waits for explicit retry.
			extsLoading = false;
		}
	}

	$effect(() => {
		if (!auth.user || extsLoaded || extsLoading) return;
		void refreshExtensions();
	});

	const extLangs = $derived([...new Set(exts.map((e) => e.lang))].sort());
	const filteredExts = $derived(
		exts.filter((e) => {
			if (extFilter === 'installed' && !e.isInstalled) return false;
			if (extFilter === 'available' && e.isInstalled) return false;
			if (extLang !== 'all' && e.lang !== extLang) return false;
			const q = extQuery.trim().toLowerCase();
			if (q && !e.name.toLowerCase().includes(q) && !e.pkgName.toLowerCase().includes(q))
				return false;
			return true;
		}),
	);
	const installedCount = $derived(exts.filter((e) => e.isInstalled).length);
	const filterOptions = $derived<{ value: ExtFilter; label: string; count: number }[]>([
		{ value: 'all', label: 'All', count: exts.length },
		{ value: 'installed', label: 'Installed', count: installedCount },
		{ value: 'available', label: 'Available', count: exts.length - installedCount },
	]);

	async function extAction(
		e: ExtensionInfo,
		verb: 'install' | 'uninstall' | 'update',
	): Promise<void> {
		if (busyPkg) return;
		// Uninstalling drops a source and invalidates the catalogue picker (reversible
		// only by reinstall + re-ingest), so gate it behind a confirm like other
		// consequential actions.
		if (
			verb === 'uninstall' &&
			!(await confirm({
				title: `Uninstall ${e.name}?`,
				message:
					'This removes its sources from the server. You will not be able to browse them until you install the extension again.',
				confirmLabel: 'Uninstall extension',
				danger: true,
			}))
		)
			return;
		if (busyPkg) return;
		busyPkg = e.pkgName;
		busyVerb = verb;
		try {
			const fn =
				verb === 'install'
					? installExtension
					: verb === 'uninstall'
						? uninstallExtension
						: updateExtension;
			const updated = await fn(e.pkgName);
			extsSeq++;
			exts = exts.map((x) => (x.pkgName === updated.pkgName ? updated : x));
			toast.success(
				verb === 'uninstall'
					? `Uninstalled ${e.name}.`
					: verb === 'update'
						? `Updated ${e.name} to ${updated.versionName}.`
						: `Installed ${e.name} ${updated.versionName}.`,
			);
		} catch (err) {
			toast.fromError(err, `Could not ${verb} ${e.name}.`);
		} finally {
			busyPkg = null;
			busyVerb = null;
		}
	}

	/**
	 * Toggle background source-sync for an extension. Subscribing makes the server
	 * periodically re-walk this extension's sources to auto-discover newly-added series
	 * (and keep enrolled series in the library so they keep updating); enabling also
	 * kicks an immediate pass server-side. Reuses `busyPkg` so a row runs one action.
	 */
	async function toggleSync(e: ExtensionInfo): Promise<void> {
		if (busyPkg) return;
		busyPkg = e.pkgName;
		busyVerb = 'sync';
		try {
			const next = await setExtensionSubscription(e.pkgName, !e.subscribed);
			extsSeq++;
			exts = exts.map((x) => (x.pkgName === e.pkgName ? { ...x, subscribed: next } : x));
			toast.success(
				next
					? `Background updates are on for ${e.name}. New series and chapters are picked up from now on.`
					: `Background updates are off for ${e.name}.`,
			);
		} catch (err) {
			toast.fromError(err, `Could not change background updates for ${e.name}.`);
		} finally {
			busyPkg = null;
			busyVerb = null;
		}
	}

	/** The browse route resolves the extension's sources itself. */
	function browseExtension(e: ExtensionInfo): void {
		void goto('/sources/browse?extension=' + encodeURIComponent(e.pkgName));
	}

	function clearFilters(): void {
		void queueNavigate(page.url, { extFilter: null, extLang: null, extQuery: null });
	}
</script>

<div class="k-page">
	<PageHeader
		title="Extensions"
		description="Extensions connect this server to sources, so install the ones you want to read from."
	>
		{#snippet actions()}
			{#if exts.length > 0}
				<Button
					icon="refresh"
					busy={extsLoading}
					onclick={() => refreshExtensions(true)}
					title="Fetch the repo lists again so versions and updates are current"
				>
					Refresh
				</Button>
			{/if}
		{/snippet}
	</PageHeader>

	<LoadState
		loading={extsLoading && exts.length === 0}
		error={exts.length === 0 ? extsError : null}
		empty={exts.length === 0}
		loadingLabel="Loading extensions. The first load fetches the repo lists."
		emptyTitle="No extension repo added yet"
		emptyHint="Extensions come from repos. Add a repo and its extensions are listed here to install."
		emptyIcon="package"
		onretry={() => refreshExtensions(true)}
	>
		{#snippet emptyActions()}
			<Button variant="primary" href="/settings#repos">Add a repo in Settings</Button>
		{/snippet}

		<div class="k-toolbar filters">
			<div class="k-field">
				<span class="k-label" aria-hidden="true">Show</span>
				<Segmented
					label="Filter by install state"
					value={extFilter}
					options={filterOptions}
					onchange={(v) => queueNavigate(page.url, { extFilter: v === 'all' ? null : v })}
				/>
			</div>
			<label class="k-field lang-filter">
				<span class="k-label">Language</span>
				<select
					class="k-select"
					autocomplete="off"
					name="extLang"
					value={extLang}
					onchange={(event) =>
						queueNavigate(page.url, {
							extLang: event.currentTarget.value === 'all' ? null : event.currentTarget.value,
						})}
					aria-label="Filter by language"
				>
					<option value="all">All languages</option>
					{#each extLangs as l (l)}
						<option value={l}>{l}</option>
					{/each}
				</select>
			</label>
			<label class="k-field k-grow">
				<span class="k-label">Name</span>
				<span class="k-search">
					<Icon name="search" />
					<input
						class="k-input"
						autocomplete="off"
						name="extQuery"
						type="search"
						placeholder="Name or package"
						value={extQuery}
						oninput={(event) =>
							queueNavigate(page.url, { extQuery: event.currentTarget.value || null }, true)}
						aria-label="Filter extensions by name"
					/>
				</span>
			</label>
		</div>

		{#if filteredExts.length === 0}
			<div class="k-empty">
				<Icon name="search" size={22} />
				<p class="k-empty__title">No extensions match these filters</p>
				<p>Try another name or language, or show all extensions.</p>
				<Button onclick={clearFilters}>Clear filters</Button>
			</div>
		{:else}
			<div
				class="k-table"
				class:is-dim={extsLoading}
				role="table"
				aria-label="Extensions"
				aria-busy={extsLoading}
			>
				<div class="k-tr k-tr--head" role="row">
					<span role="columnheader">Extension</span>
					<span role="columnheader">Language</span>
					<span role="columnheader">Version</span>
					<span role="columnheader">Background updates</span>
					<span class="k-td--end" role="columnheader">Actions</span>
				</div>
				{#each filteredExts as e (e.pkgName)}
					{@const rowBusy = busyPkg === e.pkgName}
					<div class="k-tr" role="row">
						<div class="ext" role="cell">
							{#if e.iconUrl && !failedIcons[e.pkgName]}
								<img
									width="32"
									height="32"
									class="icon"
									src={e.iconUrl}
									alt=""
									loading="lazy"
									referrerpolicy="no-referrer"
									onerror={() => iconFailed(e.pkgName)}
								/>
							{:else}
								<span class="icon placeholder"><Icon name="package" /></span>
							{/if}
							<div class="titles">
								<span class="line">
									<span class="name" title={e.name}>{e.name}</span>
									{#if e.isInstalled}<span class="k-badge" data-tone="success">Installed</span>{/if}
									{#if e.isNsfw}<span class="k-badge" data-tone="danger">NSFW</span>{/if}
								</span>
								<span class="pkg k-mono k-muted" title={e.pkgName}>{e.pkgName}</span>
							</div>
						</div>
						<span role="cell" data-label="Language">{e.lang}</span>
						<span role="cell" data-label="Version">
							<span class="version">
								<span class="k-mono">{e.versionName}</span>
								{#if e.hasUpdate}<span class="k-badge" data-tone="warning">Update available</span
									>{/if}
							</span>
						</span>
						{#if e.isInstalled}
							<span
								class="sync"
								role="cell"
								data-label="Background updates"
								title="Finds new series from this extension and keeps them updating"
							>
								<Switch
									checked={e.subscribed}
									label="Check {e.name} for new chapters in the background"
									disabled={busyPkg !== null}
									onchange={() => toggleSync(e)}
								/>
							</span>
						{:else}
							<!-- Nothing to switch until the extension is installed. -->
							<span class="sync none" role="cell"></span>
						{/if}
						<span class="k-td--end" role="cell">
							{#if e.isInstalled}
								<Button size="sm" onclick={() => browseExtension(e)}>Browse</Button>
								{#if e.hasUpdate}
									<Button
										size="sm"
										variant="primary"
										busy={rowBusy && busyVerb === 'update'}
										disabled={busyPkg !== null}
										onclick={() => extAction(e, 'update')}
									>
										Update
									</Button>
								{/if}
								<Button
									size="sm"
									variant="danger"
									busy={rowBusy && busyVerb === 'uninstall'}
									disabled={busyPkg !== null}
									onclick={() => extAction(e, 'uninstall')}
								>
									Uninstall
								</Button>
							{:else}
								<Button
									size="sm"
									variant="primary"
									busy={rowBusy && busyVerb === 'install'}
									disabled={busyPkg !== null}
									onclick={() => extAction(e, 'install')}
								>
									Install
								</Button>
							{/if}
						</span>
					</div>
				{/each}
			</div>
		{/if}
	</LoadState>
</div>

<style>
	.k-table {
		--cols: minmax(0, 2.8fr) 84px minmax(0, 1.7fr) 150px minmax(0, 2fr);
	}
	.filters {
		align-items: flex-end;
	}
	.lang-filter {
		flex: 0 1 180px;
	}
	.ext {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
	}
	.icon {
		flex: none;
		width: 32px;
		height: 32px;
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius);
		background: var(--k-surface-3);
		object-fit: cover;
	}
	.icon.placeholder {
		display: grid;
		place-items: center;
		color: var(--k-text-dim);
	}
	.titles {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.line {
		display: flex;
		align-items: center;
		gap: var(--k-space-2);
		min-width: 0;
	}
	.line .k-badge {
		flex: none;
	}
	.name {
		min-width: 0;
		overflow: hidden;
		color: var(--k-text-bright);
		font-weight: 700;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.pkg {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.version {
		display: inline-flex;
		align-items: center;
		flex-wrap: wrap;
		gap: 6px var(--k-space-2);
	}
	.sync {
		display: flex;
		align-items: center;
	}
	@media (max-width: 720px) {
		/* Stacked rows have the width to show a long name in full. */
		.ext {
			align-items: flex-start;
		}
		.line {
			flex-wrap: wrap;
		}
		.name {
			overflow-wrap: anywhere;
			white-space: normal;
		}
		.version {
			justify-content: flex-end;
		}
		.k-tr > .sync {
			align-items: center;
		}
		.k-tr > .sync.none {
			display: none;
		}
	}
</style>
