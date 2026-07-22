<script lang="ts">
	import type {
		BulkAddResult,
		ExtensionInfo,
		SourceBrowseEntry,
		SourceBrowseType,
		SourceIngestJob,
		SourceInfo,
	} from '@komika/types';
	import { tick } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { auth } from '$lib/auth.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import Segmented from '$lib/components/Segmented.svelte';
	import SourceReader from '$lib/components/SourceReader.svelte';
	import {
		browseSource,
		bulkAddSourceSeries,
		cancelExtensionIngest,
		cancelSourceIngest,
		loadExtensions,
		loadSourceIngestJobs,
		loadSources,
		startExtensionIngest,
		startSourceIngest,
	} from '$lib/data';
	import IngestCounts, { type CountItem } from './IngestCounts.svelte';
	import IngestPanel from './IngestPanel.svelte';

	// Auth redirect is centralized in +layout.svelte.

	const MAX_SOURCE_PAGE = 100_000;
	/** The server takes at most this many series in one "add selected" request. */
	const MAX_PICKED = 100;

	function sourcePageFromUrl(raw: string | null): number {
		if (raw == null || raw.trim() === '') return 1;
		const parsed = Number(raw);
		return Number.isInteger(parsed) && parsed >= 1 && parsed <= MAX_SOURCE_PAGE ? parsed : 1;
	}

	const extParam = $derived(page.url.searchParams.get('extension'));
	const sourceParam = $derived(page.url.searchParams.get('source') ?? '');
	const listingParam = $derived(page.url.searchParams.get('listing'));
	const currentBrowseType = $derived<SourceBrowseType>(
		listingParam === 'latest' ? 'LATEST' : listingParam === 'search' ? 'SEARCH' : 'POPULAR',
	);
	const qParam = $derived((page.url.searchParams.get('q') ?? '').trim());
	const pageParamRaw = $derived(page.url.searchParams.get('page'));
	const currentPageNum = $derived(sourcePageFromUrl(pageParamRaw));

	const LISTINGS: { value: SourceBrowseType; label: string }[] = [
		{ value: 'POPULAR', label: 'Popular' },
		{ value: 'LATEST', label: 'Latest' },
		{ value: 'SEARCH', label: 'Search' },
	];

	// ---- Sources ---------------------------------------------------------------
	let sources = $state<SourceInfo[]>([]);
	let sourcesLoading = $state(false);
	let sourcesLoaded = $state(false);
	let sourcesError = $state<string | null>(null);

	const validSource = $derived(
		sourceParam
			? sources.find((s) => String(s.id) === sourceParam && (!extParam || s.pkgName === extParam))
			: null,
	);
	const validSourceId = $derived(validSource ? String(validSource.id) : '');

	let searchDraft = $state('');
	$effect(() => {
		searchDraft = qParam;
	});

	let hasNext = $state(false);
	let items = $state<SourceBrowseEntry[]>([]);
	let browsing = $state(false);
	let browsed = $state(false);
	let browseError = $state<string | null>(null);

	/** Monotonic token guard for browse requests so stale responses cannot overwrite state. */
	let browseGen = 0;

	/** Selected suwayomiMangaIds (insertion-ordered). */
	let picked = $state<string[]>([]);
	/** Titles for every id seen while browsing, so bulk results can be labelled. */
	let titles = $state<Record<string, string>>({});

	let adding = $state(false);
	let bulkResult = $state<BulkAddResult | null>(null);
	let resultEl = $state<HTMLElement | null>(null);
	/** Source comic opened in the non-mutating admin reader preview. */
	let reading = $state<{ id: string; title: string } | null>(null);
	/** Covers the source refused to serve: fall back to the placeholder. */
	let failedThumbs = $state<Record<string, boolean>>({});

	function buildBrowseUrl(opts: {
		extension?: string | null;
		source?: string | null;
		listing?: SourceBrowseType;
		q?: string | null;
		page?: number | null;
	}): string {
		const sp = new URLSearchParams(page.url.searchParams);
		// `tab` belonged to the old combined Sources page; drop it if a redirect carried it over.
		for (const key of ['tab', 'extension', 'source', 'listing', 'q', 'page']) sp.delete(key);
		if (opts.extension) {
			sp.set('extension', opts.extension);
		}
		if (opts.source) {
			sp.set('source', opts.source);
		}
		if (opts.listing === 'LATEST') {
			sp.set('listing', 'latest');
		} else if (opts.listing === 'SEARCH') {
			sp.set('listing', 'search');
			if (opts.q && opts.q.trim()) {
				sp.set('q', opts.q.trim());
			}
		}
		if (opts.page && opts.page > 1) {
			sp.set('page', String(opts.page));
		}
		const qs = sp.toString();
		return qs ? `/sources/browse?${qs}` : '/sources/browse';
	}

	function onSourceChange(ev: Event): void {
		const target = ev.target as HTMLSelectElement;
		const nextSourceId = target.value;
		const url = buildBrowseUrl({
			extension: extParam,
			source: nextSourceId || undefined,
			listing: currentBrowseType,
			q: currentBrowseType === 'SEARCH' ? qParam : undefined,
			page: 1,
		});
		void goto(url);
	}

	function onPickListing(type: SourceBrowseType): void {
		if (type === currentBrowseType && currentPageNum === 1) return;
		const url = buildBrowseUrl({
			extension: extParam,
			source: validSourceId || undefined,
			listing: type,
			q: type === 'SEARCH' ? qParam : undefined,
			page: 1,
		});
		void goto(url);
	}

	function onSearch(ev: SubmitEvent): void {
		ev.preventDefault();
		const query = searchDraft.trim();
		if (!query || !validSourceId) return;
		const url = buildBrowseUrl({
			extension: extParam,
			source: validSourceId,
			listing: 'SEARCH',
			q: query,
			page: 1,
		});
		if (url === `${page.url.pathname}${page.url.search}`) {
			// Re-submitting the active search doubles as a retry after a failed request.
			void runBrowse(validSourceId, 'SEARCH', 1, query);
		} else {
			void goto(url);
		}
	}

	function goToPage(targetPage: number): void {
		if (browsing || targetPage < 1) return;
		const url = buildBrowseUrl({
			extension: extParam,
			source: validSourceId || undefined,
			listing: currentBrowseType,
			q: currentBrowseType === 'SEARCH' ? qParam : undefined,
			page: targetPage,
		});
		void goto(url, { keepFocus: true, noScroll: true });
	}

	async function refreshSources(): Promise<void> {
		sourcesLoading = true;
		sourcesError = null;
		try {
			sources = await loadSources();
			sourcesLoaded = true;
		} catch (err) {
			sourcesError = err instanceof Error ? err.message : 'Failed to load sources.';
			sources = [];
		} finally {
			sourcesLoaded = true;
			sourcesLoading = false;
		}
	}

	async function runBrowse(
		srcId: string,
		bType: SourceBrowseType,
		p: number,
		query?: string,
	): Promise<void> {
		if (!srcId) {
			browseGen++;
			items = [];
			browsed = false;
			browseError = null;
			browsing = false;
			hasNext = false;
			return;
		}
		if (bType === 'SEARCH' && !query?.trim()) {
			browseGen++;
			items = [];
			browsed = false;
			browseError = null;
			browsing = false;
			hasNext = false;
			return;
		}
		const gen = ++browseGen;
		browsing = true;
		browseError = null;
		try {
			const res = await browseSource(
				srcId,
				bType,
				p,
				bType === 'SEARCH' ? query?.trim() : undefined,
			);
			if (gen !== browseGen) return;
			items = res.items;
			hasNext = res.hasNextPage;
			browsed = true;
			const t = { ...titles };
			for (const it of res.items) t[String(it.suwayomiMangaId)] = it.title;
			titles = t;
		} catch (err) {
			if (gen !== browseGen) return;
			browseError = err instanceof Error ? err.message : 'Failed to browse the source.';
			items = [];
			hasNext = false;
			browsed = true;
		} finally {
			if (gen === browseGen) {
				browsing = false;
			}
		}
	}

	function retryBrowse(): void {
		void runBrowse(
			validSourceId,
			currentBrowseType,
			currentPageNum,
			currentBrowseType === 'SEARCH' ? qParam : undefined,
		);
	}

	function togglePick(id: string): void {
		if (picked.includes(id)) picked = picked.filter((x) => x !== id);
		else if (picked.length < MAX_PICKED) picked = [...picked, id];
	}

	async function addPicked(): Promise<void> {
		if (adding || picked.length === 0) return;
		adding = true;
		bulkResult = null;
		// What was actually sent. The grid stays usable while this runs, so by the time
		// it answers the selection may hold new picks, or belong to another source.
		const submitted = picked;
		const forSource = validSourceId;
		try {
			const result = await bulkAddSourceSeries(submitted);
			picked = picked.filter((id) => !submitted.includes(id));
			if (result.failed > 0) {
				toast.error(
					`${result.failed} of ${result.total} series could not be added. See the result for why.`,
				);
			} else {
				toast.success(
					`${result.succeeded} series handled. See the result for what happened to each.`,
				);
			}
			// The per-series result belongs to the source it was added from.
			if (forSource !== validSourceId) return;
			bulkResult = result;
			// The result sits above the grid, and the admin is usually scrolled past it.
			await tick();
			resultEl?.scrollIntoView({ block: 'start' });
		} catch (err) {
			toast.fromError(err, 'Could not add the selected series.');
		} finally {
			adding = false;
		}
	}

	/** Same wording as the catalog page's add flow. */
	function decisionLabel(decision: string): string {
		switch (decision) {
			case 'auto_merge':
				return 'Merged into an existing work';
			case 'review':
				return 'Sent to Duplicates for review';
			case 'new':
				return 'Added as a new work';
			case 'existing':
				return 'Already in the catalogue';
			default:
				return decision;
		}
	}

	function decisionTone(decision: string): 'info' | 'warning' | 'success' | undefined {
		switch (decision) {
			case 'auto_merge':
				return 'info';
			case 'review':
				return 'warning';
			case 'new':
				return 'success';
			default:
				return undefined;
		}
	}

	/** `title_corroborated` reads as "Title corroborated". */
	function methodLabel(method: string): string {
		const words = method.replace(/_/g, ' ').trim();
		return words ? words.charAt(0).toUpperCase() + words.slice(1) : 'Unknown';
	}

	function entryTitle(id: string): string {
		return titles[id] ?? `Series ${id}`;
	}

	function sourceLabel(id: string): string {
		return sources.find((s) => String(s.id) === id)?.name ?? `Source ${id}`;
	}

	// ---- Extension scope -------------------------------------------------------
	// `?extension=<pkgName>` narrows the source picker to one extension's sources.
	// The Extensions page links here with only that param, so the name and the
	// source list are resolved on this page.
	const extMatches = $derived(extParam ? sources.filter((s) => s.pkgName === extParam) : []);

	/** The extension list entry, looked up only when no source carries the name. */
	let extInfo = $state<ExtensionInfo | null>(null);
	let extLookup = $state<'idle' | 'loading' | 'done'>('idle');
	let extLookupFor = '';

	$effect(() => {
		if (!auth.user || !extParam) return;
		if (!sourcesLoaded || sourcesLoading || sourcesError) return;
		if (extMatches.length > 0 || extLookupFor === extParam) return;
		const pkgName = extParam;
		extLookupFor = pkgName;
		extInfo = null;
		extLookup = 'loading';
		loadExtensions(false)
			.then((list) => {
				if (extLookupFor === pkgName) extInfo = list.find((e) => e.pkgName === pkgName) ?? null;
			})
			.catch(() => {
				/* non-fatal: without the list the scope reads as "no installed source" */
			})
			.finally(() => {
				if (extLookupFor === pkgName) extLookup = 'done';
			});
	});

	const extKnown = $derived(extInfo && extInfo.pkgName === extParam ? extInfo : null);

	/** When set, the source picker is scoped to one extension's sources. */
	const extScope = $derived<{ pkgName: string; name: string } | null>(
		extParam && sourcesLoaded
			? { pkgName: extParam, name: extMatches[0]?.name ?? extKnown?.name ?? extParam }
			: null,
	);
	const scopedSources = $derived(extScope ? extMatches : sources);
	/** Scoped to an extension that has no source to browse. */
	const scopeEmpty = $derived(!!extScope && scopedSources.length === 0);

	const clearScopeHref = $derived(
		buildBrowseUrl({
			extension: null,
			source: undefined,
			listing: 'POPULAR',
			page: 1,
		}),
	);

	// ---- "Add all from this source" background job ------------------------------
	let job = $state<SourceIngestJob | null>(null);
	let jobStarting = $state(false);
	let jobCancelling = $state(false);
	/** The last progress-poll failure. Start and stop failures go to a toast instead. */
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
	/** Set on unmount: a poll that resolves afterwards must not schedule another tick. */
	let destroyed = false;

	function stopPolling(): void {
		if (pollTimer) {
			clearTimeout(pollTimer);
			pollTimer = null;
		}
	}

	function scheduleNextPoll(delayMs: number): void {
		stopPolling();
		if (destroyed) return;
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
		const gen = jobGen;
		// An answer is stale once another source was selected or the panel dismissed.
		// It must not bring the old job back or restart its polling.
		const stale = () => gen !== jobGen || !job || String(job.id) !== id;
		try {
			const active = await loadSourceIngestJobs(true);
			if (stale()) return;
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
			if (stale()) return;
			const term = all.find((j) => String(j.id) === id);
			if (term) job = term;
			pollFailures = 0;
			jobError = null;
			stopPolling();
		} catch (err) {
			if (stale()) return;
			pollFailures += 1;
			jobError = err instanceof Error ? err.message : 'Failed to load the job progress.';
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
	async function checkActiveJob(forSource: string): Promise<void> {
		stopPolling();
		job = null;
		jobError = null;
		const gen = ++jobGen;
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
		if (!validSource || jobStarting || job?.state === 'running') return;
		const sourceId = validSourceId;
		// This can add thousands of series, so it never starts on a single click.
		const ok = await confirm({
			title: `Add every series from ${validSource.name}?`,
			message:
				'This reads the whole source in the background and can add thousands of series. Likely duplicates are merged or sent to Duplicates for review. You can stop it at any time.',
			confirmLabel: 'Add all',
		});
		if (!ok || sourceId !== validSourceId || jobStarting || job?.state === 'running') return;
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
			toast.success('Started. Progress shows on this page.');
		} catch (err) {
			if (gen === jobGen) toast.fromError(err, 'Could not start adding from this source.');
		} finally {
			jobStarting = false;
		}
	}

	async function cancelJob(): Promise<void> {
		if (!job || jobCancelling) return;
		const id = String(job.id);
		const ok = await confirm({
			title: 'Stop adding from this source?',
			message: 'Series added so far stay in the catalogue. You can start again later.',
			confirmLabel: 'Stop adding',
			cancelLabel: 'Keep running',
			danger: true,
		});
		if (!ok || !job || String(job.id) !== id || jobCancelling) return;
		jobCancelling = true;
		const gen = jobGen;
		try {
			const stopped = await cancelSourceIngest(job.id);
			// The source changed meanwhile: the job is stopped, but it is not this panel's.
			if (gen !== jobGen || !job || String(job.id) !== id) return;
			job = stopped;
			if (job.state !== 'running') stopPolling();
			toast.success('Stopped. Series added so far stay in the catalogue.');
		} catch (err) {
			toast.fromError(err, 'Could not stop the job.');
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
		cancelled: 'Stopped',
		failed: 'Failed',
	};

	function jobTone(state: string): 'info' | 'success' | 'danger' | undefined {
		return state === 'running'
			? 'info'
			: state === 'completed'
				? 'success'
				: state === 'failed'
					? 'danger'
					: undefined;
	}

	interface JobCounts {
		pagesDone: number;
		itemsSeen: number;
		succeeded: number;
		newWorks: number;
		autoMerged: number;
		queuedForReview: number;
		alreadyExisting: number;
		failed: number;
	}

	function jobCounts(c: JobCounts): CountItem[] {
		return [
			{ label: 'Pages read', value: c.pagesDone },
			{ label: 'Series found', value: c.itemsSeen },
			{ label: 'Succeeded', value: c.succeeded },
			{ label: 'Added as new', value: c.newWorks },
			{ label: 'Merged', value: c.autoMerged },
			{ label: 'Sent to Duplicates', value: c.queuedForReview },
			{ label: 'Already in catalogue', value: c.alreadyExisting },
			{ label: 'Failed', value: c.failed, tone: c.failed > 0 ? 'danger' : undefined },
		];
	}

	// ---- "Add all from this extension" background jobs ---------------------------
	// One job per installed source of an extension, tracked together as an
	// aggregate. Independent of the per-source `job` above so both can coexist.
	let extIngest = $state<{ pkgName: string; name: string } | null>(null);
	let extJobs = $state<SourceIngestJob[]>([]);
	/** Visible source count when the run started — for honest "skipped" reporting. */
	let extVisibleSources = $state(0);
	let extStarting = $state(false);
	let extCancelling = $state(false);
	/** The last progress-poll failure. Start and stop failures go to a toast instead. */
	let extError = $state<string | null>(null);
	let extPollStopped = $state(false);
	/** Generation token: bumped on each new extension run / scope change. */
	let extGen = 0;
	let extPollTimer: ReturnType<typeof setTimeout> | null = null;
	let extPollFailures = 0;

	const extAgg = $derived(
		extJobs.reduce<JobCounts>(
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
		if (destroyed) return;
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
			extError = err instanceof Error ? err.message : 'Failed to load the job progress.';
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
		const pkgName = extScope.pkgName;
		const name = extScope.name;
		const visible = scopedSources.length;
		// One job per source, each of which can add thousands of series.
		const ok = await confirm({
			title: `Add every series from ${name}?`,
			message: `This starts a background job for ${
				visible === 1 ? 'its 1 source' : `each of its ${visible} sources`
			} and can add thousands of series. Likely duplicates are merged or sent to Duplicates for review. You can stop it at any time.`,
			confirmLabel: 'Add all',
		});
		if (!ok || extScope?.pkgName !== pkgName || extStarting || extAnyRunning) return;
		extStarting = true;
		extError = null;
		const gen = ++extGen;
		stopExtPolling();
		try {
			const jobs = await startExtensionIngest(pkgName);
			if (gen !== extGen) return;
			extIngest = { pkgName, name };
			extJobs = jobs;
			extVisibleSources = visible;
			extPollStopped = false;
			extPollFailures = 0;
			if (jobs.some((j) => j.state === 'running')) startExtPolling();
			toast.success('Started. Progress shows on this page.');
		} catch (err) {
			if (gen === extGen) toast.fromError(err, 'Could not start adding from this extension.');
		} finally {
			extStarting = false;
		}
	}

	async function cancelAllFromExtension(): Promise<void> {
		if (!extIngest || extCancelling) return;
		const pkgName = extIngest.pkgName;
		const ok = await confirm({
			title: `Stop adding from ${extIngest.name}?`,
			message: `This stops ${
				extRunningCount === 1 ? 'the 1 running job' : `all ${extRunningCount} running jobs`
			}. Series added so far stay in the catalogue. You can start again later.`,
			confirmLabel: 'Stop all',
			cancelLabel: 'Keep running',
			danger: true,
		});
		if (!ok || extIngest?.pkgName !== pkgName || extCancelling) return;
		extCancelling = true;
		try {
			const cancelled = await cancelExtensionIngest(extIngest.pkgName);
			const byId = new Map(cancelled.map((j) => [String(j.id), j]));
			extJobs = extJobs.map((j) => byId.get(String(j.id)) ?? j);
			// Any still showing running just completed between poll and cancel — one
			// more reconcile catches their terminal state; otherwise stop cleanly.
			if (extJobs.some((j) => j.state === 'running')) scheduleExtPoll(0);
			else stopExtPolling();
			toast.success('Stopped. Series added so far stay in the catalogue.');
		} catch (err) {
			toast.fromError(err, 'Could not stop the jobs.');
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
	 * On opening an extension's sources, resume an already-running extension
	 * run for it (survives navigation), and clear a stale panel left over from
	 * a *different* extension.
	 */
	async function syncExtPanel(
		pkgName: string,
		name: string,
		srcIds: string[],
		scopeGen: number,
	): Promise<void> {
		const ingestGen = extGen;
		try {
			const active = await loadSourceIngestJobs(true);
			if (scopeGen !== extScopeGen || ingestGen !== extGen) return;
			const idSet = new Set(srcIds);
			const mine = active.filter((j) => idSet.has(String(j.sourceId)));
			if (mine.length > 0) {
				extGen++; // supersede any prior extension poll loop
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

	$effect(() => {
		if (!auth.user || sourcesLoaded || sourcesLoading) return;
		void refreshSources();
	});

	// Keep hand-edited/deep-linked URLs canonical once source ids can be validated.
	// Waiting for `sourcesLoaded` avoids deleting a valid source while the picker is
	// still loading. Defaults and invalid values are repaired in place.
	$effect(() => {
		if (!auth.user) return;
		if (!sourcesLoaded || sourcesLoading || sourcesError) return;

		// An extension with a single source goes straight to it; with several, the
		// admin picks the per-language variant.
		const source =
			validSourceId || (extParam && extMatches.length === 1 ? String(extMatches[0].id) : '');
		const canPage = !!source && (currentBrowseType !== 'SEARCH' || qParam.length > 0);
		const href = buildBrowseUrl({
			extension: extParam,
			source: source || null,
			listing: currentBrowseType,
			q: source ? qParam : null,
			page: canPage ? currentPageNum : 1,
		});
		if (href !== `${page.url.pathname}${page.url.search}`) {
			void goto(href, { replaceState: true, noScroll: true, keepFocus: true });
		}
	});

	let lastCheckedJobSource = '';
	let lastTrackedSourceId = '';
	let lastExtScopeKey = '';
	let lastSyncedExtScopeKey = '';
	let extScopeGen = 0;

	$effect(() => {
		const extScopeKey = extParam ?? '';
		const extScopeChanged = extScopeKey !== lastExtScopeKey;
		if (extScopeChanged) {
			lastExtScopeKey = extScopeKey;
			extScopeGen++;
		}
		// Wait for a settled source list: a failed or in-flight load has no sources
		// to match, and syncing against it would mark this scope as already resumed.
		if (!auth.user || !sourcesLoaded || sourcesLoading || sourcesError) return;

		// Resume a running extension job when the extension scope is present.
		if (extParam) {
			if (extScopeKey !== lastSyncedExtScopeKey) {
				lastSyncedExtScopeKey = extScopeKey;
				void syncExtPanel(
					extParam,
					extMatches[0]?.name ?? extParam,
					extMatches.map((s) => String(s.id)),
					extScopeGen,
				);
			}
		} else {
			lastSyncedExtScopeKey = '';
		}

		const currentValidId = validSourceId;

		if (currentValidId !== lastTrackedSourceId) {
			lastTrackedSourceId = currentValidId;
			browseGen++;
			items = [];
			browsed = false;
			browseError = null;
			hasNext = false;
			picked = [];
			bulkResult = null;
		}

		if (!currentValidId) {
			browseGen++;
			items = [];
			browsed = false;
			browseError = null;
			browsing = false;
			hasNext = false;
			stopPolling();
			job = null;
			jobGen++;
			lastCheckedJobSource = '';
			return;
		}

		if (currentValidId !== lastCheckedJobSource) {
			lastCheckedJobSource = currentValidId;
			void checkActiveJob(currentValidId);
		}

		void runBrowse(
			currentValidId,
			currentBrowseType,
			currentPageNum,
			currentBrowseType === 'SEARCH' ? qParam : undefined,
		);
	});

	// Stop both pollers when the component unmounts, including a poll in flight.
	$effect(() => () => {
		destroyed = true;
		stopPolling();
		stopExtPolling();
	});
</script>

<div class="k-page">
	<PageHeader
		title="Browse & add"
		description="Look through a source and add its series to your catalogue."
	/>

	<LoadState
		loading={!sourcesLoaded || (sourcesLoading && sources.length === 0)}
		error={sourcesError}
		empty={sources.length === 0}
		loadingLabel="Loading sources…"
		skeletonRows={3}
		emptyTitle="No sources yet"
		emptyHint="Install an extension first. Every source it provides shows up here, NSFW ones included."
		emptyIcon="compass"
		onretry={() => refreshSources()}
	>
		{#snippet emptyActions()}
			<Button href="/sources/extensions" icon="package">Open Extensions</Button>
		{/snippet}

		{#if extScope}
			{#if scopeEmpty}
				<div class="k-notice" data-tone={extLookup === 'loading' ? undefined : 'warning'}>
					<Icon name={extLookup === 'loading' ? 'info' : 'alert'} />
					<div class="k-notice__body">
						{#if extLookup === 'loading'}
							Looking up <span class="k-mono">{extScope.pkgName}</span>…
						{:else if extKnown?.isInstalled}
							{extKnown.name} is installed, but it has not reported any sources yet. Try again shortly,
							or browse another source.
						{:else if extKnown}
							{extKnown.name} is not installed, so it has no sources to browse. Install it on the
							<a class="k-link" href="/sources/extensions">Extensions</a> page first.
						{:else}
							No installed source belongs to an extension called
							<span class="k-mono">{extScope.pkgName}</span>. Check the link you followed, or browse
							all sources.
						{/if}
					</div>
					<Button size="sm" href={clearScopeHref}>Show all sources</Button>
				</div>
			{:else}
				<div class="scope">
					<p class="scope-text">
						Showing sources from <strong>{extScope.name}</strong>
						<span class="k-muted k-num">
							({scopedSources.length} source{scopedSources.length === 1 ? '' : 's'})
						</span>
					</p>
					<div class="scope-actions">
						<Button
							icon="download"
							busy={extStarting}
							disabled={extAnyRunning}
							title="Start a background job for every source of this extension at once"
							onclick={addAllFromExtension}
						>
							{extStarting
								? 'Starting'
								: extAnyRunning
									? 'Already running'
									: `Add all from this extension${
											scopedSources.length > 1 ? ` (${scopedSources.length} sources)` : ''
										}`}
						</Button>
						<Button variant="ghost" href={clearScopeHref}>Show all sources</Button>
					</div>
				</div>
			{/if}
		{/if}

		{#if extError}
			<div class="k-notice" data-tone="danger" role="alert">
				<Icon name="alert" />
				<div class="k-notice__body">
					<p>{extError}</p>
					<p>
						{extPollStopped
							? 'The jobs may still be running. Live progress updates paused after repeated failures.'
							: 'Trying again shortly.'}
					</p>
				</div>
				{#if extPollStopped}
					<Button size="sm" onclick={retryExtPolling}>Retry</Button>
				{/if}
			</div>
		{/if}

		{#if extIngest}
			<IngestPanel
				heading="Add all from {extIngest.name}"
				detail="{extDoneCount} of {extJobs.length} source{extJobs.length === 1 ? '' : 's'} done"
				stateLabel={extAnyRunning ? 'Running' : 'Finished'}
				tone={extAnyRunning ? 'info' : 'success'}
				running={extAnyRunning}
				counts={jobCounts(extAgg)}
				note={extSkipped > 0
					? `${extSkipped} source${extSkipped === 1 ? ' was' : 's were'} skipped because ${
							extSkipped === 1 ? 'it is' : 'they are'
						} NSFW or unavailable.`
					: null}
				review={extAgg.queuedForReview > 0}
			>
				{#snippet actions()}
					{#if extAnyRunning}
						<Button
							size="sm"
							variant="danger"
							busy={extCancelling}
							onclick={cancelAllFromExtension}
						>
							{extCancelling ? 'Stopping' : 'Stop all'}
						</Button>
					{:else}
						<Button size="sm" onclick={dismissExtIngest}>Dismiss</Button>
					{/if}
				{/snippet}
				<ul class="per-source">
					{#each extJobs as ej (ej.id)}
						<li class="per-source-item">
							<span class="k-badge" data-tone={jobTone(ej.state)}>
								{JOB_STATE_LABEL[ej.state] ?? ej.state}
							</span>
							<span class="per-source-name">{sourceLabel(String(ej.sourceId))}</span>
							<span class="per-source-nums k-muted k-num">
								{ej.itemsSeen.toLocaleString()} found, {ej.newWorks.toLocaleString()} new,
								{ej.alreadyExisting.toLocaleString()} already in catalogue{ej.queuedForReview > 0
									? `, ${ej.queuedForReview.toLocaleString()} sent to Duplicates`
									: ''}{ej.failed > 0 ? `, ${ej.failed.toLocaleString()} failed` : ''}
							</span>
						</li>
					{/each}
				</ul>
			</IngestPanel>
		{/if}

		{#if !scopeEmpty}
			<div class="tools">
				<label class="k-field source-field">
					<span class="k-label">Source</span>
					<select
						class="k-select"
						value={validSourceId}
						onchange={onSourceChange}
						aria-label="Pick a source"
					>
						<option value="" disabled selected={!validSourceId}>
							{extScope && scopedSources.length !== 1 ? 'Pick a language…' : 'Pick a source…'}
						</option>
						{#each scopedSources as s (s.id)}
							<option value={String(s.id)} selected={String(s.id) === validSourceId}
								>{s.name} ({s.lang}{s.isNsfw ? ', NSFW' : ''})</option
							>
						{/each}
					</select>
				</label>
				<div class="k-field">
					<span class="k-label" aria-hidden="true">Listing</span>
					<Segmented
						value={currentBrowseType}
						options={LISTINGS}
						label="Listing"
						onchange={onPickListing}
					/>
				</div>
				{#if currentBrowseType === 'SEARCH'}
					<form class="search-form" onsubmit={onSearch}>
						<label class="k-field search-field">
							<span class="k-label">Search</span>
							<span class="k-search">
								<Icon name="search" />
								<input
									class="k-input"
									autocomplete="off"
									name="searchDraft"
									type="search"
									placeholder="Series title"
									bind:value={searchDraft}
									aria-label="Search the source"
								/>
							</span>
						</label>
						<Button type="submit" disabled={browsing || !validSourceId}>Search</Button>
					</form>
				{/if}
				{#if validSourceId}
					<Button
						class="add-all"
						icon="download"
						busy={jobStarting}
						disabled={job?.state === 'running'}
						title="Add every series of this source in the background, merging likely duplicates"
						onclick={addAllFromSource}
					>
						{jobStarting
							? 'Starting'
							: job?.state === 'running'
								? 'Already running'
								: 'Add all from this source'}
					</Button>
				{/if}
			</div>
		{/if}

		{#if jobError}
			<div class="k-notice" data-tone="danger" role="alert">
				<Icon name="alert" />
				<div class="k-notice__body">
					<p>{jobError}</p>
					<p>
						{pollStopped
							? 'The job may still be running. Live progress updates paused after repeated failures.'
							: 'Trying again shortly.'}
					</p>
				</div>
				{#if pollStopped}
					<Button size="sm" onclick={retryPolling}>Retry</Button>
				{/if}
			</div>
		{/if}

		{#if job}
			<IngestPanel
				heading="Add all from source {job.sourceId}"
				detail={sourceLabel(String(job.sourceId))}
				stateLabel={JOB_STATE_LABEL[job.state] ?? job.state}
				tone={jobTone(job.state)}
				running={job.state === 'running'}
				counts={jobCounts(job)}
				error={job.error}
				review={job.queuedForReview > 0}
			>
				{#snippet actions()}
					{#if job?.state === 'running'}
						<Button size="sm" variant="danger" busy={jobCancelling} onclick={cancelJob}>
							{jobCancelling ? 'Stopping' : 'Stop'}
						</Button>
					{:else}
						<Button size="sm" onclick={dismissJob}>Dismiss</Button>
					{/if}
				{/snippet}
			</IngestPanel>
		{/if}

		{#if bulkResult}
			<section class="result" bind:this={resultEl}>
				<div class="result-head">
					<div>
						<h2>Result of the last add</h2>
						<p class="k-hint k-num">
							{bulkResult.succeeded} of {bulkResult.total} series handled without an error.
						</p>
					</div>
					<Button size="sm" onclick={() => (bulkResult = null)}>Dismiss</Button>
				</div>
				<IngestCounts
					items={[
						{ label: 'Added as new', value: bulkResult.newWorks },
						{ label: 'Merged', value: bulkResult.autoMerged },
						{ label: 'Sent to Duplicates', value: bulkResult.queuedForReview },
						{ label: 'Already in catalogue', value: bulkResult.alreadyExisting },
						{
							label: 'Failed',
							value: bulkResult.failed,
							tone: bulkResult.failed > 0 ? 'danger' : undefined,
						},
					]}
				/>
				<div
					class="k-table"
					role="table"
					aria-label="Per-series ingest outcome"
					aria-busy={jobStarting}
				>
					<div class="k-tr k-tr--head" role="row">
						<span role="columnheader">Series</span>
						<span role="columnheader">Decision</span>
						<span role="columnheader">Work</span>
						<span role="columnheader">Matched by</span>
					</div>
					{#each bulkResult.entries as en (en.suwayomiMangaId)}
						<div class="k-tr" role="row">
							<span class="series-cell" role="cell">
								<span class="series-title">{entryTitle(String(en.suwayomiMangaId))}</span>
								<span class="k-mono k-muted">Source id {en.suwayomiMangaId}</span>
							</span>
							<span role="cell" data-label="Decision">
								{#if en.result}
									<span class="k-badge decision" data-tone={decisionTone(en.result.decision)}
										>{decisionLabel(en.result.decision)}</span
									>
								{:else}
									<span class="k-badge decision" data-tone="danger">Failed</span>
								{/if}
							</span>
							<span class="wrap" role="cell" data-label="Work">
								{#if en.result}
									<span class="k-mono">{en.result.workId}</span>
								{:else}
									<span class="fail-reason">{en.error ?? 'Unknown error'}</span>
								{/if}
							</span>
							<span class="k-muted" role="cell" data-label="Matched by">
								{#if en.result?.method}
									{methodLabel(en.result.method)}{en.result.score != null
										? `, ${Math.round(en.result.score * 100)}%`
										: ''}
								{:else}
									None
								{/if}
							</span>
						</div>
					{/each}
				</div>
				{#if bulkResult.queuedForReview > 0}
					<p class="k-hint">
						Some series look like works already in the catalogue. Decide on them on the
						<a class="k-link" href="/review">Duplicates</a> page.
					</p>
				{/if}
			</section>
		{/if}

		{#if !scopeEmpty}
			{#if !validSourceId}
				<div class="k-empty">
					<Icon name="compass" size={22} />
					{#if extScope && scopedSources.length > 1}
						<p class="k-empty__title">Pick a language</p>
						<p>{extScope.name} has one source per language. Choose one above to see its series.</p>
					{:else}
						<p class="k-empty__title">Pick a source</p>
						<p>Choose a source above to see its series.</p>
					{/if}
				</div>
			{:else}
				<LoadState
					error={browseError}
					empty={!browsing && browsed && items.length === 0}
					emptyIcon="search"
					emptyTitle={currentBrowseType === 'SEARCH' ? 'No results' : 'This listing is empty'}
					emptyHint={currentBrowseType === 'SEARCH'
						? 'Nothing in this source matches your search. Try another title.'
						: 'The source returned no series here. Try another listing, or check the source on the Health page.'}
					onretry={retryBrowse}
				>
					{#if browsing && items.length === 0}
						<div class="browse" role="status">
							<span class="k-sr-only">Loading series…</span>
							<div class="grid" aria-hidden="true">
								{#each { length: 12 } as _, i (i)}
									<div class="skel">
										<span class="k-skeleton skel-cover"></span>
										<span class="k-skeleton skel-line" style:width="{82 - (i % 3) * 16}%"></span>
									</div>
								{/each}
							</div>
						</div>
					{:else if !browsed && currentBrowseType === 'SEARCH'}
						<div class="k-empty">
							<Icon name="search" size={22} />
							<p class="k-empty__title">Search this source</p>
							<p>Type a series title above and press Search.</p>
						</div>
					{:else if items.length > 0}
						<div class="browse">
							<div class="grid" class:is-dim={browsing}>
								{#each items as it (it.suwayomiMangaId)}
									{@const id = String(it.suwayomiMangaId)}
									{@const isPicked = picked.includes(id)}
									<article class="card" class:is-picked={isPicked} title={it.title}>
										<button
											type="button"
											class="card-open"
											onclick={() => (reading = { id, title: it.title })}
										>
											<span class="cover">
												{#if it.thumbnailUrl && !failedThumbs[id]}
													<img
														src={it.thumbnailUrl}
														alt=""
														loading="lazy"
														decoding="async"
														referrerpolicy="no-referrer"
														onerror={() => (failedThumbs = { ...failedThumbs, [id]: true })}
													/>
												{/if}
												{#if it.inLibrary}
													<span class="in-catalogue">
														<span class="k-badge" data-tone="success">In catalogue</span>
													</span>
												{/if}
											</span>
											<span class="card-title">{it.title}</span>
										</button>
										<Button
											class="pick"
											size="sm"
											iconOnly
											icon={isPicked ? 'check' : 'plus'}
											aria-pressed={isPicked}
											aria-label="Select {it.title}"
											disabled={!isPicked && picked.length >= MAX_PICKED}
											onclick={() => togglePick(id)}
										/>
									</article>
								{/each}
							</div>
							<Pager page={currentPageNum} {hasNext} busy={browsing} onpage={goToPage} />
							{#if picked.length > 0}
								<!-- Last in the column and sticky to the viewport bottom: it follows the
								     scroll, then settles under the pager instead of covering it. -->
								<div class="bulk">
									<p class="bulk-count k-num">
										<strong>{picked.length}</strong> selected{picked.length >= MAX_PICKED
											? `. ${MAX_PICKED} is the most you can add at once.`
											: ''}
									</p>
									<div class="bulk-actions">
										<Button variant="ghost" disabled={adding} onclick={() => (picked = [])}>
											Clear
										</Button>
										<Button variant="primary" icon="plus" busy={adding} onclick={addPicked}>
											{adding ? 'Adding' : 'Add selected to catalogue'}
										</Button>
									</div>
								</div>
							{/if}
						</div>
					{/if}
				</LoadState>
			{/if}
		{/if}
	</LoadState>
</div>

{#if reading}
	<SourceReader seriesId={reading.id} title={reading.title} onclose={() => (reading = null)} />
{/if}

<style>
	/* ---- extension scope ---- */
	.scope {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3);
		padding-bottom: var(--k-space-4);
		border-bottom: 1px solid var(--k-border);
		min-width: 0;
	}
	.scope-text {
		min-width: 0;
		color: var(--k-text-2);
		font-size: var(--k-text-md);
		overflow-wrap: anywhere;
	}
	.scope-text strong {
		color: var(--k-text-bright);
	}
	.scope-actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}

	/* ---- per-source rows inside the extension job ---- */
	.per-source {
		display: grid;
		margin: 0;
		padding: 0;
		list-style: none;
		border-top: 1px solid var(--k-border);
	}
	.per-source-item {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--k-space-2) var(--k-space-3);
		padding: 10px 0;
		border-bottom: 1px solid var(--k-border);
		font-size: var(--k-text-sm);
		min-width: 0;
	}
	.per-source-item:last-child {
		padding-bottom: 0;
		border-bottom: 0;
	}
	.per-source-name {
		flex: 1 1 160px;
		min-width: 0;
		color: var(--k-text-1);
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.per-source-nums {
		min-width: 0;
		overflow-wrap: anywhere;
	}

	/* ---- toolbar ---- */
	.tools {
		display: flex;
		align-items: flex-end;
		flex-wrap: wrap;
		gap: var(--k-space-4);
		min-width: 0;
	}
	.source-field {
		flex: 1 1 240px;
		max-width: 380px;
		/* A select is as wide as its longest option unless its track may shrink. */
		grid-template-columns: minmax(0, 1fr);
	}
	.search-form {
		display: flex;
		align-items: flex-end;
		flex: 1 1 260px;
		gap: var(--k-space-2);
		min-width: 0;
		max-width: 420px;
	}
	.search-field {
		flex: 1 1 auto;
	}
	.tools :global(.add-all) {
		margin-left: auto;
	}

	/* ---- result of "add selected" ---- */
	.result {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--k-space-4);
		min-width: 0;
		/* Clears the sticky top bar on narrow screens when scrolled into view. */
		scroll-margin-top: 72px;
	}
	.result-head {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3);
	}
	.result-head h2 {
		font-size: var(--k-text-lg);
		font-weight: 700;
	}
	.k-table {
		--cols: minmax(0, 2fr) minmax(0, 1.5fr) minmax(0, 1.4fr) minmax(0, 1.1fr);
	}
	.series-cell {
		display: grid;
		gap: 2px;
	}
	.series-title {
		color: var(--k-text-bright);
		font-weight: 600;
		overflow-wrap: anywhere;
	}
	.wrap {
		overflow-wrap: anywhere;
	}
	/* Decision labels are sentences: let the badge wrap instead of spilling out of its cell. */
	.decision {
		height: auto;
		min-height: 22px;
		padding-block: 4px;
		line-height: 1.3;
		white-space: normal;
		text-align: left;
	}
	.fail-reason {
		color: var(--k-danger);
	}

	/* An extension name in the prompt can be one long word. */
	.k-empty p {
		max-width: 100%;
		overflow-wrap: anywhere;
	}

	/* ---- browse grid ---- */
	.browse {
		display: flex;
		flex-direction: column;
		gap: var(--k-space-6);
		min-width: 0;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(148px, 1fr));
		gap: var(--k-space-5) var(--k-space-4);
		min-width: 0;
		transition: opacity var(--k-dur) var(--k-ease);
	}
	.grid.is-dim {
		opacity: 0.55;
	}
	.card {
		position: relative;
		min-width: 0;
	}
	.card-open {
		display: grid;
		gap: var(--k-space-2);
		width: 100%;
		padding: 0;
		border: 0;
		border-radius: var(--k-radius);
		background: none;
		color: inherit;
		text-align: left;
		cursor: pointer;
	}
	.cover {
		position: relative;
		display: block;
		aspect-ratio: 2 / 3;
		border: 1px solid var(--k-border-1);
		border-radius: var(--k-radius);
		background: var(--k-hatch), var(--k-cover);
		overflow: hidden;
		transition: border-color var(--k-dur) var(--k-ease);
	}
	.cover img {
		display: block;
		width: 100%;
		height: 100%;
		object-fit: cover;
	}
	.card-open:hover .cover {
		border-color: var(--k-border-strong);
	}
	.card.is-picked .cover {
		border-color: var(--k-accent);
		outline: 2px solid var(--k-accent);
		outline-offset: -1px;
	}
	/* The badge tone is translucent: give it a solid backing so it reads over any cover. */
	.in-catalogue {
		position: absolute;
		left: 6px;
		bottom: 6px;
		display: flex;
		border-radius: 6px;
		background: var(--k-surface);
	}
	.card-title {
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		overflow: hidden;
		color: var(--k-text-1);
		font-size: var(--k-text-sm);
		font-weight: 600;
		line-height: 1.4;
		overflow-wrap: anywhere;
	}
	.card-open:hover .card-title {
		color: var(--k-text-bright);
	}
	/* The selection toggle sits on the cover, so it needs opaque fills of its own. */
	.card :global(.pick) {
		position: absolute;
		top: 6px;
		right: 6px;
		background: var(--k-surface);
		border-color: var(--k-border-strong);
		color: var(--k-text-bright);
	}
	.card :global(.pick:hover:not(:disabled)) {
		background: var(--k-surface-5);
	}
	.card :global(.pick[aria-pressed='true']),
	.card :global(.pick[aria-pressed='true']:hover:not(:disabled)) {
		background: var(--k-accent);
		border-color: transparent;
		color: var(--k-on-accent);
	}
	.skel {
		display: grid;
		gap: var(--k-space-2);
	}
	.skel-cover {
		aspect-ratio: 2 / 3;
	}
	.skel-line {
		height: 12px;
	}

	/* ---- bulk bar ---- */
	.bulk {
		position: sticky;
		bottom: var(--k-space-4);
		z-index: 20;
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--k-space-3);
		padding: var(--k-space-3) var(--k-space-3) var(--k-space-3) var(--k-space-4);
		border: 1px solid var(--k-border-3);
		border-radius: var(--k-radius-lg);
		background: var(--k-surface-5);
		box-shadow: var(--k-shadow-2);
	}
	.bulk-count {
		min-width: 0;
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
	}
	.bulk-count strong {
		color: var(--k-text-bright);
	}
	.bulk-actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--k-space-2);
	}

	@media (max-width: 720px) {
		.source-field,
		.search-form {
			flex-basis: 100%;
			max-width: none;
		}
		.tools :global(.add-all) {
			margin-left: 0;
		}
		.grid {
			grid-template-columns: repeat(auto-fill, minmax(132px, 1fr));
		}
	}
</style>
