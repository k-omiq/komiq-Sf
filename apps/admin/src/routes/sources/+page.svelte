<script lang="ts">
	import type {
		BulkAddResult,
		ExtensionInfo,
		SourceBrowseEntry,
		SourceBrowseType,
		SourceIngestJob,
		SourceInfo,
		SourceScanHealth,
	} from '@komika/types';
	import { auth } from '$lib/auth.svelte';
	import {
		addExtensionRepo,
		browseSource,
		bulkAddSourceSeries,
		cancelExtensionIngest,
		cancelSourceIngest,
		installExtension,
		loadExtensions,
		loadSourceIngestJobs,
		loadSourceScanHealth,
		loadSources,
		materializeCatalogueCovers,
		persistCatalogue,
		setExtensionSubscription,
		startExtensionIngest,
		startSourceIngest,
		uninstallExtension,
		updateExtension,
	} from '$lib/data';

	// Auth redirect is centralized in +layout.svelte.

	let tab = $state<'extensions' | 'catalogue' | 'health'>('extensions');

	// ---- Maintenance: persist whole catalogue to the DB cache -----------------
	// Materializes the entire Suwayomi library into the DB read-cache that the
	// reader now reads from (metadata synchronously; chapter lists in a server
	// background task). Heavy on a large real catalogue, so it's behind a confirm
	// step. Execution is gated server-side by require_admin — that IS the prod gate.
	let persistConfirm = $state(false);
	let persisting = $state(false);
	let persistError = $state<string | null>(null);
	/** Series count from the last successful run, for honest reporting. */
	let persistCount = $state<number | null>(null);

	// ---- Maintenance: materialize covers into the DB (off the CF Worker) ------
	// Crawls the whole catalogue, fetches each still-uncached work's MangaDex
	// cover, and stores a bounded WebP in the DB so the web reader serves covers
	// from /covers/{id}.webp instead of the image Worker. A polite background
	// crawl (bounded by the MangaDex limiter); the returned number is how many
	// works were still uncached (queued) at kick-off — progress lands in logs.
	let coversConfirm = $state(false);
	let coversRunning = $state(false);
	let coversError = $state<string | null>(null);
	let coversQueued = $state<number | null>(null);

	async function runMaterializeCovers(): Promise<void> {
		if (coversRunning) return;
		coversRunning = true;
		coversError = null;
		coversQueued = null;
		try {
			coversQueued = await materializeCatalogueCovers();
			coversConfirm = false;
		} catch (err) {
			coversError = err instanceof Error ? err.message : 'Failed to start cover materialization.';
		} finally {
			coversRunning = false;
		}
	}

	async function runPersistCatalogue(): Promise<void> {
		if (persisting) return;
		persisting = true;
		persistError = null;
		persistCount = null;
		try {
			persistCount = await persistCatalogue();
			persistConfirm = false;
		} catch (err) {
			persistError = err instanceof Error ? err.message : 'Failed to persist the catalogue.';
		} finally {
			persisting = false;
		}
	}

	// ---- Extensions tab -------------------------------------------------------
	let exts = $state<ExtensionInfo[]>([]);
	let extsLoading = $state(false);
	let extsLoaded = $state(false);
	let extsError = $state<string | null>(null);
	let extFilter = $state<'all' | 'installed' | 'available'>('all');
	let extLang = $state('all');
	let extQuery = $state('');
	/** pkgName of the row with an in-flight action (one at a time). */
	let busyPkg = $state<string | null>(null);
	/** Per-row action outcome (success note or error). */
	let rowMsg = $state<Record<string, string>>({});

	// One-time "add repo" affordance shown when nothing is seeded.
	// `index.pb` is the URL Keiyoushi publishes; the older `index.min.json` on the
	// same branch still resolves but now holds only two "update your app" stubs.
	const KEIYOUSHI_URL = 'https://github.com/keiyoushi/extensions/raw/repo/index.pb';
	let repoUrl = $state(KEIYOUSHI_URL);
	let addingRepo = $state(false);
	let repoMsg = $state<string | null>(null);

	/** Extensions whose store icon failed to load — fall back to the placeholder. */
	let failedIcons = $state<Record<string, boolean>>({});

	function iconFailed(pkgName: string): void {
		failedIcons = { ...failedIcons, [pkgName]: true };
	}

	async function refreshExtensions(refresh = false): Promise<void> {
		extsLoading = true;
		extsError = null;
		try {
			exts = await loadExtensions(refresh);
			extsLoaded = true;
		} catch (err) {
			extsError = err instanceof Error ? err.message : 'Failed to load extensions.';
			// Keep any already-listed extensions on a failed on-demand refresh; only
			// a failed initial load leaves the list empty.
		} finally {
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
			!confirm(
				`Uninstall ${e.name}? This removes the source and its catalogue picker entry until you reinstall it.`,
			)
		)
			return;
		busyPkg = e.pkgName;
		const { [e.pkgName]: _drop, ...rest } = rowMsg;
		rowMsg = rest;
		try {
			const fn =
				verb === 'install'
					? installExtension
					: verb === 'uninstall'
						? uninstallExtension
						: updateExtension;
			const updated = await fn(e.pkgName);
			exts = exts.map((x) => (x.pkgName === updated.pkgName ? updated : x));
			rowMsg = {
				...rowMsg,
				[e.pkgName]: verb === 'uninstall' ? 'Uninstalled.' : `Installed ${updated.versionName}.`,
			};
			// Installed sources changed — make the catalogue tab re-fetch its picker.
			sourcesLoaded = false;
		} catch (err) {
			rowMsg = {
				...rowMsg,
				[e.pkgName]: err instanceof Error ? err.message : `Failed to ${verb}.`,
			};
		} finally {
			busyPkg = null;
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
		const { [e.pkgName]: _drop, ...rest } = rowMsg;
		rowMsg = rest;
		try {
			const next = await setExtensionSubscription(e.pkgName, !e.subscribed);
			exts = exts.map((x) => (x.pkgName === e.pkgName ? { ...x, subscribed: next } : x));
			rowMsg = {
				...rowMsg,
				[e.pkgName]: next ? 'Syncing new series from now on.' : 'Sync off.',
			};
		} catch (err) {
			rowMsg = {
				...rowMsg,
				[e.pkgName]: err instanceof Error ? err.message : 'Failed to change sync.',
			};
		} finally {
			busyPkg = null;
		}
	}

	async function onAddRepo(ev: SubmitEvent): Promise<void> {
		ev.preventDefault();
		if (addingRepo) return;
		addingRepo = true;
		repoMsg = null;
		try {
			const count = await addExtensionRepo(repoUrl.trim());
			repoMsg = `Repo added — ${count} extension${count === 1 ? '' : 's'} known.`;
			await refreshExtensions(false);
		} catch (err) {
			repoMsg = err instanceof Error ? err.message : 'Failed to add the repo.';
		} finally {
			addingRepo = false;
		}
	}

	// ---- Sources → catalogue tab ------------------------------------------------
	let sources = $state<SourceInfo[]>([]);
	let sourcesLoading = $state(false);
	let sourcesLoaded = $state(false);
	let sourcesError = $state<string | null>(null);
	let sourceId = $state('');

	let browseType = $state<SourceBrowseType>('POPULAR');
	let searchQuery = $state('');
	let pageNum = $state(1);
	let hasNext = $state(false);
	let items = $state<SourceBrowseEntry[]>([]);
	let browsing = $state(false);
	let browsed = $state(false);
	let browseError = $state<string | null>(null);

	/** Selected suwayomiMangaIds (insertion-ordered). */
	let picked = $state<string[]>([]);
	/** Titles for every id seen while browsing, so bulk results can be labelled. */
	let titles = $state<Record<string, string>>({});

	let adding = $state(false);
	let addError = $state<string | null>(null);
	let bulkResult = $state<BulkAddResult | null>(null);

	async function refreshSources(): Promise<void> {
		sourcesLoading = true;
		sourcesError = null;
		try {
			sources = await loadSources();
			sourcesLoaded = true;
			if (sourceId && !sources.some((s) => String(s.id) === sourceId)) sourceId = '';
		} catch (err) {
			sourcesError = err instanceof Error ? err.message : 'Failed to load sources.';
			sources = [];
		} finally {
			sourcesLoading = false;
		}
	}

	$effect(() => {
		if (!auth.user || tab !== 'catalogue' || sourcesLoaded || sourcesLoading) return;
		void refreshSources();
	});

	// ---- Scan health (Phase E4.3) ---------------------------------------------
	// "Is this source actually working?" had no honest answer before E4: every Suwayomi
	// fetch path falls back to reading Suwayomi's own DB when the upstream fetch fails,
	// so a dead source's scans were recorded as successes — 209 series on one source sat
	// at 0 chapters reporting zero failures. `cachedFallback` is that case, now counted.
	let health = $state<SourceScanHealth[]>([]);
	let healthLoading = $state(false);
	let healthLoaded = $state(false);
	let healthError = $state<string | null>(null);

	async function refreshHealth(): Promise<void> {
		healthLoading = true;
		healthError = null;
		try {
			health = await loadSourceScanHealth();
			healthLoaded = true;
		} catch (err) {
			healthError = err instanceof Error ? err.message : 'Failed to load scan health.';
		} finally {
			healthLoading = false;
		}
	}

	$effect(() => {
		if (!auth.user || tab !== 'health' || healthLoaded || healthLoading) return;
		void refreshHealth();
	});

	/** Sources with an open whole-source outage — the loud ones, surfaced above the table. */
	const outages = $derived(health.filter((h) => h.outageDetectedAt));
	/** Works that go dark while those outages last. */
	const outageWorks = $derived(outages.reduce((n, h) => n + h.exclusiveWorks, 0));

	function shortDate(iso: string | null): string {
		if (!iso) return '—';
		const d = new Date(iso);
		return Number.isNaN(d.getTime()) ? '—' : d.toISOString().slice(0, 16).replace('T', ' ');
	}

	async function browse(page: number): Promise<void> {
		if (!sourceId) return;
		browsing = true;
		browseError = null;
		try {
			const res = await browseSource(
				sourceId,
				browseType,
				page,
				browseType === 'SEARCH' ? searchQuery.trim() : undefined,
			);
			items = res.items;
			pageNum = res.page;
			hasNext = res.hasNextPage;
			browsed = true;
			const t = { ...titles };
			for (const it of res.items) t[String(it.suwayomiMangaId)] = it.title;
			titles = t;
		} catch (err) {
			browseError = err instanceof Error ? err.message : 'Failed to browse the source.';
			items = [];
			hasNext = false;
		} finally {
			browsing = false;
		}
	}

	function startBrowse(type: SourceBrowseType): void {
		browseType = type;
		items = [];
		browsed = false;
		if (type !== 'SEARCH') void browse(1);
	}

	function onPickSource(): void {
		items = [];
		browsed = false;
		browseError = null;
		picked = [];
		bulkResult = null;
		void checkActiveJob();
		if (sourceId && browseType !== 'SEARCH') void browse(1);
	}

	function onSearch(ev: SubmitEvent): void {
		ev.preventDefault();
		if (searchQuery.trim()) void browse(1);
	}

	function togglePick(id: string): void {
		if (picked.includes(id)) picked = picked.filter((x) => x !== id);
		else if (picked.length < 100) picked = [...picked, id];
	}

	async function addPicked(): Promise<void> {
		if (adding || picked.length === 0) return;
		adding = true;
		addError = null;
		bulkResult = null;
		try {
			bulkResult = await bulkAddSourceSeries(picked);
			picked = [];
		} catch (err) {
			addError = err instanceof Error ? err.message : 'Bulk add failed.';
		} finally {
			adding = false;
		}
	}

	/** Same decision vocabulary as the catalog page's Tier-2 add flow. */
	function decisionLabel(decision: string): string {
		switch (decision) {
			case 'auto_merge':
				return 'auto-merged';
			case 'review':
				return 'queued for review';
			case 'new':
				return 'new work';
			case 'existing':
				return 'already in catalogue';
			default:
				return decision;
		}
	}

	function decisionClass(decision: string): string {
		switch (decision) {
			case 'auto_merge':
				return 'merged';
			case 'review':
				return 'review';
			case 'new':
				return 'new';
			case 'existing':
				return 'existing';
			default:
				return '';
		}
	}

	function entryTitle(id: string): string {
		return titles[id] ?? `#${id}`;
	}

	// ---- extension → catalogue (double-click / Browse) --------------------------
	/** When set, the catalogue source picker is scoped to one extension's sources. */
	let extScope = $state<{ pkgName: string; name: string } | null>(null);
	/** pkgName currently resolving its sources (button busy state). */
	let resolvingPkg = $state<string | null>(null);

	const scopedSources = $derived(
		extScope ? sources.filter((s) => s.pkgName === extScope!.pkgName) : sources,
	);

	/**
	 * Open the catalogue for an extension: resolve its installed Suwayomi sources
	 * (match on pkgName). One source → straight to its catalogue; several → scope
	 * the picker to them and let the admin choose the per-language variant.
	 */
	async function browseExtension(e: ExtensionInfo): Promise<void> {
		if (!e.isInstalled) {
			rowMsg = { ...rowMsg, [e.pkgName]: 'Install this extension first to browse it.' };
			return;
		}
		if (resolvingPkg) return;
		resolvingPkg = e.pkgName;
		try {
			if (!sourcesLoaded) await refreshSources();
			const matches = sources.filter((s) => s.pkgName === e.pkgName);
			extScope = { pkgName: e.pkgName, name: e.name };
			tab = 'catalogue';
			resetCatalogueView();
			// Resume an already-running extension-wide ingest for this extension
			// (or clear a stale panel from a different one).
			void syncExtPanel(
				e.pkgName,
				e.name,
				matches.map((s) => String(s.id)),
			);
			if (matches.length === 1) {
				sourceId = String(matches[0].id);
				browseType = 'POPULAR';
				await checkActiveJob();
				void browse(1);
			} else {
				// Zero (engine hasn't exposed them yet — honest empty picker) or several:
				// let the scoped picker prompt.
				sourceId = '';
			}
		} catch (err) {
			rowMsg = {
				...rowMsg,
				[e.pkgName]: err instanceof Error ? err.message : 'Failed to open the catalogue.',
			};
		} finally {
			resolvingPkg = null;
		}
	}

	function clearScope(): void {
		extScope = null;
		sourceId = '';
		resetCatalogueView();
		stopPolling();
		job = null;
		// Invalidate any in-flight job lookup/start so it can't repopulate the panel.
		jobGen++;
	}

	/** Reset the browse listing + selection state (not the source pick). */
	function resetCatalogueView(): void {
		items = [];
		browsed = false;
		browseError = null;
		picked = [];
		bulkResult = null;
		addError = null;
	}

	// ---- "Add all from this source" background ingest (S1) -----------------------
	let job = $state<SourceIngestJob | null>(null);
	let jobStarting = $state(false);
	let jobCancelling = $state(false);
	let jobError = $state<string | null>(null);
	/** True once polling gave up after repeated failures — surfaces a Retry button. */
	let pollStopped = $state(false);
	/**
	 * Monotonic token bumped whenever the source context changes (source switch,
	 * scope clear). An async that resolves after the token moved is stale and must
	 * not mutate the visible job/poll state — guards the source-switch race where
	 * source A's in-flight lookup resolves after source B is selected.
	 */
	let jobGen = 0;

	const POLL_BASE_MS = 2000;
	const POLL_MAX_FAILURES = 3;
	/** setTimeout handle (self-scheduling, so the delay can back off). */
	let pollTimer: ReturnType<typeof setTimeout> | null = null;
	/** Consecutive poll failures — drives backoff and the give-up threshold. */
	let pollFailures = 0;

	function stopPolling(): void {
		if (pollTimer) {
			clearTimeout(pollTimer);
			pollTimer = null;
		}
	}

	function scheduleNextPoll(delayMs: number): void {
		stopPolling();
		pollTimer = setTimeout(() => void pollJob(), delayMs);
	}

	/** (Re)start polling from a clean slate: clears any pending tick + failure state. */
	function startPolling(): void {
		stopPolling();
		pollFailures = 0;
		pollStopped = false;
		scheduleNextPoll(POLL_BASE_MS);
	}

	/** Restart polling after it gave up (Retry affordance), attempting immediately. */
	function retryPolling(): void {
		if (!job) return;
		pollFailures = 0;
		pollStopped = false;
		jobError = null;
		scheduleNextPoll(0);
	}

	/**
	 * One poll tick: refresh our job from the active list; on drop-out fetch the
	 * terminal row. Resilient by design — a fetch failure (server down, token
	 * expiry, 5xx) does NOT keep the tight 2s loop alive: it backs off (2s → 4s →
	 * 8s) and, after {@link POLL_MAX_FAILURES} consecutive failures, stops and
	 * surfaces a Retry button instead of hammering the backend forever. Both the
	 * active-list fetch AND the terminal-row fetch share this handling, so a
	 * failure on the terminal fetch can't silently strand the panel as "running".
	 */
	async function pollJob(): Promise<void> {
		pollTimer = null;
		if (!job) return;
		const id = String(job.id);
		try {
			const active = await loadSourceIngestJobs(true);
			const mine = active.find((j) => String(j.id) === id);
			if (mine) {
				job = mine;
				pollFailures = 0;
				jobError = null;
				scheduleNextPoll(POLL_BASE_MS);
				return;
			}
			// Dropped from the active list → reached a terminal state. Grab the
			// terminal row once (covered by the same catch), then stop cleanly.
			const all = await loadSourceIngestJobs(false);
			const term = all.find((j) => String(j.id) === id);
			if (term) job = term;
			pollFailures = 0;
			jobError = null;
			stopPolling();
		} catch (err) {
			pollFailures += 1;
			jobError = err instanceof Error ? err.message : 'Failed to poll ingest progress.';
			if (pollFailures >= POLL_MAX_FAILURES) {
				// Give up the loop; the panel keeps its last-known state + a Retry button.
				stopPolling();
				pollStopped = true;
			} else {
				// Exponential backoff: 2s → 4s → 8s (capped).
				scheduleNextPoll(Math.min(POLL_BASE_MS * 2 ** pollFailures, 8000));
			}
		}
	}

	/** On (re)selecting a source, resume showing any job already running for it. */
	async function checkActiveJob(): Promise<void> {
		stopPolling();
		job = null;
		jobError = null;
		const gen = ++jobGen;
		const forSource = sourceId;
		if (!forSource) return;
		try {
			const active = await loadSourceIngestJobs(true);
			// Bail if another source was selected while this lookup was in flight —
			// otherwise a stale resolve would show/poll the wrong source's job.
			if (gen !== jobGen) return;
			const mine = active.find((j) => String(j.sourceId) === forSource);
			if (mine) {
				job = mine;
				startPolling();
			}
		} catch {
			/* non-fatal — a missing job list shouldn't block browsing */
		}
	}

	async function addAllFromSource(): Promise<void> {
		if (!sourceId || jobStarting || job?.state === 'running') return;
		jobStarting = true;
		jobError = null;
		const gen = jobGen;
		try {
			const started = await startSourceIngest(sourceId);
			// If the source changed mid-request, don't show/poll this job for the
			// now-selected source (it's still running server-side and resumes on
			// re-selecting its source).
			if (gen !== jobGen) return;
			job = started;
			if (job.state === 'running') startPolling();
		} catch (err) {
			if (gen === jobGen) {
				jobError = err instanceof Error ? err.message : 'Failed to start ingest.';
			}
		} finally {
			jobStarting = false;
		}
	}

	async function cancelJob(): Promise<void> {
		if (!job || jobCancelling) return;
		jobCancelling = true;
		try {
			job = await cancelSourceIngest(job.id);
			if (job.state !== 'running') stopPolling();
		} catch (err) {
			jobError = err instanceof Error ? err.message : 'Failed to cancel ingest.';
		} finally {
			jobCancelling = false;
		}
	}

	function dismissJob(): void {
		stopPolling();
		job = null;
		jobError = null;
		pollFailures = 0;
		pollStopped = false;
	}

	const JOB_STATE_LABEL: Record<string, string> = {
		running: 'Running',
		completed: 'Completed',
		cancelled: 'Cancelled',
		failed: 'Failed',
	};

	// ---- "Add all from this EXTENSION" background ingest (F1) --------------------
	// One ingest job per installed source of an extension, tracked together as an
	// aggregate. Independent of the per-source `job` above so both can coexist.
	let extIngest = $state<{ pkgName: string; name: string } | null>(null);
	let extJobs = $state<SourceIngestJob[]>([]);
	/** Visible source count when the run started — for honest "skipped" reporting. */
	let extVisibleSources = $state(0);
	let extStarting = $state(false);
	let extCancelling = $state(false);
	let extError = $state<string | null>(null);
	let extPollStopped = $state(false);
	/** Generation token: bumped on each new extension run / scope change. */
	let extGen = 0;
	let extPollTimer: ReturnType<typeof setTimeout> | null = null;
	let extPollFailures = 0;

	const extAgg = $derived(
		extJobs.reduce(
			(a, j) => ({
				pagesDone: a.pagesDone + j.pagesDone,
				itemsSeen: a.itemsSeen + j.itemsSeen,
				succeeded: a.succeeded + j.succeeded,
				newWorks: a.newWorks + j.newWorks,
				autoMerged: a.autoMerged + j.autoMerged,
				queuedForReview: a.queuedForReview + j.queuedForReview,
				alreadyExisting: a.alreadyExisting + j.alreadyExisting,
				failed: a.failed + j.failed,
			}),
			{
				pagesDone: 0,
				itemsSeen: 0,
				succeeded: 0,
				newWorks: 0,
				autoMerged: 0,
				queuedForReview: 0,
				alreadyExisting: 0,
				failed: 0,
			},
		),
	);
	const extRunningCount = $derived(extJobs.filter((j) => j.state === 'running').length);
	const extDoneCount = $derived(extJobs.filter((j) => j.state !== 'running').length);
	const extAnyRunning = $derived(extRunningCount > 0);
	/** Sources present-but-not-started (e.g. server-side skipped); honest only when >0. */
	const extSkipped = $derived(
		Math.max(0, extVisibleSources - new Set(extJobs.map((j) => String(j.sourceId))).size),
	);

	function stopExtPolling(): void {
		if (extPollTimer) {
			clearTimeout(extPollTimer);
			extPollTimer = null;
		}
	}

	function scheduleExtPoll(delayMs: number): void {
		stopExtPolling();
		extPollTimer = setTimeout(() => void pollExtJobs(), delayMs);
	}

	function startExtPolling(): void {
		stopExtPolling();
		extPollFailures = 0;
		extPollStopped = false;
		scheduleExtPoll(EXT_POLL_BASE_MS);
	}

	function retryExtPolling(): void {
		if (extJobs.length === 0) return;
		extPollFailures = 0;
		extPollStopped = false;
		extError = null;
		scheduleExtPoll(0);
	}

	const EXT_POLL_BASE_MS = 2000;

	/**
	 * Poll all tracked extension jobs at once: refresh each from the active list,
	 * and fetch the full list once to capture terminal rows for any that dropped
	 * out. Shares the single-poller's backoff/give-up/Retry + generation guard, so
	 * a server blip (the note warns :8080 may restart) backs off rather than
	 * looping, and a stale run can't clobber a newer one.
	 */
	async function pollExtJobs(): Promise<void> {
		extPollTimer = null;
		if (!extIngest || extJobs.length === 0) return;
		const gen = extGen;
		const trackedIds = new Set(extJobs.map((j) => String(j.id)));
		try {
			const active = await loadSourceIngestJobs(true);
			if (gen !== extGen) return;
			const activeById = new Map(active.map((j) => [String(j.id), j]));
			const anyDropped = [...trackedIds].some((id) => !activeById.has(id));
			let allById: Map<string, SourceIngestJob> | null = null;
			if (anyDropped) {
				const all = await loadSourceIngestJobs(false);
				if (gen !== extGen) return;
				allById = new Map(all.map((j) => [String(j.id), j]));
			}
			extJobs = extJobs.map((j) => {
				const id = String(j.id);
				return activeById.get(id) ?? allById?.get(id) ?? j;
			});
			extPollFailures = 0;
			extError = null;
			if (extJobs.every((j) => j.state !== 'running')) stopExtPolling();
			else scheduleExtPoll(EXT_POLL_BASE_MS);
		} catch (err) {
			extPollFailures += 1;
			extError = err instanceof Error ? err.message : 'Failed to poll extension ingest progress.';
			if (extPollFailures >= POLL_MAX_FAILURES) {
				stopExtPolling();
				extPollStopped = true;
			} else {
				scheduleExtPoll(Math.min(EXT_POLL_BASE_MS * 2 ** extPollFailures, 8000));
			}
		}
	}

	async function addAllFromExtension(): Promise<void> {
		if (!extScope || extStarting || extAnyRunning) return;
		extStarting = true;
		extError = null;
		const gen = ++extGen;
		stopExtPolling();
		const pkgName = extScope.pkgName;
		const name = extScope.name;
		const visible = scopedSources.length;
		try {
			const jobs = await startExtensionIngest(pkgName);
			if (gen !== extGen) return;
			extIngest = { pkgName, name };
			extJobs = jobs;
			extVisibleSources = visible;
			extPollStopped = false;
			extPollFailures = 0;
			if (jobs.some((j) => j.state === 'running')) startExtPolling();
		} catch (err) {
			if (gen === extGen) {
				extError = err instanceof Error ? err.message : 'Failed to start extension ingest.';
			}
		} finally {
			extStarting = false;
		}
	}

	async function cancelAllFromExtension(): Promise<void> {
		if (!extIngest || extCancelling) return;
		extCancelling = true;
		try {
			const cancelled = await cancelExtensionIngest(extIngest.pkgName);
			const byId = new Map(cancelled.map((j) => [String(j.id), j]));
			extJobs = extJobs.map((j) => byId.get(String(j.id)) ?? j);
			// Any still showing running just completed between poll and cancel — one
			// more reconcile catches their terminal state; otherwise stop cleanly.
			if (extJobs.some((j) => j.state === 'running')) scheduleExtPoll(0);
			else stopExtPolling();
		} catch (err) {
			extError = err instanceof Error ? err.message : 'Failed to cancel extension ingest.';
		} finally {
			extCancelling = false;
		}
	}

	function dismissExtIngest(): void {
		stopExtPolling();
		extIngest = null;
		extJobs = [];
		extError = null;
		extPollFailures = 0;
		extPollStopped = false;
	}

	/**
	 * On opening an extension's catalogue, resume an already-running extension
	 * ingest for it (survives navigation), and clear a stale panel left over from
	 * a *different* extension.
	 */
	async function syncExtPanel(pkgName: string, name: string, srcIds: string[]): Promise<void> {
		// Snapshot the gen but DON'T claim it yet — only take over if we actually
		// find a running ingest for this extension, so browsing extension B can't
		// freeze extension A's still-running poller.
		const gen = extGen;
		try {
			const active = await loadSourceIngestJobs(true);
			if (gen !== extGen) return;
			const idSet = new Set(srcIds);
			const mine = active.filter((j) => idSet.has(String(j.sourceId)));
			if (mine.length > 0) {
				extGen++; // claim: supersede any prior run's poll loop
				extIngest = { pkgName, name };
				extJobs = mine;
				extVisibleSources = srcIds.length;
				extPollStopped = false;
				extPollFailures = 0;
				startExtPolling();
			} else if (extIngest && extIngest.pkgName !== pkgName && !extAnyRunning) {
				// Stale panel from a different, no-longer-running extension — clear it.
				dismissExtIngest();
			}
		} catch {
			/* non-fatal — resume is best-effort */
		}
	}

	// Stop both pollers when the component unmounts.
	$effect(() => () => {
		stopPolling();
		stopExtPolling();
	});
</script>

<div class="page">
	<div class="page-head">
		<div>
			<h1>Sources</h1>
			<p class="lede">
				Manage Keiyoushi extensions on the engine and bulk-ingest source catalogues into the
				canonical DB
			</p>
		</div>
		<div class="tabs" role="tablist" aria-label="Sources console">
			<button
				role="tab"
				aria-selected={tab === 'extensions'}
				class:on={tab === 'extensions'}
				onclick={() => (tab = 'extensions')}>Extensions</button
			>
			<button
				role="tab"
				aria-selected={tab === 'catalogue'}
				class:on={tab === 'catalogue'}
				onclick={() => (tab = 'catalogue')}>Catalogue</button
			>
			<button
				role="tab"
				aria-selected={tab === 'health'}
				class:on={tab === 'health'}
				onclick={() => (tab = 'health')}
			>
				Scan health{#if outages.length > 0}<span class="tab-alarm" aria-label="sources down"
						>{outages.length}</span
					>{/if}
			</button>
		</div>
	</div>

	<section class="maint" aria-label="Maintenance">
		<div class="maint-copy">
			<div class="maint-head">
				<span class="maint-kicker">Production maintenance</span>
				<h2>Save everything to the DB</h2>
			</div>
			<p class="maint-lede">
				Materialize the entire source library into the DB read-cache that home, series, updates and
				chapters read from. Series metadata is saved immediately; chapter lists fill in a background
				task afterwards. Safe to re-run; heavy on a large catalogue.
			</p>
			{#if persistError}
				<p class="maint-msg error">{persistError}</p>
			{:else if persistCount !== null}
				<p class="maint-msg ok">
					Persisted {persistCount} series to the DB. Chapter lists are filling in the background.
				</p>
			{/if}
		</div>
		<div class="maint-actions">
			{#if persistConfirm}
				<span class="maint-confirm-q">Save the whole catalogue now?</span>
				<button class="act primary" disabled={persisting} onclick={runPersistCatalogue}>
					{persisting ? 'Saving…' : 'Confirm save'}
				</button>
				<button class="act" disabled={persisting} onclick={() => (persistConfirm = false)}>
					Cancel
				</button>
			{:else}
				<button
					class="act primary"
					disabled={persisting}
					onclick={() => {
						persistError = null;
						persistCount = null;
						persistConfirm = true;
					}}
					title="Materialize the whole Suwayomi library into the DB read-cache"
				>
					{persisting ? 'Saving…' : 'Save everything to DB'}
				</button>
			{/if}
		</div>
	</section>

	<section class="maint" aria-label="Cover cache">
		<div class="maint-copy">
			<div class="maint-head">
				<span class="maint-kicker">Production maintenance</span>
				<h2>Cache covers in the DB</h2>
			</div>
			<p class="maint-lede">
				Fetch every catalogue cover once and store it in the DB, so the web reader loads covers from
				our own origin (<code>/covers/…</code>) instead of the Cloudflare image Worker. Runs as a
				polite background crawl bounded by the MangaDex rate limiter; safe to re-run (only
				still-uncached works are fetched).
			</p>
			{#if coversError}
				<p class="maint-msg error">{coversError}</p>
			{:else if coversQueued !== null}
				<p class="maint-msg ok">
					Crawl started for {coversQueued} uncached {coversQueued === 1 ? 'work' : 'works'}. Covers
					fill in the background; watch the server logs for progress.
				</p>
			{/if}
		</div>
		<div class="maint-actions">
			{#if coversConfirm}
				<span class="maint-confirm-q">Crawl all covers now?</span>
				<button class="act primary" disabled={coversRunning} onclick={runMaterializeCovers}>
					{coversRunning ? 'Starting…' : 'Confirm crawl'}
				</button>
				<button class="act" disabled={coversRunning} onclick={() => (coversConfirm = false)}>
					Cancel
				</button>
			{:else}
				<button
					class="act primary"
					disabled={coversRunning}
					onclick={() => {
						coversError = null;
						coversQueued = null;
						coversConfirm = true;
					}}
					title="Materialize every catalogue cover into the DB (served off the CF Worker)"
				>
					{coversRunning ? 'Starting…' : 'Cache all covers'}
				</button>
			{/if}
		</div>
	</section>

	{#if tab === 'extensions'}
		{#if extsError && exts.length === 0}
			<div class="notice error">{extsError}</div>
		{:else if extsLoading && exts.length === 0}
			<div class="notice">Loading extensions… (first load fetches the Keiyoushi index)</div>
		{:else if exts.length === 0}
			<div class="notice">
				<p>No extensions are known to the engine — the extension store may not be seeded yet.</p>
				<form class="repo-form" onsubmit={onAddRepo}>
					<input type="url" bind:value={repoUrl} placeholder="Extension repo index URL" required />
					<button type="submit" disabled={addingRepo}>
						{addingRepo ? 'Adding…' : 'Add Keiyoushi repo'}
					</button>
				</form>
				{#if repoMsg}<p class="repo-msg">{repoMsg}</p>{/if}
			</div>
		{:else}
			{#if extsError}
				<div class="notice error">{extsError}</div>
			{/if}
			<div class="toolbar">
				<div class="segmented">
					<button class:on={extFilter === 'all'} onclick={() => (extFilter = 'all')}
						>All ({exts.length})</button
					>
					<button class:on={extFilter === 'installed'} onclick={() => (extFilter = 'installed')}
						>Installed ({installedCount})</button
					>
					<button class:on={extFilter === 'available'} onclick={() => (extFilter = 'available')}
						>Available ({exts.length - installedCount})</button
					>
				</div>
				<select bind:value={extLang} aria-label="Filter by language">
					<option value="all">All languages</option>
					{#each extLangs as l (l)}
						<option value={l}>{l}</option>
					{/each}
				</select>
				<input
					class="filter-q"
					type="search"
					placeholder="Filter extensions…"
					bind:value={extQuery}
					aria-label="Filter extensions by name"
				/>
				<button
					class="act"
					onclick={() => refreshExtensions(true)}
					disabled={extsLoading}
					title="Re-fetch the extension store indexes so versions and updates are fresh"
				>
					{extsLoading ? 'Refreshing…' : 'Refresh'}
				</button>
			</div>

			<p class="hint">
				Double-click an installed extension (or use its Browse button) to open its catalogue.
			</p>
			{#if filteredExts.length === 0}
				<div class="notice">No extensions match the current filter.</div>
			{:else}
				<div class="table" class:dim={extsLoading}>
					<div class="thead">
						<span>Extension</span>
						<span>Lang</span>
						<span>Version</span>
						<span class="right">Actions</span>
					</div>
					{#each filteredExts as e (e.pkgName)}
						<!-- Double-click is a pointer affordance for discoverability; the Browse
						     button below is the keyboard/a11y-accessible path to the same action. -->
						<!-- svelte-ignore a11y_no_static_element_interactions -->
						<div class="row" class:clickable={e.isInstalled} ondblclick={() => browseExtension(e)}>
							<div class="cell-ext">
								{#if e.iconUrl && !failedIcons[e.pkgName]}
									<img
										class="icon"
										src={e.iconUrl}
										alt=""
										loading="lazy"
										referrerpolicy="no-referrer"
										onerror={() => iconFailed(e.pkgName)}
									/>
								{:else}
									<div class="icon placeholder"></div>
								{/if}
								<div class="titles">
									<span class="title">
										{e.name}
										{#if e.isNsfw}<span class="nsfw">NSFW</span>{/if}
										{#if e.isInstalled}<span class="installed-badge">Installed</span>{/if}
									</span>
									<span class="meta">{e.pkgName}</span>
								</div>
							</div>
							<span class="lang">{e.lang}</span>
							<span class="version">
								{e.versionName}
								{#if e.hasUpdate}<span class="update-badge">update available</span>{/if}
							</span>
							<span class="actions">
								{#if rowMsg[e.pkgName]}<span class="row-msg">{rowMsg[e.pkgName]}</span>{/if}
								{#if e.isInstalled}
									<button
										class="act"
										disabled={resolvingPkg !== null}
										onclick={() => browseExtension(e)}
										>{resolvingPkg === e.pkgName ? 'Opening…' : 'Browse'}</button
									>
									{#if e.hasUpdate}
										<button
											class="act primary"
											disabled={busyPkg !== null}
											onclick={() => extAction(e, 'update')}
											>{busyPkg === e.pkgName ? 'Working…' : 'Update'}</button
										>
									{/if}
									<button
										class="act"
										class:primary={e.subscribed}
										disabled={busyPkg !== null}
										onclick={() => toggleSync(e)}
										title="Auto-discover new series from this extension and keep them updating"
										>{busyPkg === e.pkgName
											? 'Working…'
											: e.subscribed
												? 'Sync: on'
												: 'Sync: off'}</button
									>
									<button
										class="act"
										disabled={busyPkg !== null}
										onclick={() => extAction(e, 'uninstall')}
										>{busyPkg === e.pkgName ? 'Working…' : 'Uninstall'}</button
									>
								{:else}
									<button
										class="act primary"
										disabled={busyPkg !== null}
										onclick={() => extAction(e, 'install')}
										>{busyPkg === e.pkgName ? 'Installing…' : 'Install'}</button
									>
								{/if}
							</span>
						</div>
					{/each}
				</div>
			{/if}
		{/if}
	{:else if tab === 'health'}
		<!-- Sources → scan health (Phase E4.3) -->
		{#if healthError && health.length === 0}
			<div class="notice error">{healthError}</div>
		{:else if healthLoading && health.length === 0}
			<div class="notice">Loading scan health…</div>
		{:else if health.length === 0}
			<div class="notice">No source has any tracked series yet, so there is nothing to report.</div>
		{:else}
			{#if healthError}
				<div class="notice error">{healthError}</div>
			{/if}
			{#if outages.length > 0}
				<div class="notice error">
					<p>
						<strong>{outages.length} source{outages.length === 1 ? '' : 's'} down.</strong>
						Their series are parked until the source recovers, so they are not being polled every day
						for nothing.
						{#if outageWorks > 0}
							<strong>{outageWorks}</strong> work{outageWorks === 1 ? '' : 's'} reachable through no other
							source {outageWorks === 1 ? 'is' : 'are'} unreadable while this lasts.
						{/if}
					</p>
					<ul class="outage-list">
						{#each outages as o (o.id)}
							<li>
								<strong>{o.name ?? o.pkgName ?? o.id}</strong>
								— {o.confirmedFailing}/{o.series} series failing since {shortDate(
									o.outageDetectedAt,
								)}
								{#if o.outageKind === 'cached_fallback'}
									· serving Suwayomi's cache instead of the source
								{:else if o.outageKind === 'fetch_error'}
									· upstream fetch failing
								{/if}
								{#if o.outageParkedUntil}· next probe {shortDate(o.outageParkedUntil)}{/if}
								{#if !o.installed}· <em>extension no longer installed</em>{/if}
							</li>
						{/each}
					</ul>
				</div>
			{/if}
			<div class="toolbar">
				<span class="scope-label">
					{health.length} source{health.length === 1 ? '' : 's'} with tracked series · worst first
				</span>
				<button class="act" onclick={() => refreshHealth()} disabled={healthLoading}>
					{healthLoading ? 'Refreshing…' : 'Refresh'}
				</button>
			</div>
			<div class="htable" role="table" aria-label="Per-source scan health">
				<div class="hrow head" role="row">
					<span role="columnheader">Source</span>
					<span role="columnheader" title="Series the scanner tracks for this source">Series</span>
					<span role="columnheader" title="Series with a live failure streak">Failing</span>
					<span
						role="columnheader"
						title="Failures where Suwayomi served its own cached chapter list because the upstream fetch failed — before E4 these recorded as successes"
						>Cached</span
					>
					<span role="columnheader" title="Failures where the upstream fetch errored outright"
						>Errors</span
					>
					<span
						role="columnheader"
						title="Series the scanner has never got a chapter for — across a whole source, the signature of one that has never worked"
						>0 ch</span
					>
					<span role="columnheader" title="Works reachable through no other source">Only here</span>
					<span role="columnheader">Last scan</span>
				</div>
				{#each health as h (h.id)}
					<div class="hrow" role="row" class:down={!!h.outageDetectedAt}>
						<span role="cell" class="src-cell">
							<strong>{h.name ?? h.pkgName ?? h.id}</strong>
							{#if h.outageDetectedAt}<span class="pill bad">down</span>{/if}
							{#if !h.installed}<span class="pill warn">uninstalled</span>{/if}
							{#if h.subscriptionDisabledAt}<span class="pill warn">sync disabled</span>
							{:else if h.subscribed}<span class="pill">subscribed</span>{/if}
						</span>
						<span role="cell">{h.series}</span>
						<span role="cell" class:bad={h.confirmedFailing > 0}>
							{h.failing}{#if h.confirmedFailing > 0}<em class="sub"
									>({h.confirmedFailing} confirmed)</em
								>{/if}
						</span>
						<span role="cell" class:bad={h.cachedFallback > 0}>{h.cachedFallback}</span>
						<span role="cell" class:bad={h.fetchError > 0}>{h.fetchError}</span>
						<span role="cell">{h.zeroChapterSeries}</span>
						<span role="cell">{h.exclusiveWorks}</span>
						<span role="cell" class="when">{shortDate(h.lastScannedAt)}</span>
					</div>
				{/each}
			</div>
			<p class="hint">
				<strong>Cached</strong> is the failure mode this panel exists for: on an upstream error the engine
				answers from its own database, so the scan used to succeed with stale — or empty — data. A cached
				answer cannot contain a chapter we have not already seen, so it is now counted as a failed scan.
			</p>
		{/if}
	{:else}
		<!-- Sources → catalogue -->
		{#if sourcesError}
			<div class="notice error">{sourcesError}</div>
		{:else if sourcesLoading && sources.length === 0}
			<div class="notice">Loading sources…</div>
		{:else if sources.length === 0}
			<div class="notice">
				No installed sources are visible. Install an extension in the Extensions tab first — and
				every installed source (including NSFW ones like MangaDex) is listed here for management.
			</div>
		{:else}
			{#if extScope}
				<div class="scope-bar">
					<span class="scope-label">
						Catalogue for <strong>{extScope.name}</strong>
						· {scopedSources.length} source{scopedSources.length === 1 ? '' : 's'}
					</span>
					<div class="scope-actions">
						{#if scopedSources.length > 0}
							<button
								class="act primary"
								disabled={extStarting || extAnyRunning}
								onclick={addAllFromExtension}
								title="Start an ingest for every source of this extension at once"
							>
								{extStarting
									? 'Starting…'
									: extAnyRunning
										? 'Ingest running…'
										: `Add all from this extension${
												scopedSources.length > 1 ? ` (${scopedSources.length} sources)` : ''
											}`}
							</button>
						{/if}
						<button class="act" onclick={clearScope}>Show all sources</button>
					</div>
				</div>
			{/if}

			{#if extError}
				<div class="notice error poll-error">
					<span>
						{extError}{extPollStopped ? ' — progress updates paused after repeated failures.' : ''}
					</span>
					{#if extPollStopped}
						<button class="act" onclick={retryExtPolling}>Retry</button>
					{/if}
				</div>
			{/if}

			{#if extIngest}
				<div class="job ext-job" class:done={!extAnyRunning}>
					<div class="job-head">
						<span class="job-title">
							<span class="job-state {extAnyRunning ? 'running' : 'completed'}">
								{extAnyRunning ? 'Running' : 'Finished'}
							</span>
							Add all from {extIngest.name} · {extDoneCount}/{extJobs.length} sources done
						</span>
						{#if extAnyRunning}
							<button class="act" disabled={extCancelling} onclick={cancelAllFromExtension}>
								{extCancelling ? 'Cancelling…' : 'Cancel all'}
							</button>
						{:else}
							<button class="act" onclick={dismissExtIngest}>Dismiss</button>
						{/if}
					</div>
					{#if extSkipped > 0}
						<p class="job-note">
							{extSkipped} source{extSkipped === 1 ? '' : 's'} skipped (NSFW or unavailable).
						</p>
					{/if}
					<div class="job-stats">
						<span class="jstat"><b>{extAgg.pagesDone}</b> pages</span>
						<span class="jstat"><b>{extAgg.itemsSeen}</b> seen</span>
						<span class="jstat ok"><b>{extAgg.succeeded}</b> succeeded</span>
						<span class="jstat new"><b>{extAgg.newWorks}</b> new</span>
						{#if extAgg.autoMerged > 0}<span class="jstat merged"
								><b>{extAgg.autoMerged}</b> merged</span
							>{/if}
						{#if extAgg.queuedForReview > 0}<span class="jstat review"
								><b>{extAgg.queuedForReview}</b> for review</span
							>{/if}
						{#if extAgg.alreadyExisting > 0}<span class="jstat existing"
								><b>{extAgg.alreadyExisting}</b> existing</span
							>{/if}
						{#if extAgg.failed > 0}<span class="jstat bad"><b>{extAgg.failed}</b> failed</span>{/if}
					</div>
					{#if extAnyRunning}
						<div class="job-progress"><div class="job-bar"></div></div>
					{/if}
					<div class="ext-rows">
						{#each extJobs as ej (ej.id)}
							<div class="ext-row">
								<span class="ext-row-state {ej.state}">{JOB_STATE_LABEL[ej.state] ?? ej.state}</span
								>
								<span class="ext-row-src">source {ej.sourceId}</span>
								<span class="ext-row-nums">
									{ej.itemsSeen} seen · {ej.newWorks} new · {ej.alreadyExisting} existing{ej.queuedForReview >
									0
										? ` · ${ej.queuedForReview} review`
										: ''}{ej.failed > 0 ? ` · ${ej.failed} failed` : ''}
								</span>
							</div>
						{/each}
					</div>
					{#if extAgg.queuedForReview > 0}
						<p class="review-hint">
							Matches queued for review can be resolved on the <a href="/review">Review</a> page.
						</p>
					{/if}
				</div>
			{/if}
			<div class="toolbar">
				<select bind:value={sourceId} onchange={onPickSource} aria-label="Pick a source">
					<option value="" disabled>
						{extScope && scopedSources.length !== 1 ? 'Pick a language…' : 'Pick a source…'}
					</option>
					{#each scopedSources as s (s.id)}
						<option value={String(s.id)}>{s.name} ({s.lang}){s.isNsfw ? ' · NSFW' : ''}</option>
					{/each}
				</select>
				<div class="segmented">
					<button class:on={browseType === 'POPULAR'} onclick={() => startBrowse('POPULAR')}
						>Popular</button
					>
					<button class:on={browseType === 'LATEST'} onclick={() => startBrowse('LATEST')}
						>Latest</button
					>
					<button class:on={browseType === 'SEARCH'} onclick={() => startBrowse('SEARCH')}
						>Search</button
					>
				</div>
				{#if browseType === 'SEARCH'}
					<form class="search-form" onsubmit={onSearch}>
						<input
							type="search"
							placeholder="Search this source…"
							bind:value={searchQuery}
							aria-label="Search the source"
						/>
						<button type="submit" disabled={browsing || !sourceId}>Search</button>
					</form>
				{/if}
				{#if sourceId}
					<button
						class="act primary add-all"
						disabled={jobStarting || job?.state === 'running'}
						onclick={addAllFromSource}
						title="Walk this source's whole catalogue through the dedup add flow in the background"
					>
						{jobStarting
							? 'Starting…'
							: job?.state === 'running'
								? 'Ingest running…'
								: 'Add all from this source'}
					</button>
				{/if}
			</div>

			{#if jobError}
				<div class="notice error poll-error">
					<span>
						{jobError}{pollStopped ? ' — progress updates paused after repeated failures.' : ''}
					</span>
					{#if pollStopped}
						<button class="act" onclick={retryPolling}>Retry</button>
					{/if}
				</div>
			{/if}

			{#if job}
				<div class="job" class:done={job.state !== 'running'}>
					<div class="job-head">
						<span class="job-title">
							<span class="job-state {job.state}">{JOB_STATE_LABEL[job.state] ?? job.state}</span>
							Add all from source {job.sourceId}
						</span>
						{#if job.state === 'running'}
							<button class="act" disabled={jobCancelling} onclick={cancelJob}>
								{jobCancelling ? 'Cancelling…' : 'Cancel'}
							</button>
						{:else}
							<button class="act" onclick={dismissJob}>Dismiss</button>
						{/if}
					</div>
					{#if job.error}<p class="job-err">{job.error}</p>{/if}
					<div class="job-stats">
						<span class="jstat"><b>{job.pagesDone}</b> pages</span>
						<span class="jstat"><b>{job.itemsSeen}</b> seen</span>
						<span class="jstat ok"><b>{job.succeeded}</b> succeeded</span>
						<span class="jstat new"><b>{job.newWorks}</b> new</span>
						{#if job.autoMerged > 0}<span class="jstat merged"><b>{job.autoMerged}</b> merged</span
							>{/if}
						{#if job.queuedForReview > 0}<span class="jstat review"
								><b>{job.queuedForReview}</b> for review</span
							>{/if}
						{#if job.alreadyExisting > 0}<span class="jstat existing"
								><b>{job.alreadyExisting}</b> existing</span
							>{/if}
						{#if job.failed > 0}<span class="jstat bad"><b>{job.failed}</b> failed</span>{/if}
					</div>
					{#if job.state === 'running'}
						<div class="job-progress"><div class="job-bar"></div></div>
					{/if}
					{#if job.queuedForReview > 0}
						<p class="review-hint">
							Matches queued for review can be resolved on the <a href="/review">Review</a> page.
						</p>
					{/if}
				</div>
			{/if}

			{#if picked.length > 0 || bulkResult || addError}
				<div class="bulk-bar">
					<span class="picked-count">
						{picked.length} selected{picked.length >= 100 ? ' (max 100)' : ''}
					</span>
					<button class="act primary" disabled={adding || picked.length === 0} onclick={addPicked}>
						{adding ? 'Adding…' : `Add selected to catalogue`}
					</button>
					{#if picked.length > 0}
						<button class="act" disabled={adding} onclick={() => (picked = [])}>Clear</button>
					{/if}
				</div>
			{/if}

			{#if addError}<div class="notice error">{addError}</div>{/if}

			{#if bulkResult}
				<div class="result">
					<div class="result-head">
						<h2>Ingest result</h2>
						<button class="act" onclick={() => (bulkResult = null)}>Dismiss</button>
					</div>
					<div class="summary">
						<span class="chip">{bulkResult.succeeded}/{bulkResult.total} succeeded</span>
						{#if bulkResult.failed > 0}<span class="chip bad">{bulkResult.failed} failed</span>{/if}
						{#if bulkResult.newWorks > 0}<span class="chip new">{bulkResult.newWorks} new</span
							>{/if}
						{#if bulkResult.autoMerged > 0}<span class="chip merged"
								>{bulkResult.autoMerged} auto-merged</span
							>{/if}
						{#if bulkResult.queuedForReview > 0}<span class="chip review"
								>{bulkResult.queuedForReview} for review</span
							>{/if}
						{#if bulkResult.alreadyExisting > 0}<span class="chip existing"
								>{bulkResult.alreadyExisting} already existing</span
							>{/if}
					</div>
					<div class="rtable" role="table" aria-label="Per-series ingest outcome">
						<div class="rrow head" role="row">
							<span role="columnheader">Series</span>
							<span role="columnheader">Decision</span>
							<span role="columnheader">Work</span>
							<span role="columnheader">Signal</span>
						</div>
						{#each bulkResult.entries as en (en.suwayomiMangaId)}
							<div class="rrow" role="row">
								<span class="work" role="cell">
									<span class="wtitle">{entryTitle(String(en.suwayomiMangaId))}</span>
									<span class="wid">suwayomi #{en.suwayomiMangaId}</span>
								</span>
								<span role="cell">
									{#if en.result}
										<span class="badge {decisionClass(en.result.decision)}"
											>{decisionLabel(en.result.decision)}</span
										>
									{:else}
										<span class="badge bad">failed</span>
									{/if}
								</span>
								<span class="work" role="cell">
									{#if en.result}
										<span class="wid">{en.result.workId}</span>
									{:else}
										<span class="err-msg">{en.error ?? 'Unknown error'}</span>
									{/if}
								</span>
								<span class="muted" role="cell">
									{#if en.result?.method}
										{en.result.method}{en.result.score != null
											? ` · ${Math.round(en.result.score * 100)}%`
											: ''}
									{:else}
										—
									{/if}
								</span>
							</div>
						{/each}
					</div>
					{#if bulkResult.queuedForReview > 0}
						<p class="review-hint">
							Matches queued for review can be resolved on the <a href="/review">Review</a> page.
						</p>
					{/if}
				</div>
			{/if}

			{#if browseError}
				<div class="notice error">{browseError}</div>
			{:else if !sourceId}
				{#if extScope && scopedSources.length === 0}
					<div class="notice">
						{extScope.name} is installed but the engine hasn't exposed any sources for it yet. Try again
						shortly, or pick another source.
					</div>
				{:else if extScope && scopedSources.length > 1}
					<div class="notice">Pick a language above to browse {extScope.name}.</div>
				{:else}
					<div class="notice">Pick a source above to browse its catalogue.</div>
				{/if}
			{:else if browsing && items.length === 0}
				<div class="notice">Loading {browseType.toLowerCase()}…</div>
			{:else if browsed && items.length === 0}
				<div class="notice">
					{browseType === 'SEARCH' ? 'No results for this search.' : 'This listing is empty.'}
				</div>
			{:else if !browsed && browseType === 'SEARCH'}
				<div class="notice">Enter a search above to browse this source.</div>
			{:else if items.length > 0}
				<div class="grid" class:dim={browsing}>
					{#each items as it (it.suwayomiMangaId)}
						{@const id = String(it.suwayomiMangaId)}
						<button
							class="card"
							class:picked={picked.includes(id)}
							onclick={() => togglePick(id)}
							aria-pressed={picked.includes(id)}
							title={it.title}
						>
							{#if it.thumbnailUrl}
								<img
									class="thumb"
									src={it.thumbnailUrl}
									alt=""
									loading="lazy"
									referrerpolicy="no-referrer"
								/>
							{:else}
								<div class="thumb placeholder"></div>
							{/if}
							<span class="card-title">{it.title}</span>
							{#if it.inLibrary}<span class="lib-badge">in library</span>{/if}
							<span class="check" aria-hidden="true">{picked.includes(id) ? '✓' : ''}</span>
						</button>
					{/each}
				</div>
				<div class="pager">
					<button
						class="act"
						disabled={browsing || pageNum <= 1}
						onclick={() => browse(pageNum - 1)}>Prev</button
					>
					<span class="page-no">Page {pageNum}</span>
					<button class="act" disabled={browsing || !hasNext} onclick={() => browse(pageNum + 1)}
						>Next</button
					>
				</div>
			{/if}
		{/if}
	{/if}
</div>

<style>
	.page {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-6);
	}
	.page-head {
		display: flex;
		align-items: flex-end;
		justify-content: space-between;
		gap: var(--k-space-6);
		flex-wrap: wrap;
	}
	h1 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 30px;
		letter-spacing: -0.02em;
		color: var(--k-text-bright);
	}
	.lede {
		margin-top: 6px;
		font-size: 13.5px;
		color: var(--k-text-dim);
		max-width: 60ch;
	}
	.tabs {
		display: flex;
		gap: 4px;
		padding: 4px;
		background: var(--k-surface);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
	}
	.tabs button {
		height: 34px;
		padding: 0 18px;
		border: none;
		border-radius: var(--k-radius);
		background: transparent;
		color: var(--k-text-dim);
		font-family: var(--k-font-sans);
		font-size: 13px;
		font-weight: 600;
		cursor: pointer;
	}
	.tabs button.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.maint {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--k-space-6);
		flex-wrap: wrap;
		padding: 16px 20px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-lg);
	}
	.maint-copy {
		display: flex;
		flex-direction: column;
		gap: 6px;
		min-width: 0;
		flex: 1 1 340px;
	}
	.maint-head {
		display: flex;
		flex-direction: column;
		gap: 4px;
	}
	.maint-kicker {
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.08em;
		text-transform: uppercase;
		color: var(--k-accent-teal);
	}
	.maint-head h2 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text-bright);
	}
	.maint-lede {
		font-size: 13px;
		color: var(--k-text-dim);
		max-width: 68ch;
		line-height: 1.45;
	}
	.maint-msg {
		font-size: 12.5px;
		font-weight: 600;
	}
	.maint-msg.ok {
		color: var(--k-accent-teal);
	}
	.maint-msg.error {
		color: #f0808a;
	}
	.maint-actions {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
	}
	.maint-confirm-q {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-1);
	}
	.notice {
		padding: 14px 16px;
		border-radius: var(--k-radius-md);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		color: var(--k-text-dim);
		font-size: 13.5px;
	}
	.notice.error {
		color: #f0808a;
		border-color: rgba(240, 128, 138, 0.4);
	}
	.repo-form {
		display: flex;
		gap: 8px;
		margin-top: 12px;
	}
	.repo-form input {
		flex: 1;
		max-width: 520px;
		height: 38px;
		padding: 0 12px;
		background: var(--k-surface);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 13px;
	}
	.repo-form button {
		height: 38px;
		padding: 0 16px;
		border: none;
		border-radius: var(--k-radius-md);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-family: var(--k-font-sans);
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
	}
	.repo-form button:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.repo-msg {
		margin-top: 10px;
		font-size: 12.5px;
		color: var(--k-text-2);
	}
	.toolbar {
		display: flex;
		align-items: center;
		gap: 12px;
		flex-wrap: wrap;
	}
	.segmented {
		display: flex;
		gap: 4px;
		padding: 4px;
		background: var(--k-surface);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
	}
	.segmented button {
		height: 32px;
		padding: 0 14px;
		border: none;
		border-radius: var(--k-radius);
		background: transparent;
		color: var(--k-text-dim);
		font-family: var(--k-font-sans);
		font-size: 12.5px;
		font-weight: 600;
		cursor: pointer;
	}
	.segmented button.on {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
	.toolbar select,
	.filter-q,
	.search-form input {
		height: 40px;
		padding: 0 12px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius-md);
		color: var(--k-text);
		font-family: var(--k-font-sans);
		font-size: 13.5px;
	}
	.toolbar select:focus,
	.filter-q:focus,
	.search-form input:focus {
		outline: none;
		border-color: var(--k-border-strong);
	}
	.filter-q {
		width: 220px;
	}
	.search-form {
		display: flex;
		gap: 8px;
	}
	.search-form input {
		width: 240px;
	}
	.search-form button {
		height: 40px;
		padding: 0 16px;
		border: none;
		border-radius: var(--k-radius-md);
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-family: var(--k-font-sans);
		font-weight: 700;
		font-size: 13px;
		cursor: pointer;
	}
	.search-form button:disabled {
		opacity: 0.6;
		cursor: default;
	}

	/* extensions table */
	.table {
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-lg);
		overflow: hidden;
		transition: opacity 0.15s;
	}
	.table.dim {
		opacity: 0.6;
	}
	.thead,
	.row {
		display: grid;
		grid-template-columns: minmax(260px, 3fr) 70px 170px minmax(200px, 2fr);
		align-items: center;
		gap: 16px;
		padding: 12px 20px;
	}
	.thead {
		background: var(--k-surface);
		border-bottom: 1px solid var(--k-border);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.thead .right {
		text-align: right;
	}
	.row {
		border-bottom: 1px solid rgba(255, 255, 255, 0.04);
		font-size: 14px;
		color: var(--k-text-2);
	}
	.row:last-child {
		border-bottom: none;
	}
	.row:hover {
		background: var(--k-hover-fill);
	}
	.cell-ext {
		display: flex;
		align-items: center;
		gap: 14px;
		min-width: 0;
	}
	.icon {
		flex: 0 0 auto;
		width: 36px;
		height: 36px;
		object-fit: cover;
		border-radius: 8px;
		background: var(--k-cover);
		border: 1px solid var(--k-border-1);
	}
	.icon.placeholder {
		background-image: var(--k-hatch);
	}
	.titles {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}
	.title {
		font-weight: 600;
		color: var(--k-text-1);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.meta {
		font-size: 12px;
		color: var(--k-text-fainter);
		font-family: var(--k-font-mono, monospace);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.nsfw {
		display: inline-block;
		margin-left: 8px;
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.05em;
		color: var(--k-accent);
		border: 1px solid rgba(224, 131, 105, 0.4);
		border-radius: var(--k-radius-sm);
		padding: 1px 5px;
		vertical-align: middle;
	}
	.installed-badge {
		display: inline-block;
		margin-left: 8px;
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.05em;
		color: var(--k-accent-teal);
		border: 1px solid rgba(95, 200, 207, 0.4);
		border-radius: var(--k-radius-sm);
		padding: 1px 5px;
		vertical-align: middle;
	}
	.lang {
		font-size: 13px;
		color: var(--k-text-2);
	}
	.version {
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-size: 13px;
		color: var(--k-text-2);
	}
	.update-badge {
		font-size: 11px;
		font-weight: 700;
		color: var(--k-hiatus);
	}
	.actions {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: 8px;
		flex-wrap: wrap;
	}
	.row-msg {
		font-size: 12px;
		color: var(--k-text-faint);
		max-width: 260px;
	}
	.act {
		height: 32px;
		padding: 0 14px;
		border-radius: var(--k-radius);
		background: transparent;
		border: 1px solid var(--k-border-4);
		color: var(--k-text-1);
		font-family: var(--k-font-sans);
		font-weight: 600;
		font-size: 12.5px;
		cursor: pointer;
		transition: all 0.15s;
		white-space: nowrap;
	}
	.act:hover:not(:disabled) {
		border-color: var(--k-border-strong);
	}
	.act:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.act.primary {
		background: var(--k-primary);
		border-color: transparent;
		color: var(--k-on-primary);
	}

	/* catalogue grid */
	.bulk-bar {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 12px 16px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
	}
	.picked-count {
		font-size: 13px;
		font-weight: 600;
		color: var(--k-text-1);
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
		gap: 14px;
		transition: opacity 0.15s;
	}
	.grid.dim {
		opacity: 0.6;
	}
	.card {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 8px;
		padding: 0;
		background: transparent;
		border: 2px solid transparent;
		border-radius: var(--k-radius-md);
		cursor: pointer;
		text-align: left;
		font-family: var(--k-font-sans);
	}
	.card.picked {
		border-color: var(--k-accent-teal);
	}
	.thumb {
		width: 100%;
		aspect-ratio: 5 / 7;
		object-fit: cover;
		border-radius: var(--k-radius);
		background: var(--k-cover);
		border: 1px solid var(--k-border-1);
	}
	.thumb.placeholder {
		background-image: var(--k-hatch);
	}
	.card-title {
		font-size: 12.5px;
		font-weight: 600;
		color: var(--k-text-1);
		line-height: 1.3;
		display: -webkit-box;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		-webkit-box-orient: vertical;
		overflow: hidden;
		padding: 0 4px 6px;
	}
	.lib-badge {
		position: absolute;
		top: 8px;
		left: 8px;
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--k-on-primary);
		background: var(--k-primary);
		border-radius: var(--k-radius-sm);
		padding: 2px 6px;
	}
	.check {
		position: absolute;
		top: 8px;
		right: 8px;
		width: 24px;
		height: 24px;
		display: grid;
		place-items: center;
		border-radius: 50%;
		background: rgba(6, 6, 7, 0.65);
		border: 1px solid var(--k-border-strong);
		color: var(--k-accent-teal);
		font-size: 13px;
		font-weight: 800;
	}
	.card.picked .check {
		background: var(--k-accent-teal);
		color: #06181a;
		border-color: transparent;
	}
	.pager {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 14px;
	}
	.page-no {
		font-size: 13px;
		color: var(--k-text-dim);
	}

	/* bulk result (mirrors the review page's table) */
	.result {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-4);
		padding: var(--k-space-5);
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-lg);
	}
	.result-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.result-head h2 {
		font-family: var(--k-font-display);
		font-weight: 700;
		font-size: 18px;
		color: var(--k-text-bright);
	}
	.summary {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.chip {
		font-size: 12px;
		font-weight: 700;
		padding: 4px 10px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	.chip.bad {
		background: rgba(240, 128, 138, 0.15);
		color: #f0808a;
	}
	.chip.new {
		background: rgba(127, 211, 154, 0.15);
		color: var(--k-ongoing);
	}
	.chip.merged {
		background: rgba(95, 200, 207, 0.15);
		color: var(--k-accent-teal);
	}
	.chip.review {
		background: rgba(224, 179, 84, 0.15);
		color: var(--k-hiatus);
	}
	.chip.existing {
		background: rgba(143, 184, 255, 0.15);
		color: var(--k-completed);
	}
	.rtable {
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
		overflow: hidden;
	}
	.rrow {
		display: grid;
		grid-template-columns: 2fr 1.2fr 2fr 1.2fr;
		align-items: center;
		gap: var(--k-space-4);
		padding: 10px 14px;
		border-bottom: 1px solid var(--k-border);
	}
	.rrow:last-child {
		border-bottom: none;
	}
	.rrow.head {
		background: var(--k-surface);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.work {
		display: flex;
		flex-direction: column;
		gap: 2px;
		min-width: 0;
	}
	.wtitle {
		font-size: 13.5px;
		font-weight: 600;
		color: var(--k-text-1);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.wid {
		font-size: 11px;
		color: var(--k-text-faint);
		font-family: var(--k-font-mono, monospace);
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.err-msg {
		font-size: 12px;
		color: #f0808a;
	}
	.muted {
		font-size: 12.5px;
		color: var(--k-text-dim);
	}
	.badge {
		display: inline-block;
		font-size: 11px;
		font-weight: 700;
		padding: 3px 9px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	.badge.new {
		background: rgba(127, 211, 154, 0.15);
		color: var(--k-ongoing);
	}
	.badge.merged {
		background: rgba(95, 200, 207, 0.15);
		color: var(--k-accent-teal);
	}
	.badge.review {
		background: rgba(224, 179, 84, 0.15);
		color: var(--k-hiatus);
	}
	.badge.existing {
		background: rgba(143, 184, 255, 0.15);
		color: var(--k-completed);
	}
	.badge.bad {
		background: rgba(240, 128, 138, 0.15);
		color: #f0808a;
	}
	.review-hint {
		font-size: 12.5px;
		color: var(--k-text-dim);
	}
	.review-hint a {
		color: var(--k-accent-teal);
	}

	/* extension → catalogue */
	.hint {
		font-size: 12.5px;
		color: var(--k-text-faint);
	}
	.row.clickable {
		cursor: pointer;
	}
	.row.clickable:focus-visible {
		outline: 2px solid var(--k-accent-teal);
		outline-offset: -2px;
	}
	.scope-bar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 10px 14px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
	}
	.scope-label {
		font-size: 13px;
		color: var(--k-text-2);
	}
	.scope-label strong {
		color: var(--k-text-bright);
	}
	.scope-actions {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.add-all {
		margin-left: auto;
	}

	/* extension-wide aggregate ingest panel */
	.ext-job {
		border-color: var(--k-border-strong);
	}
	.job-note {
		font-size: 12px;
		color: var(--k-hiatus);
	}
	.ext-rows {
		display: flex;
		flex-direction: column;
		gap: 4px;
		max-height: 220px;
		overflow-y: auto;
		border-top: 1px solid var(--k-border);
		padding-top: 8px;
	}
	.ext-row {
		display: grid;
		grid-template-columns: 90px minmax(120px, 1fr) 2fr;
		align-items: center;
		gap: 10px;
		font-size: 12px;
	}
	.ext-row-state {
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.04em;
		text-transform: uppercase;
		padding: 2px 6px;
		border-radius: var(--k-radius-sm);
		text-align: center;
	}
	.ext-row-state.running {
		color: var(--k-accent-teal);
		background: rgba(95, 200, 207, 0.12);
	}
	.ext-row-state.completed {
		color: var(--k-ongoing);
		background: rgba(127, 211, 154, 0.12);
	}
	.ext-row-state.cancelled {
		color: var(--k-hiatus);
		background: rgba(224, 179, 84, 0.12);
	}
	.ext-row-state.failed {
		color: #f0808a;
		background: rgba(240, 128, 138, 0.12);
	}
	.ext-row-src {
		color: var(--k-text-faint);
		font-family: var(--k-font-mono, monospace);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.ext-row-nums {
		color: var(--k-text-dim);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* ingest job progress */
	.poll-error {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
	}
	.job {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding: 14px 16px;
		background: var(--k-surface-2);
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-lg);
	}
	.job.done {
		opacity: 0.92;
	}
	.job-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
	}
	.job-title {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 13.5px;
		font-weight: 600;
		color: var(--k-text-1);
	}
	.job-state {
		font-size: 10px;
		font-weight: 800;
		letter-spacing: 0.05em;
		text-transform: uppercase;
		padding: 2px 7px;
		border-radius: var(--k-radius-sm);
		border: 1px solid transparent;
	}
	.job-state.running {
		color: var(--k-accent-teal);
		background: rgba(95, 200, 207, 0.12);
		border-color: rgba(95, 200, 207, 0.35);
	}
	.job-state.completed {
		color: var(--k-ongoing);
		background: rgba(127, 211, 154, 0.12);
		border-color: rgba(127, 211, 154, 0.35);
	}
	.job-state.cancelled {
		color: var(--k-hiatus);
		background: rgba(224, 179, 84, 0.12);
		border-color: rgba(224, 179, 84, 0.35);
	}
	.job-state.failed {
		color: #f0808a;
		background: rgba(240, 128, 138, 0.12);
		border-color: rgba(240, 128, 138, 0.35);
	}
	.job-err {
		font-size: 12.5px;
		color: #f0808a;
	}
	.job-stats {
		display: flex;
		flex-wrap: wrap;
		gap: 14px;
	}
	.jstat {
		font-size: 12.5px;
		color: var(--k-text-dim);
	}
	.jstat b {
		color: var(--k-text-1);
		font-weight: 700;
	}
	.jstat.ok b {
		color: var(--k-ongoing);
	}
	.jstat.new b {
		color: var(--k-ongoing);
	}
	.jstat.merged b {
		color: var(--k-accent-teal);
	}
	.jstat.review b {
		color: var(--k-hiatus);
	}
	.jstat.existing b {
		color: var(--k-completed);
	}
	.jstat.bad b {
		color: #f0808a;
	}
	.job-progress {
		height: 4px;
		border-radius: 2px;
		background: var(--k-surface-4);
		overflow: hidden;
	}
	.job-bar {
		height: 100%;
		width: 40%;
		border-radius: 2px;
		background: var(--k-accent-teal);
		animation: indeterminate 1.4s ease-in-out infinite;
	}
	@keyframes indeterminate {
		0% {
			margin-left: -40%;
		}
		100% {
			margin-left: 100%;
		}
	}

	/* scan health (E4.3) — its own grid, deliberately not sharing `.rrow`, whose
	   4-column template belongs to the ingest-outcome table. */
	.tab-alarm {
		display: inline-block;
		margin-left: 6px;
		min-width: 16px;
		padding: 0 5px;
		border-radius: var(--k-radius-pill);
		background: rgba(240, 128, 138, 0.18);
		color: #f0808a;
		font-size: 11px;
		font-weight: 700;
	}
	.outage-list {
		margin: 8px 0 0;
		padding-left: 18px;
		font-size: 12.5px;
		line-height: 1.6;
	}
	.htable {
		border: 1px solid var(--k-border);
		border-radius: var(--k-radius-md);
		overflow: hidden;
	}
	.hrow {
		display: grid;
		grid-template-columns: minmax(0, 2.4fr) repeat(6, minmax(0, 0.7fr)) minmax(0, 1.2fr);
		align-items: center;
		gap: var(--k-space-3, 8px);
		padding: 9px 14px;
		border-bottom: 1px solid var(--k-border);
		font-size: 13px;
		font-variant-numeric: tabular-nums;
	}
	.hrow:last-child {
		border-bottom: none;
	}
	.hrow.head {
		background: var(--k-surface);
		font-size: 11px;
		font-weight: 700;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--k-text-faint);
	}
	.hrow.down {
		background: rgba(240, 128, 138, 0.06);
	}
	.src-cell {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
	}
	.src-cell strong {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.hrow .bad {
		color: #f0808a;
		font-weight: 600;
	}
	.sub {
		margin-left: 4px;
		font-size: 11px;
		font-style: normal;
		color: var(--k-text-faint);
	}
	.when {
		font-size: 11.5px;
		color: var(--k-text-dim);
		white-space: nowrap;
	}
	.pill {
		flex: none;
		font-size: 10.5px;
		font-weight: 700;
		padding: 2px 7px;
		border-radius: var(--k-radius-pill);
		background: var(--k-surface-4);
		color: var(--k-text-2);
	}
	.pill.bad {
		background: rgba(240, 128, 138, 0.15);
		color: #f0808a;
	}
	.pill.warn {
		background: rgba(224, 179, 84, 0.15);
		color: var(--k-hiatus);
	}
</style>
