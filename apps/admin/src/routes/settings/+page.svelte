<script lang="ts">
	import { href } from '$lib/paths';
	import type { ExtensionAutoUpdate, ExtensionInfo } from '@komika/types';
	import type { ExtensionRepo, MangadexSetting } from '@komika/api';
	import { auth, toggleNsfwVisibility } from '$lib/auth.svelte';
	import { config, serverAddress, target } from '$lib/config';
	import { confirm } from '$lib/confirm.svelte';
	import {
		addExtensionRepo,
		isEngineUnreachable,
		loadExtensionRepos,
		loadMangadexSetting,
		removeExtensionRepo,
		setMangadexEnabled,
		consolidateDuplicates,
		loadExtensionAutoUpdate,
		loadExtensions,
		materializeCatalogueCovers,
		persistCatalogue,
		runExtensionUpdates,
		setRegistrationOpen,
	} from '$lib/data';
	import { instance, loadInstance } from '$lib/instance.svelte';
	import { setTheme, theme, type ThemePref } from '$lib/theme.svelte';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Segmented from '$lib/components/Segmented.svelte';
	import Switch from '$lib/components/Switch.svelte';

	const sections = $derived([
		{ id: 'appearance', label: 'Appearance' },
		{ id: 'content', label: 'Content' },
		// Only on a server that reports its settings (`instance`).
		...(instance.info ? [{ id: 'accounts', label: 'Accounts' }] : []),
		{ id: 'sources', label: 'Sources' },
		{ id: 'updates', label: 'Extension updates' },
		{ id: 'maintenance', label: 'Maintenance' },
		{ id: 'about', label: 'About' },
	]);

	function shortDate(iso: string | null): string {
		if (!iso) return 'Never';
		const d = new Date(iso);
		return Number.isNaN(d.getTime()) ? 'Never' : d.toISOString().slice(0, 16).replace('T', ' ');
	}

	// ---- Content: the admin's own show_nsfw flag ------------------------------
	// An ACCOUNT preference, not a console filter: with it off the server refuses
	// source browsing, extension install and source ingest for this account. The
	// console's own lists (Catalog, Latest updates) read with a per-request
	// `includeNsfw` override, so mis-flagged works stay reachable either way.
	let nsfwBusy = $state(false);

	async function onToggleNsfw(next: boolean): Promise<void> {
		if (!auth.user || nsfwBusy) return;
		nsfwBusy = true;
		try {
			await toggleNsfwVisibility(next);
			toast.success(next ? 'NSFW content is on.' : 'NSFW content is off.');
		} catch (err) {
			toast.fromError(err, 'Failed to change NSFW visibility.');
		} finally {
			nsfwBusy = false;
		}
	}

	// ---- Accounts: the sign-up switch -------------------------------------------
	// Read from the public `instance` facts (self-host builds already loaded them at
	// boot; the hosted console asks here). A server that predates them hides the
	// section. The server refuses the change when its environment pins the value.
	$effect(() => {
		if (!instance.ready) void loadInstance();
	});
	let signupBusy = $state(false);
	let signupPinned = $state<string | null>(null);

	async function onToggleSignup(next: boolean): Promise<void> {
		if (!instance.info || signupBusy) return;
		signupBusy = true;
		try {
			const open = await setRegistrationOpen(next);
			instance.info = { ...instance.info, registrationOpen: open };
			toast.success(open ? 'Anyone can sign up now.' : 'Sign-up is closed.');
		} catch (err) {
			const message = err instanceof Error ? err.message : '';
			if (message.includes('KOMIKA_REGISTRATION_OPEN')) signupPinned = message;
			else toast.fromError(err, 'The setting could not be saved.');
		} finally {
			signupBusy = false;
		}
	}

	// ---- Sources: the MangaDex switch ------------------------------------------
	// Off on a new self-hosted install. The server refuses the change when its
	// environment pins it (`pinnedBy`); the switch is then read-only.
	let mangadex = $state<MangadexSetting | null>(null);
	let mangadexBusy = $state(false);

	async function refreshMangadex(): Promise<void> {
		try {
			mangadex = await loadMangadexSetting();
		} catch {
			mangadex = null; // An older server: the row is hidden.
		}
	}

	async function onToggleMangadex(next: boolean): Promise<void> {
		if (!mangadex || mangadexBusy) return;
		mangadexBusy = true;
		try {
			const enabled = await setMangadexEnabled(next);
			mangadex = { ...mangadex, enabled };
			if (instance.info) instance.info = { ...instance.info, mangadexEnabled: enabled };
			toast.success(enabled ? 'MangaDex is on.' : 'MangaDex is off.');
		} catch (err) {
			toast.fromError(err, 'The setting could not be saved.');
			await refreshMangadex();
		} finally {
			mangadexBusy = false;
		}
	}

	// ---- Sources: extension repos ---------------------------------------------
	// The engine keeps the repo list; extension counts are read off its extension list.
	let repoList = $state<ExtensionRepo[]>([]);
	let exts = $state<ExtensionInfo[]>([]);
	let extsLoading = $state(false);
	let extsError = $state<string | null>(null);
	/** The engine is down or still starting: a calm note, not an error wall. */
	let engineDown = $state(false);
	let repoUrl = $state('');
	let addingRepo = $state(false);
	let removingRepo = $state<string | null>(null);

	const repos = $derived(
		repoList.map((r) => ({ ...r, count: exts.filter((e) => e.repo === r.indexUrl).length })),
	);

	async function refreshRepos(): Promise<void> {
		extsLoading = true;
		extsError = null;
		try {
			repoList = await loadExtensionRepos();
			exts = repoList.length ? await loadExtensions(false) : [];
			engineDown = false;
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else extsError = err instanceof Error ? err.message : 'Failed to load extension repos.';
		} finally {
			extsLoading = false;
		}
	}

	async function onAddRepo(ev: SubmitEvent): Promise<void> {
		ev.preventDefault();
		if (addingRepo) return;
		addingRepo = true;
		try {
			const count = await addExtensionRepo(repoUrl.trim());
			toast.success(`Repo added. ${count} extension${count === 1 ? '' : 's'} known.`);
			repoUrl = '';
			await refreshRepos();
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else toast.fromError(err, 'Failed to add the repo.');
		} finally {
			addingRepo = false;
		}
	}

	async function onRemoveRepo(r: ExtensionRepo): Promise<void> {
		if (removingRepo) return;
		const ok = await confirm({
			title: `Remove ${r.name}?`,
			message:
				'Its extensions are no longer offered for install or updates. Series you already added stay in the catalog.',
			confirmLabel: 'Remove repo',
			danger: true,
		});
		if (!ok) return;
		removingRepo = r.indexUrl;
		try {
			await removeExtensionRepo(r.indexUrl);
			toast.success(`Removed ${r.name}.`);
			await refreshRepos();
		} catch (err) {
			if (isEngineUnreachable(err)) engineDown = true;
			else toast.fromError(err, 'Failed to remove the repo.');
		} finally {
			removingRepo = null;
		}
	}

	// ---- Extension updater ----------------------------------------------------
	// The server updates installed extensions on a timer; this is its status and
	// history, plus a way to run it now.
	let autoUpdate = $state<ExtensionAutoUpdate | null>(null);
	let autoUpdateError = $state<string | null>(null);
	let autoUpdateStarting = $state(false);
	/** Bumped by every poll loop so a newer one supersedes an older one. */
	let autoUpdatePoll = 0;
	/** Set when the page is left. Nothing may start, or carry on, after that: the
	 * controls that would stop it are gone. */
	let left = false;

	async function refreshAutoUpdate(): Promise<void> {
		try {
			autoUpdate = await loadExtensionAutoUpdate();
			autoUpdateError = null;
		} catch (err) {
			autoUpdateError = err instanceof Error ? err.message : 'Failed to load update status.';
		}
	}

	/** Start a pass, then follow it until it finishes (it runs server-side, and each
	 * extension is a download, so it outlasts the request that started it). */
	async function runUpdatesNow(): Promise<void> {
		autoUpdateStarting = true;
		autoUpdateError = null;
		try {
			autoUpdate = await runExtensionUpdates();
		} catch (err) {
			autoUpdateError = err instanceof Error ? err.message : 'Failed to start the update.';
			return;
		} finally {
			autoUpdateStarting = false;
		}
		await followUpdates();
	}

	/** Re-read the status until the running pass ends. Also started on load, because a
	 * pass the server began on its own timer would otherwise leave "Update all now"
	 * disabled until a reload. */
	async function followUpdates(): Promise<void> {
		if (left) return;
		const poll = ++autoUpdatePoll;
		// ~5 minutes at most; a pass that long is stuck, and a reload still works.
		for (let i = 0; i < 75 && autoUpdate?.running && poll === autoUpdatePoll; i++) {
			await new Promise((r) => setTimeout(r, 4000));
			if (poll !== autoUpdatePoll) return;
			await refreshAutoUpdate();
		}
	}

	function everyLabel(seconds: number): string {
		if (seconds % 3600 === 0) return `${seconds / 3600} h`;
		if (seconds % 60 === 0) return `${seconds / 60} min`;
		return `${seconds} s`;
	}

	$effect(() => {
		if (!auth.user) return;
		void refreshRepos();
		void refreshMangadex();
		void refreshAutoUpdate().then(followUpdates);
	});
	// Stop following a running update pass, and stop a merge run between batches, when
	// the page is left.
	$effect(() => () => {
		left = true;
		autoUpdatePoll++;
	});

	// ---- Maintenance: save the whole catalogue --------------------------------
	// Materializes the entire Suwayomi library into the DB read-cache the reader
	// reads from (metadata synchronously; chapter lists in a server background
	// task). Heavy on a large catalogue, so it asks first. Execution is gated
	// server-side by require_admin.
	let persisting = $state(false);
	let persistResult = $state<{ ok: boolean; text: string } | null>(null);

	async function runPersistCatalogue(): Promise<void> {
		if (persisting) return;
		const ok = await confirm({
			title: 'Save the whole catalogue now?',
			message:
				'Every series from the source engine is copied into the database. This is slow on a large catalogue. You can run it again safely.',
			confirmLabel: 'Save catalogue',
		});
		if (!ok) return;
		persisting = true;
		persistResult = null;
		try {
			const n = await persistCatalogue();
			persistResult = {
				ok: true,
				text: `Saved ${n} series. Chapter lists are filling in the background.`,
			};
		} catch (err) {
			persistResult = {
				ok: false,
				text: err instanceof Error ? err.message : 'Failed to save the catalogue.',
			};
		} finally {
			persisting = false;
		}
	}

	// ---- Maintenance: store covers in the DB ----------------------------------
	// Crawls the catalogue, fetches each still-uncached work's cover and stores a
	// bounded WebP in the DB so the reader serves covers from /covers/{id}.webp.
	// The returned number is how many works were still uncached at kick-off.
	let coversRunning = $state(false);
	let coversResult = $state<{ ok: boolean; text: string } | null>(null);

	async function runMaterializeCovers(): Promise<void> {
		if (coversRunning) return;
		const ok = await confirm({
			title: 'Download all missing covers now?',
			message:
				'The server fetches every cover it has not stored yet. It runs slowly in the background. You can run it again safely.',
			confirmLabel: 'Download covers',
		});
		if (!ok) return;
		coversRunning = true;
		coversResult = null;
		try {
			const n = await materializeCatalogueCovers();
			coversResult = {
				ok: true,
				text: `Started for ${n} work${n === 1 ? '' : 's'} with no stored cover. Progress shows in the server logs.`,
			};
		} catch (err) {
			coversResult = {
				ok: false,
				text: err instanceof Error ? err.message : 'Failed to start the cover download.',
			};
		} finally {
			coversRunning = false;
		}
	}

	// ---- Maintenance: bulk consolidation (destructive) ------------------------
	// One batch per server call: waiting reviews first, then groups of works that share
	// a title. The run is BOUNDED: a confirm gates the first batch, MAX_BATCHES caps a
	// single run, and Stop halts it between batches. The server merges a pair only on
	// evidence, closes pairs that are two different works, and leaves real questions in
	// Duplicates. A merge is irreversible, so the run stays attended by design.
	const BATCH_SIZE = 200;
	const MAX_BATCHES = 80;

	let consolidating = $state(false);
	let stopRequested = $state(false);
	let consolidateMsg = $state<string | null>(null);
	let batchesDone = $state(0);

	async function consolidate(): Promise<void> {
		if (consolidating) return;
		const ok = await confirm({
			title: 'Merge duplicate works?',
			message:
				`This sorts every pair of works that share a title or an alternative title. Articles (the, a, an) are ignored, so "The X" matches "X".\n\n` +
				`A pair is merged when the author, description, cover or original-language title also matches, or when the shared title is long enough to mean one series. Two MangaDex entries merge only on the same title and the same author. A novel, a sequel, a promo or a work in another language stays separate.\n\n` +
				`Pairs that are clearly two different works are closed. Unclear pairs stay in Duplicates, likeliest first.\n\n` +
				`Each merge permanently deletes the merged work. There is no undo. You can stop the run between batches.`,
			confirmLabel: 'Merge duplicates',
			danger: true,
		});
		if (!ok) return;
		consolidating = true;
		stopRequested = false;
		batchesDone = 0;
		consolidateMsg = 'Starting…';
		let merged = 0;
		let dismissed = 0;
		let pending: number | null = null;
		let stopped = false;
		let finished = false;
		const count = (n: number, noun: string) => `${n} ${noun}${n === 1 ? '' : 's'}`;
		const summary = () =>
			`Merged ${count(merged, 'work')}. Closed ${count(dismissed, 'pair')} as different works.` +
			(pending === null ? '' : ` ${count(pending, 'pair')} left to review.`);
		try {
			for (let i = 0; i < MAX_BATCHES; i++) {
				// `left`: each batch deletes works for good, so a run nobody can see or stop
				// any more ends with the batch in flight.
				if (stopRequested || left) {
					stopped = true;
					break;
				}
				const batch = await consolidateDuplicates(BATCH_SIZE);
				batchesDone += 1;
				merged += batch.merged;
				dismissed += batch.dismissed;
				pending = batch.pending;
				consolidateMsg = `Batch ${batchesDone} done. ${summary()}`;
				if (batch.done) {
					finished = true;
					break;
				}
			}
			consolidateMsg = stopped
				? `Stopped. ${summary()}`
				: finished
					? `Done. ${summary()}`
					: `Batch limit reached. ${summary()} Run again to continue.`;
		} catch (err) {
			const why = err instanceof Error ? err.message : 'Merging failed.';
			consolidateMsg = `${why} Before the failure: ${summary()}`;
		} finally {
			consolidating = false;
			stopRequested = false;
		}
	}
</script>

<div class="k-page">
	<PageHeader
		title="Settings"
		description="Your preferences, extension repos, and jobs that act on the whole instance."
	/>

	<div class="layout">
		<nav class="toc" aria-label="Settings sections">
			{#each sections as s (s.id)}
				<a href="#{s.id}">{s.label}</a>
			{/each}
		</nav>

		<div class="sections">
			<section id="appearance" class="group" aria-labelledby="appearance-h">
				<div class="group-head">
					<div>
						<h2 id="appearance-h" class="group-title">Appearance</h2>
						<p class="group-desc">Saved in this browser only.</p>
					</div>
				</div>
				<div class="group-body">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Theme</p>
							<p class="k-hint">System follows your device setting.</p>
						</div>
						<Segmented
							label="Theme"
							value={theme.pref}
							options={[
								{ value: 'dark', label: 'Dark' },
								{ value: 'light', label: 'Light' },
								{ value: 'system', label: 'System' },
							]}
							onchange={(v: ThemePref) => setTheme(v)}
						/>
					</div>
				</div>
			</section>

			<section id="content" class="group" aria-labelledby="content-h">
				<div class="group-head">
					<div>
						<h2 id="content-h" class="group-title">Content</h2>
						<p class="group-desc">Applies to your account, here and in the reader.</p>
					</div>
				</div>
				<div class="group-body">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Show NSFW content</p>
							<p class="k-hint">
								When this is off, the server blocks source browsing, extension install, and adding
								series from sources for your account. Catalog and Latest updates still list NSFW
								works, so you can fix wrong labels.
							</p>
						</div>
						<Switch
							label="Show NSFW content"
							checked={auth.user?.showNsfw ?? false}
							disabled={nsfwBusy}
							onchange={onToggleNsfw}
						/>
					</div>
				</div>
			</section>

			{#if instance.info}
				<section id="accounts" class="group" aria-labelledby="accounts-h">
					<div class="group-head">
						<div>
							<h2 id="accounts-h" class="group-title">Accounts</h2>
							<p class="group-desc">Who can get an account on this server.</p>
						</div>
					</div>
					<div class="group-body">
						<div class="setting">
							<div class="setting-text">
								<p class="setting-name">Anyone can sign up</p>
								{#if signupPinned}
									<p class="k-hint">{signupPinned}</p>
								{:else}
									<p class="k-hint">
										When this is off, the reader hides sign-up and only people you add on the <a
											class="k-link"
											href={href('/users')}>Users</a
										> page can sign in.
									</p>
								{/if}
							</div>
							<Switch
								label="Anyone can sign up"
								checked={instance.info.registrationOpen}
								disabled={signupBusy || !!signupPinned}
								onchange={onToggleSignup}
							/>
						</div>
					</div>
				</section>
			{/if}

			<section id="sources" class="group" aria-labelledby="sources-h">
				<div class="group-head">
					<div>
						<h2 id="sources-h" class="group-title">Sources</h2>
						<p class="group-desc">
							Where the library comes from. No source is built in: turn on MangaDex, add extension
							repos, or both.
						</p>
					</div>
				</div>
				<div class="group-body jobs">
					{#if mangadex}
						<div class="setting">
							<div class="setting-text">
								<p class="setting-name">MangaDex</p>
								<p class="k-hint">
									When on, the server fetches the MangaDex catalogue, covers and chapter pages. When
									off, it never contacts MangaDex.
								</p>
								{#if mangadex.pinned}
									<p class="k-hint">
										Set by <code>{mangadex.pinnedBy ?? 'the server configuration'}</code> in the server
										environment.
									</p>
								{/if}
							</div>
							<Switch
								label="MangaDex"
								checked={mangadex.enabled}
								disabled={mangadexBusy || mangadex.pinned}
								onchange={onToggleMangadex}
							/>
						</div>
					{/if}

					<div id="repos" class="setting repos">
						<div class="setting-text">
							<p class="setting-name">Extension repos</p>
							<p class="k-hint">
								A repo is a list of extensions the engine can install. Add the index address each
								repo publishes.
							</p>
						</div>
						{#if engineDown}
							<p class="k-hint calm" role="status">
								<Icon name="info" />
								<span
									>The source engine is not reachable yet. It may still be starting.
									<button class="k-link retry" type="button" onclick={refreshRepos}
										>Try again</button
									></span
								>
							</p>
						{:else}
							<form class="repo-form" onsubmit={onAddRepo}>
								<input
									class="k-input"
									type="url"
									name="repository"
									autocomplete="off"
									aria-label="Repo index address"
									bind:value={repoUrl}
									placeholder="https://example.org/extensions/index.min.json"
									required
								/>
								<Button type="submit" variant="primary" icon="plus" busy={addingRepo}
									>Add repo</Button
								>
							</form>

							{#if extsError}
								<p class="k-error" role="alert">
									{extsError}
									<button class="k-link retry" type="button" onclick={refreshRepos}
										>Try again</button
									>
								</p>
							{:else if repos.length > 0}
								<ul class="repo-list" aria-label="Extension repos">
									{#each repos as r (r.indexUrl)}
										<li>
											<span class="repo-text">
												<span class="repo-name">{r.name}</span>
												<span class="k-mono k-truncate k-muted" title={r.indexUrl}
													>{r.indexUrl}</span
												>
											</span>
											{#if r.count}
												<span class="k-hint k-num"
													>{r.count} extension{r.count === 1 ? '' : 's'}</span
												>
											{/if}
											<Button
												variant="ghost"
												size="sm"
												icon="trash"
												iconOnly
												aria-label="Remove {r.name}"
												busy={removingRepo === r.indexUrl}
												disabled={!!removingRepo && removingRepo !== r.indexUrl}
												onclick={() => onRemoveRepo(r)}
											/>
										</li>
									{/each}
								</ul>
							{:else if extsLoading}
								<p class="k-hint">Loading repos…</p>
							{:else}
								<p class="k-hint">No repo added yet. Extensions appear after you add one.</p>
							{/if}
						{/if}
					</div>
				</div>
				<div class="group-foot">
					<a class="k-link" href={href('/sources')}>Manage installed extensions</a>
				</div>
			</section>

			<section id="updates" class="group" aria-labelledby="updates-h">
				<div class="group-head">
					<div>
						<h2 id="updates-h" class="group-title">Extension updates</h2>
						<p class="group-desc">
							A source that stops working is often one extension release behind. The server can
							update installed extensions on a timer.
						</p>
					</div>
					<Button
						icon="refresh"
						busy={autoUpdateStarting || !!autoUpdate?.running}
						onclick={runUpdatesNow}
					>
						{autoUpdate?.running ? 'Updating…' : 'Update all now'}
					</Button>
				</div>
				<div class="group-body k-stack">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Automatic updates</p>
							<p class="k-hint">
								{#if autoUpdate && !autoUpdate.enabled}
									The server does not update extensions on its own. Set
									<code>EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS</code> on the server to turn this on.
								{:else}
									The interval is set on the server.
								{/if}
							</p>
						</div>
						{#if !autoUpdate}
							<span class="k-badge">{autoUpdateError ? 'Unknown' : 'Loading…'}</span>
						{:else if !autoUpdate.enabled}
							<span class="k-badge" data-tone="warning">Off</span>
						{:else}
							<span class="k-badge" data-tone="success"
								>On, every {everyLabel(autoUpdate.intervalSeconds)}</span
							>
						{/if}
					</div>

					{#if autoUpdateError}
						<div class="k-notice" data-tone="danger" role="alert">
							<Icon name="alert" />
							<div class="k-notice__body">{autoUpdateError}</div>
						</div>
					{/if}

					{#if autoUpdate}
						{#if autoUpdate.lastError}
							<div class="k-notice" data-tone="danger" role="alert">
								<Icon name="alert" />
								<div class="k-notice__body">
									Last run {shortDate(autoUpdate.lastRunAt)} failed: {autoUpdate.lastError}
								</div>
							</div>
						{:else if autoUpdate.lastRunAt}
							<p class="k-hint">
								Last run {shortDate(autoUpdate.lastRunAt)}.
								{#if autoUpdate.lastUpdated === 0 && autoUpdate.lastFailed === 0}
									Everything was up to date.
								{:else}
									{autoUpdate.lastUpdated} updated{#if autoUpdate.lastFailed > 0}, <span class="bad"
											>{autoUpdate.lastFailed} failed</span
										>{/if}.
								{/if}
							</p>
						{:else if !autoUpdate.running}
							<p class="k-hint">No run yet since the server started.</p>
						{/if}

						{#if autoUpdate.recent.length > 0}
							<details class="history">
								<summary>Update history ({autoUpdate.recent.length})</summary>
								<ul>
									{#each autoUpdate.recent as e (e.at + e.pkgName)}
										<li class:bad={!!e.error}>
											<span class="k-muted k-num">{shortDate(e.at)}</span>
											<span class="k-mono k-truncate">{e.pkgName}</span>
											{#if e.error}
												<span>failed: {e.error}</span>
											{:else}
												<span class="k-num"
													>{e.fromVersion ?? 'unknown'} to {e.toVersion ?? 'unknown'}</span
												>
											{/if}
											<span class="k-muted">{e.origin}</span>
										</li>
									{/each}
								</ul>
							</details>
						{/if}
					{/if}
				</div>
			</section>

			<section id="maintenance" class="group" aria-labelledby="maintenance-h">
				<div class="group-head">
					<div>
						<h2 id="maintenance-h" class="group-title">Maintenance</h2>
						<p class="group-desc">
							Jobs that act on the whole catalogue. Each one asks before it starts.
						</p>
					</div>
				</div>
				<div class="group-body jobs">
					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Save the whole catalogue</p>
							<p class="k-hint">
								Copy every series from the source engine into the database that readers load from.
								Series details are saved first. Chapter lists follow in the background.
							</p>
							{#if persistResult}
								<p class="result" class:bad={!persistResult.ok} role="status">
									{persistResult.text}
								</p>
							{/if}
						</div>
						<Button icon="database" busy={persisting} onclick={runPersistCatalogue}>
							{persisting ? 'Saving…' : 'Save catalogue'}
						</Button>
					</div>

					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Store covers on this server</p>
							<p class="k-hint">
								Download each cover once and keep it in the database, so covers load from this
								server. Only covers that are not stored yet are fetched.
							</p>
							{#if coversResult}
								<p class="result" class:bad={!coversResult.ok} role="status">{coversResult.text}</p>
							{/if}
						</div>
						<Button icon="image" busy={coversRunning} onclick={runMaterializeCovers}>
							{coversRunning ? 'Starting…' : 'Download covers'}
						</Button>
					</div>

					<div class="setting">
						<div class="setting-text">
							<p class="setting-name">Merge duplicate works</p>
							<p class="k-hint">
								Sort works that share a title or an alternative title. Articles (the, a, an) are
								ignored. A pair with matching evidence is merged. A pair that is two different works
								is closed. Each merge deletes the merged work and cannot be undone. Unclear pairs
								stay in
								<a class="k-link" href={href('/review')}>Duplicates</a>, likeliest first.
							</p>
							{#if consolidateMsg}
								<p class="result" role="status">{consolidateMsg}</p>
							{/if}
						</div>
						<div class="job-actions">
							{#if consolidating}
								<Button disabled={stopRequested} onclick={() => (stopRequested = true)}>
									{stopRequested ? 'Stopping after this batch…' : 'Stop'}
								</Button>
							{/if}
							<Button variant="danger" icon="merge" busy={consolidating} onclick={consolidate}>
								{consolidating ? `Merging, batch ${batchesDone + 1}` : 'Merge duplicates'}
							</Button>
						</div>
					</div>
				</div>
			</section>

			<section id="about" class="group" aria-labelledby="about-h">
				<div class="group-head">
					<h2 id="about-h" class="group-title">About</h2>
				</div>
				<div class="group-body">
					<dl class="facts">
						<dt>Server</dt>
						<!-- The embedded console talks to its own origin's /graphql: show that origin. -->
						<dd class="k-mono">{target === 'embedded' ? serverAddress() : config.apiEndpoint}</dd>
						{#if instance.info?.version}
							<dt>Version</dt>
							<dd class="k-mono">{instance.info.version}</dd>
						{/if}
						<dt>Signed in as</dt>
						<dd>{auth.user?.username ?? 'Unknown'}</dd>
					</dl>
				</div>
			</section>
		</div>
	</div>
</div>

<style>
	.layout {
		display: grid;
		grid-template-columns: 180px minmax(0, 720px);
		gap: var(--k-space-8);
		align-items: start;
	}
	.toc {
		position: sticky;
		top: var(--k-space-6);
		display: grid;
		gap: 2px;
	}
	.toc a {
		padding: 7px var(--k-space-3);
		border-radius: var(--k-radius);
		color: var(--k-text-dim);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.toc a:hover {
		background: var(--k-hover-fill);
		color: var(--k-text-bright);
	}
	/* Sections are separated by a rule and space, not boxed: nothing here needs
	   elevation, and the page reads as one form. */
	.sections {
		display: grid;
		min-width: 0;
	}
	.group {
		padding: var(--k-space-8) 0;
		border-top: 1px solid var(--k-border-1);
		scroll-margin-top: var(--k-space-4);
	}
	.group:first-child {
		padding-top: 0;
		border-top: 0;
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
	.group-foot {
		margin-top: var(--k-space-4);
		font-size: var(--k-text-sm);
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
	.jobs {
		display: grid;
		gap: var(--k-space-5);
	}
	.jobs .setting + .setting {
		padding-top: var(--k-space-5);
		border-top: 1px solid var(--k-border);
	}
	.job-actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}
	.result {
		margin-top: 4px;
		color: var(--k-success);
		font-size: var(--k-text-sm);
		line-height: 1.55;
	}
	.bad {
		color: var(--k-danger);
	}
	code {
		font-family: var(--k-font-mono);
		font-size: 0.92em;
		overflow-wrap: anywhere;
	}

	.repos {
		display: grid;
		justify-content: stretch;
		gap: var(--k-space-4);
	}
	.repo-form {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}
	.repo-form .k-input {
		flex: 1 1 280px;
		min-width: 0;
	}
	/* Flat rows, no rules: the list reads as one quiet block. */
	.repo-list {
		display: grid;
		gap: 2px;
		padding: 0;
		list-style: none;
	}
	.repo-list li {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
		min-height: 52px;
		padding: 6px 6px 6px 14px;
		border-radius: var(--k-radius);
		background: var(--k-surface-2);
		color: var(--k-text-2);
	}
	.repo-text {
		display: grid;
		gap: 2px;
		flex: 1 1 auto;
		min-width: 0;
		font-size: var(--k-text-sm);
	}
	.repo-name {
		color: var(--k-text-1);
		font-weight: 600;
	}
	.repo-text .k-mono {
		font-size: var(--k-text-xs);
	}
	.calm {
		display: flex;
		align-items: flex-start;
		gap: 8px;
		line-height: 1.55;
	}
	.calm :global(svg) {
		flex: none;
		margin-top: 3px;
	}
	.retry {
		padding: 0;
		border: 0;
		background: none;
		font: inherit;
		cursor: pointer;
	}

	.history summary {
		cursor: pointer;
		color: var(--k-text-muted);
		font-size: var(--k-text-sm);
		font-weight: 600;
	}
	.history ul {
		display: grid;
		gap: 6px;
		margin-top: var(--k-space-3);
		padding: 0;
		list-style: none;
		font-size: var(--k-text-xs);
	}
	.history li {
		display: grid;
		grid-template-columns: 124px minmax(0, 1.4fr) minmax(0, 1fr) auto;
		gap: var(--k-space-3);
		align-items: baseline;
	}

	.facts {
		display: grid;
		grid-template-columns: 120px minmax(0, 1fr);
		gap: var(--k-space-2) var(--k-space-4);
		font-size: var(--k-text-sm);
	}
	.facts dt {
		color: var(--k-text-dim);
	}
	.facts dd {
		overflow-wrap: anywhere;
	}

	@media (max-width: 900px) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
			gap: var(--k-space-4);
		}
		.toc {
			position: static;
			display: flex;
			gap: var(--k-space-1);
			overflow-x: auto;
			padding-bottom: 4px;
		}
		.toc a {
			white-space: nowrap;
		}
		.history li {
			grid-template-columns: minmax(0, 1fr);
			gap: 2px;
		}
	}
</style>
