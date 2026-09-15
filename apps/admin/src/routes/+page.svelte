<script lang="ts">
	import { href } from '$lib/paths';
	import type {
		MatchResult,
		Series,
		SeriesSourceGroup,
		SeriesStatus,
		WorkSource,
	} from '@komika/types';
	import { untrack } from 'svelte';
	import { goto, beforeNavigate } from '$app/navigation';
	import { page as pageState } from '$app/state';
	import { auth } from '$lib/auth.svelte';
	import { assetSrc } from '$lib/config';
	import { confirm } from '$lib/confirm.svelte';
	import { toast } from '$lib/toast.svelte';
	import Button from '$lib/components/Button.svelte';
	import Cover from '$lib/components/Cover.svelte';
	import Dialog from '$lib/components/Dialog.svelte';
	import GettingStarted from '$lib/components/GettingStarted.svelte';
	import Icon from '$lib/components/Icon.svelte';
	import LoadState from '$lib/components/LoadState.svelte';
	import PageHeader from '$lib/components/PageHeader.svelte';
	import Pager from '$lib/components/Pager.svelte';
	import Segmented from '$lib/components/Segmented.svelte';
	import {
		loadCatalog,
		loadSeriesSources,
		loadWorkSources,
		mergeWorks,
		saveSeriesAdmin,
		setSeriesPaused,
		triggerScan,
		addSourceSeries,
		STATUS_OPTIONS,
	} from '$lib/data';

	/**
	 * The two id spaces this table can hold. An EMPTY query lists Suwayomi mangas
	 * (numeric ids); a TEXT query hits the canonical FTS index and returns canonical
	 * works (`w_`-prefixed). They take different actions — provenance is keyed by
	 * Suwayomi id, `Add` only accepts a Suwayomi id — so every row action branches on
	 * this, the same way `loadSeriesDetail()` routes in lib/data.ts.
	 */
	function isCanonical(s: Series): boolean {
		return s.id.startsWith('w_');
	}

	const MAX_CATALOG_PAGE = 100_000;

	/** A safe 1-based page from the URL; malformed or absurd offsets fall back to page 1. */
	function pageFromUrl(params: URLSearchParams): number {
		const raw = params.get('page');
		if (raw == null || raw.trim() === '') return 1;
		const parsed = Number(raw);
		return Number.isInteger(parsed) && parsed >= 1 && parsed <= MAX_CATALOG_PAGE ? parsed : 1;
	}

	/** Build the canonical catalogue URL while preserving params owned by other features. */
	function catalogueHref(q: string, toPage: number): string {
		const params = new URLSearchParams(pageState.url.searchParams);
		const committed = q.trim();
		if (committed) params.set('q', committed);
		else params.delete('q');
		if (toPage > 1) params.set('page', String(toPage));
		else params.delete('page');
		const search = params.toString();
		return search ? `${pageState.url.pathname}?${search}` : pageState.url.pathname;
	}

	const appliedQuery = $derived(pageState.url.searchParams.get('q')?.trim() ?? '');
	const page = $derived(pageFromUrl(pageState.url.searchParams));
	// The text box is a draft. Results, labels and pagination continue to use the
	// committed URL query until the form is submitted.
	let searchDraft = $state('');
	$effect(() => {
		searchDraft = appliedQuery;
	});
	// `series` holds ONLY the current SERVER page (≤ the server's page size, itself far
	// under the 200-id provenance cap) — never the whole catalogue. loadCatalog() pulls
	// exactly one page from backend.search(query, page); the pager below requests the
	// next/previous server page. So the console's memory footprint is one page, not the
	// full library.
	let series = $state<Series[]>([]);
	let loading = $state(false);
	let loadError = $state<string | null>(null);

	// Server-driven pagination. `page` is the SERVER page number; `total`/`hasNextPage`
	// come straight from the search envelope. An empty-query (catalogue) search reports
	// a real `total`; a live text search reports `total: null`, so we fall back to
	// driving the Next button off `hasNextPage` alone.
	let total = $state<number | null>(null);
	let hasNextPage = $state(false);
	// The server's page size, learned from a full page (when more pages follow, the page
	// is full), so the range/​page-count math never hardcodes the server's constant.
	let pageSize = $state(0);
	const totalPages = $derived(
		total != null && pageSize > 0 ? Math.max(1, Math.ceil(total / pageSize)) : null,
	);
	const rangeStart = $derived(series.length === 0 ? 0 : (page - 1) * pageSize + 1);
	const rangeEnd = $derived(series.length === 0 ? 0 : (page - 1) * pageSize + series.length);
	const pagerDetail = $derived(
		totalPages != null
			? `of ${totalPages}, showing ${rangeStart} to ${rangeEnd}${total != null ? ` of ${total}` : ''}`
			: `showing ${rangeStart} to ${rangeEnd}`,
	);
	// The count shown next to the search. A text search reports no total, so say
	// "at least" while more pages follow rather than pretend the page is everything.
	const countNumber = $derived(total ?? series.length);
	const countText = $derived(
		total != null
			? String(total)
			: hasNextPage
				? `At least ${series.length}`
				: String(series.length),
	);
	/** A page is on screen (not the first load, not a failed one), so a count means something. */
	const listed = $derived(!loadError && !(loading && series.length === 0));
	/** A catalogue request has answered at least once (before that, `series` is just empty). */
	let answered = $state(false);
	/** The first page of the whole catalogue is on screen: what the first-run card reads. */
	const unfiltered = $derived(answered && listed && !loading && !appliedQuery && page === 1);

	// ---- source/extension provenance (seriesSourcesBatch) ----
	let provenance = $state<Record<string, SeriesSourceGroup>>({});
	let provenanceError = $state<string | null>(null);
	/** A provenance fetch is in flight: rows without an answer yet say "Checking". */
	let provenancePending = $state(false);

	/** Monotonic token: a slow provenance fetch must not overwrite a newer page's. */
	let provSeq = 0;

	/**
	 * Fetch provenance for just the current server page (its size ≤ the 200-id cap).
	 * Only Suwayomi-id rows are sent: `seriesSourcesBatch` looks up `source_type =
	 * 'suwayomi'` mappings, so a `w_` work id would match nothing and come back as a
	 * null workId — which the table used to render as "not catalogued".
	 *
	 * The map is REPLACED in one assignment once the response lands (never cleared
	 * up-front), so a single-row mutation that re-runs this can't flicker every row's
	 * Source column to "—" and yank the Merge button mid-selection.
	 */
	async function loadProvenance(list: Series[]): Promise<void> {
		const seq = ++provSeq;
		provenanceError = null;
		const ids = list.filter((s) => !isCanonical(s)).map((s) => s.id);
		if (ids.length === 0) {
			provenance = {};
			provenancePending = false;
			return;
		}
		provenancePending = true;
		try {
			const groups = await loadSeriesSources(ids);
			if (seq !== provSeq) return; // a newer page superseded this fetch
			// Carry over what we already knew for the rows still on screen, then
			// overlay the fresh groups — bounded to the current page either way.
			const map: Record<string, SeriesSourceGroup> = {};
			for (const id of ids) {
				const prev = provenance[id];
				if (prev) map[id] = prev;
			}
			for (const g of groups) map[String(g.seriesId)] = g;
			provenance = map;
		} catch (err) {
			if (seq !== provSeq) return;
			provenanceError = err instanceof Error ? err.message : 'The request failed.';
		} finally {
			if (seq === provSeq) provenancePending = false;
		}
	}

	/** Compact label for one source mapping: extension pkg tail + lang, or the source type. */
	function extLabel(src: WorkSource): string {
		const pkg = src.extension?.pkgName;
		const name = pkg ? (pkg.split('.').pop() ?? pkg) : src.sourceType;
		return src.lang ? `${name} (${src.lang})` : name;
	}

	// ---- targeted pause toggle ----
	let pausing = $state<string | null>(null);

	/**
	 * Pause or unpause one row. `setSeriesPaused` writes the same forced override the
	 * drawer's scanner mode does (true is "Paused", false is "Keep scanning") and
	 * leaves the other overrides alone, so the two controls cannot disagree: the
	 * returned series replaces the row, and the drawer decodes its mode from that.
	 * Unpausing also re-scans on the server before it answers.
	 */
	async function setPaused(s: Series, paused: boolean): Promise<void> {
		// Like the row editor: the server keys pause overrides by numeric source series
		// id and rejects a `w_` one, so the buttons are not rendered for catalogue works.
		if (pausing || isCanonical(s)) return;
		pausing = s.id;
		try {
			const updated = await setSeriesPaused(s.id, paused);
			series = series.map((x) => (x.id === updated.id ? updated : x));
			toast.success(
				paused
					? `Scanning is paused for ${s.title}.`
					: `Scanning resumed for ${s.title}. It was scanned just now.`,
			);
		} catch (err) {
			toast.fromError(err, paused ? 'Could not pause scanning.' : 'Could not resume scanning.');
		} finally {
			pausing = null;
		}
	}

	// ---- merge duplicate works (admin D1, destructive + irreversible) ----
	type MergeSide = { seriesId: string; workId: string; title: string; group: SeriesSourceGroup };
	/** The work chosen to be DELETED (its mappings/data fold into the target). */
	let mergeSource = $state<MergeSide | null>(null);
	/** The surviving target; when set, the confirmation dialog is shown. */
	let mergeTarget = $state<MergeSide | null>(null);
	let merging = $state(false);
	// A failed merge stays inline in the confirm dialog: the dialog is still open and
	// makes everything behind it (the toaster included) inert.
	let mergeError = $state<string | null>(null);

	/** The canonical work id for a row, or null when not catalogued. A text-search row
	 * IS a canonical work, so its own id is the work id — no provenance lookup needed. */
	function workIdOf(s: Series): string | null {
		if (isCanonical(s)) return s.id;
		const g = provenance[s.id];
		return g?.workId ?? null;
	}

	/** Can this row be a target for the in-progress merge? (catalogued + a different work) */
	function canBeTarget(s: Series): boolean {
		if (!mergeSource) return false;
		const wid = workIdOf(s);
		return !!wid && wid !== mergeSource.workId;
	}

	/**
	 * Build a merge side for a row. Suwayomi rows reuse the batch provenance already on
	 * screen; canonical (`w_`) rows aren't in that batch at all, so their source
	 * mappings are pulled from the work itself — the confirm dialog must show real
	 * provenance for a destructive, irreversible merge, not an empty list.
	 */
	async function sideFor(s: Series): Promise<MergeSide | null> {
		const workId = workIdOf(s);
		if (!workId) return null;
		const cached = provenance[s.id];
		if (cached?.workId === workId) return { seriesId: s.id, workId, title: s.title, group: cached };
		let sources: WorkSource[] = [];
		try {
			sources = await loadWorkSources(workId);
		} catch {
			// Provenance chips are informational; the merge itself doesn't need them.
		}
		return { seriesId: s.id, workId, title: s.title, group: { seriesId: s.id, workId, sources } };
	}

	async function startMerge(s: Series): Promise<void> {
		const side = await sideFor(s);
		if (!side) return;
		mergeSource = side;
		mergeTarget = null;
		mergeError = null;
	}

	async function pickTarget(s: Series): Promise<void> {
		if (!mergeSource || !canBeTarget(s)) return;
		const side = await sideFor(s);
		if (!mergeSource) return; // the merge was cancelled while we fetched
		mergeTarget = side;
		mergeError = null;
	}

	function cancelMerge(): void {
		mergeSource = null;
		mergeTarget = null;
		mergeError = null;
	}

	/** Step back from the confirm dialog to target picking (not while a merge runs). */
	function backToTargets(): void {
		if (!merging) mergeTarget = null;
	}

	async function confirmMerge(): Promise<void> {
		if (!mergeSource || !mergeTarget || merging) return;
		merging = true;
		mergeError = null;
		try {
			const res = await mergeWorks(mergeSource.workId, mergeTarget.workId);
			const moved = res.movedSourceSeries;
			toast.success(
				`Merged into ${mergeTarget.title}. ${moved} source ${moved === 1 ? 'link' : 'links'} moved and the duplicate work was deleted.`,
			);
			mergeSource = null;
			mergeTarget = null;
			// Re-fetch the current server page so the deleted work's phantom row is gone.
			await refresh(appliedQuery, page);
			// Provenance too, explicitly: the page's id list usually survives a merge
			// unchanged (source-series rows don't disappear when their work is folded),
			// and the provenance effect is keyed on that id list — so it wouldn't re-fire
			// and the Source column would keep showing the deleted work's mappings.
			await loadProvenance(series);
		} catch (err) {
			mergeError = err instanceof Error ? err.message : 'Merge failed.';
		} finally {
			merging = false;
		}
	}

	// Auth redirect is centralized in +layout.svelte.

	// Canonicalise invalid/default URL values without adding a history entry. Generated
	// URLs likewise omit an empty query and page 1.
	$effect(() => {
		const href = catalogueHref(appliedQuery, page);
		if (href !== `${pageState.url.pathname}${pageState.url.search}`) {
			void goto(href, { replaceState: true, noScroll: true, keepFocus: true });
		}
	});

	// One fetch driver for initial load, reload, Back/Forward and our own navigations.
	// Event handlers only update the URL, so a transition cannot issue a duplicate fetch.
	$effect(() => {
		if (!auth.user) return;
		void refresh(appliedQuery, page);
	});

	// Fetch provenance for only the rows on the current server page (≤ the 200-id batch
	// cap), so no rendered row ever silently shows "—" from a batch-size truncation.
	// Keyed on the ID LIST, not the array reference: unpause/save/scan replace `series`
	// with a new array holding the same ids, and re-running the whole page's provenance
	// query for a one-row edit blanked every Source cell (and every Merge button) while
	// it was in flight.
	const seriesIdKey = $derived(series.map((s) => s.id).join(','));
	$effect(() => {
		void seriesIdKey; // the effect's only dependency
		void loadProvenance(untrack(() => series));
	});

	/** Monotonic token so a slow page load can't land after a newer one (rapid Next /
	 * search-then-page would otherwise leave the table on a stale page). */
	let loadSeq = 0;

	/** Fetch ONE server page of the catalog (default page 1 on a fresh query). */
	async function refresh(q: string, toPage = 1): Promise<void> {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			const res = await loadCatalog(q, toPage);
			if (seq !== loadSeq) return; // a newer request superseded this one
			series = res.items;
			total = res.total;
			hasNextPage = res.hasNextPage;
			answered = true;
			// Learn the server's page size from any full page (a page with more to
			// follow is full); keep the last known size otherwise.
			if (res.hasNextPage && res.items.length > 0) pageSize = res.items.length;
			else if (pageSize === 0) {
				// Opened directly on the last page. Its own length is not the page size
				// (35 results, page 2 holds 5), but every page before it was full.
				const before = (res.total ?? res.items.length) - res.items.length;
				pageSize = toPage > 1 && before > 0 ? Math.ceil(before / (toPage - 1)) : res.items.length;
			}
			// Provenance for the new page is fetched by the series effect.
		} catch (err) {
			if (seq !== loadSeq) return;
			loadError = err instanceof Error ? err.message : 'Failed to load catalog.';
			series = [];
			provenance = {};
			total = null;
			hasNextPage = false;
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	function onSearch(e: SubmitEvent): void {
		e.preventDefault();
		const href = catalogueHref(searchDraft, 1);
		if (href !== `${pageState.url.pathname}${pageState.url.search}`) {
			void goto(href, { noScroll: true, keepFocus: true });
		} else {
			// Re-submitting the active search is also the manual retry affordance.
			void refresh(appliedQuery, page);
		}
	}

	function clearSearch(): void {
		searchDraft = '';
		const href = catalogueHref('', 1);
		if (href !== `${pageState.url.pathname}${pageState.url.search}`) {
			void goto(href, { noScroll: true, keepFocus: true });
		}
	}

	/** Move by ±1 SERVER page, refetching that page from the server. */
	function goPage(delta: number): void {
		if (loading) return;
		const next = page + delta;
		if (next < 1) return;
		if (delta > 0 && !hasNextPage) return;
		if (totalPages != null && next > totalPages) return;
		const href = catalogueHref(appliedQuery, next);
		if (href !== `${pageState.url.pathname}${pageState.url.search}`) {
			void goto(href, { noScroll: true, keepFocus: true });
		}
	}

	// ---- add to canonical catalogue (Tier-2 dedup add flow) ----
	/** Per-row result of a successful Add (the marker in the Source column); stays
	 * until the page changes. */
	let addMsg = $state<Record<string, MatchResult>>({});
	let addingId = $state<string | null>(null);

	function decisionLabel(r: MatchResult): string {
		switch (r.decision) {
			case 'auto_merge':
				return 'Merged';
			case 'review':
				return 'In review';
			case 'new':
				return 'Added';
			case 'existing':
				return 'Already added';
			default:
				return r.decision;
		}
	}

	function decisionToast(title: string, r: MatchResult): string {
		switch (r.decision) {
			case 'auto_merge':
				return `${title} matched a work already in the catalogue and was merged into it (${r.workId}).`;
			case 'review':
				return `${title} looks like a work already in the catalogue. It is queued for review.`;
			case 'new':
				return `${title} was added to the catalogue as a new work.`;
			case 'existing':
				return `${title} is already in the catalogue.`;
			default:
				return `${title}: ${r.decision}.`;
		}
	}

	/**
	 * Hide Add once the row is known to be catalogued: its sources resolve to a work,
	 * or an Add on this page just linked it. While that is still unknown (sources not
	 * loaded, or the lookup failed) the button stays, as before.
	 */
	function canAdd(s: Series): boolean {
		return !isCanonical(s) && !workIdOf(s) && !addMsg[s.id];
	}

	async function onAdd(s: Series): Promise<void> {
		if (addingId) return;
		// `addSourceSeries` takes a Suwayomi manga id; a canonical work is already in the
		// catalogue, so the button isn't rendered for one. Guarded here too.
		if (isCanonical(s)) return;
		addingId = s.id;
		try {
			const r = await addSourceSeries(s.id);
			addMsg = { ...addMsg, [s.id]: r };
			if (r.decision === 'existing') toast.info(decisionToast(s.title, r));
			else toast.success(decisionToast(s.title, r));
		} catch (err) {
			toast.fromError(err, 'Could not add this series to the catalogue.');
		} finally {
			addingId = null;
		}
	}

	// ---- editor ----
	type Scanner = 'auto' | 'paused' | 'running';
	let editing = $state<Series | null>(null);
	let fStatus = $state<'source' | SeriesStatus>('source');
	let fInterval = $state('');
	let fPoll = $state('');
	let fScanner = $state<Scanner>('auto');
	let saving = $state(false);
	// Save and scan failures stay inline in the drawer: it is still open and makes
	// everything behind it (the toaster included) inert.
	let saveError = $state<string | null>(null);
	let intervalError = $state<string | null>(null);
	let pollError = $state<string | null>(null);
	let scanning = $state(false);
	let scanError = $state<string | null>(null);

	/**
	 * The row editor writes SCAN/STATUS overrides, which are keyed by Suwayomi series
	 * id: `updateSeriesAdmin` and `triggerScan` both `parse::<i64>()` the id and reject
	 * a `w_` one ("seriesId must be a numeric Suwayomi series id"). A text query now
	 * returns canonical works, so opening this on one would guarantee an error on Save
	 * and on Scan now — those rows link to /series/{id} (the metadata editor, which
	 * does operate on works) instead.
	 */
	function openEditor(s: Series): void {
		if (isCanonical(s)) return;
		editing = s;
		saveError = null;
		intervalError = null;
		pollError = null;
		scanError = null;
		// Decode straight from the raw admin overrides so an explicit choice is
		// never mistaken for the status default (even when they coincide).
		fStatus = s.scan.statusOverride ?? 'source';
		fInterval = s.scan.overrideIntervalHours != null ? String(s.scan.overrideIntervalHours) : '';
		fPoll = s.scan.pollEveryMinutesOverride == null ? '' : String(s.scan.pollEveryMinutesOverride);
		fScanner =
			s.scan.pausedOverride == null ? 'auto' : s.scan.pausedOverride ? 'paused' : 'running';
	}

	const editorDirty = $derived(
		!!editing &&
			(fStatus !== (editing.scan.statusOverride ?? 'source') ||
				fInterval !==
					(editing.scan.overrideIntervalHours != null
						? String(editing.scan.overrideIntervalHours)
						: '') ||
				fPoll !==
					(editing.scan.pollEveryMinutesOverride != null
						? String(editing.scan.pollEveryMinutesOverride)
						: '') ||
				fScanner !==
					(editing.scan.pausedOverride == null
						? 'auto'
						: editing.scan.pausedOverride
							? 'paused'
							: 'running')),
	);
	beforeNavigate(({ cancel }) => {
		// Navigation guards must answer synchronously, so this one stays native.
		if (
			(editorDirty || saving) &&
			!window.confirm(
				'Leave this page? Unsaved changes will be lost. A save already in progress may still complete.',
			)
		)
			cancel();
	});
	async function closeEditor(): Promise<void> {
		if (saving) return;
		if (
			editorDirty &&
			!(await confirm({
				title: 'Discard your changes?',
				message: 'The scan settings you changed for this series have not been saved.',
				confirmLabel: 'Discard changes',
				cancelLabel: 'Keep editing',
				danger: true,
			}))
		)
			return;
		editing = null;
	}

	async function save(): Promise<void> {
		if (!editing || saving) return;
		saveError = null;
		const interval = fInterval.trim() === '' ? null : Number(fInterval);
		const poll = fPoll.trim() === '' ? null : Number(fPoll);
		intervalError =
			interval != null && (!Number.isFinite(interval) || interval <= 0)
				? 'Enter a number of hours greater than zero, or leave this blank.'
				: null;
		pollError =
			poll != null && (!Number.isInteger(poll) || poll <= 0)
				? 'Enter a whole number of minutes greater than zero, or leave this blank.'
				: null;
		if (intervalError || pollError) return;
		saving = true;
		try {
			const updated = await saveSeriesAdmin({
				seriesId: editing.id,
				overrideIntervalHours: interval,
				pollEveryMinutes: poll,
				paused: fScanner === 'auto' ? null : fScanner === 'paused',
				status: fStatus === 'source' ? null : fStatus,
			});
			series = series.map((s) => (s.id === updated.id ? updated : s));
			toast.success(`Scan settings saved for ${updated.title}.`);
			editing = null;
		} catch (err) {
			saveError =
				err instanceof Error ? err.message : 'The settings could not be saved. Try again.';
		} finally {
			saving = false;
		}
	}

	async function scanNow(): Promise<void> {
		if (!editing || scanning) return;
		scanning = true;
		scanError = null;
		try {
			const updated = await triggerScan(editing.id);
			series = series.map((s) => (s.id === updated.id ? updated : s));
			// Only if the drawer still shows that series. Closed and reopened on another
			// one, it would otherwise switch target under the form being edited.
			if (editing?.id === updated.id) editing = updated;
			const at = updated.scan.lastScannedAt;
			toast.success(
				at
					? `${updated.title} was scanned at ${new Date(at).toLocaleTimeString()}.`
					: `${updated.title} was scanned.`,
			);
		} catch (err) {
			scanError = err instanceof Error ? err.message : 'The scan failed. Try again.';
		} finally {
			scanning = false;
		}
	}

	function cadence(s: Series): string {
		if (s.scan.overrideIntervalHours != null)
			return `Every ${s.scan.overrideIntervalHours}h (set here)`;
		if (s.scan.avgIntervalHours > 0) return `Every ${s.scan.avgIntervalHours}h (average)`;
		return 'Automatic timing';
	}

	/** Badge tone per status; UNKNOWN keeps the neutral badge. */
	const STATUS_TONE: Record<SeriesStatus, string | undefined> = {
		ONGOING: 'success',
		COMPLETED: 'info',
		HIATUS: 'warning',
		CANCELLED: 'danger',
		UNKNOWN: undefined,
	};

	/** "ONGOING" reads as "Ongoing": enum words from the API, shown as words. */
	function sentence(word: string): string {
		return word.charAt(0).toUpperCase() + word.slice(1).toLowerCase().replaceAll('_', ' ');
	}

	const SCANNER_OPTIONS: { value: Scanner; label: string }[] = [
		{ value: 'auto', label: 'Auto' },
		{ value: 'running', label: 'Keep scanning' },
		{ value: 'paused', label: 'Paused' },
	];
</script>

{#snippet mergeSide(side: MergeSide, kept: boolean)}
	<div class="side">
		<span class="k-badge" data-tone={kept ? 'success' : 'danger'}>
			{kept ? 'Will be kept' : 'Will be deleted'}
		</span>
		<p class="side-title">{side.title}</p>
		<p class="k-mono k-muted side-id">{side.workId}</p>
		<div class="chips">
			{#if side.group.sources.length > 0}
				{#each side.group.sources as src (`${src.sourceType}:${src.sourceId}:${src.sourceKey}`)}
					<span class="k-badge" title={src.extension?.pkgName ?? src.sourceType}
						>{extLabel(src)}</span
					>
				{/each}
			{:else}
				<span class="detail">No sources linked</span>
			{/if}
		</div>
	</div>
{/snippet}

<div class="k-page">
	<PageHeader
		title="Catalog"
		description="Find a series, set how often it is scanned, add it to the catalogue, or merge duplicates."
	/>

	<GettingStarted
		empty={unfiltered && series.length === 0}
		hasSeries={unfiltered ? series.length > 0 : null}
	/>

	<div class="find">
		<form class="k-toolbar" onsubmit={onSearch}>
			<div class="k-search k-grow">
				<Icon name="search" />
				<input
					class="k-input"
					type="search"
					name="q"
					aria-label="Search the catalog"
					autocomplete="off"
					placeholder="Search by title"
					bind:value={searchDraft}
				/>
			</div>
			<Button type="submit" variant="primary">Search</Button>
			{#if searchDraft.trim() || appliedQuery}
				<Button variant="ghost" onclick={clearSearch}>Clear</Button>
			{/if}
		</form>
		<!-- The console reads the catalogue NSFW-inclusive regardless of the header's NSFW
		     preference; say so, so the console/reader difference isn't a mystery. -->
		<p class="k-muted mode">
			{#if appliedQuery}
				{#if listed}
					<span class="k-num">{countText}</span>
					catalogue {countNumber === 1 ? 'work matches' : 'works match'} “{appliedQuery}”.
				{:else}
					Searching catalogue works for “{appliedQuery}”.
				{/if}
				Clear the search to list series as the source engine has them.
			{:else}
				{#if listed}
					<span class="k-num">{countText}</span> series, listed as the source engine has them.
				{:else}
					Series are listed as the source engine has them.
				{/if}
				Search by title to list catalogue works instead.
			{/if}
			Series flagged NSFW always appear here, whatever your NSFW setting.
		</p>
	</div>

	{#if provenanceError}
		<div class="k-notice" data-tone="warning" role="alert">
			<Icon name="alert" />
			<div class="k-notice__body">
				Could not check which catalogue work each series belongs to, so the Source column and the
				Merge action are incomplete. {provenanceError}
			</div>
			<Button size="sm" onclick={() => loadProvenance(series)}>Retry</Button>
		</div>
	{/if}

	{#if mergeSource}
		<div class="k-notice" data-tone="info">
			<Icon name="merge" />
			<div class="k-notice__body">
				Merging <strong>{mergeSource.title}</strong>
				<span class="k-mono">({mergeSource.workId})</span>. Choose the row to keep. The work you are
				merging will be deleted, and its sources and reading data move to the one you keep.
			</div>
			<Button size="sm" onclick={cancelMerge}>Cancel merge</Button>
		</div>
	{/if}

	<LoadState
		loading={loading && series.length === 0}
		error={loadError}
		empty={series.length === 0}
		skeletonRows={8}
		emptyIcon="book"
		emptyTitle={page > 1
			? 'Nothing on this page'
			: appliedQuery
				? 'No catalogue works match this search'
				: 'No series yet'}
		emptyHint={page > 1
			? 'Go back to the previous page.'
			: appliedQuery
				? 'Check the spelling or try a shorter title. Clear the search to list series from the source engine.'
				: 'Series show up here once the source engine has them. Add one to a library in the reader, then come back.'}
		onretry={() => refresh(appliedQuery, page)}
	>
		<div
			class="k-table"
			class:is-dim={loading}
			role="table"
			aria-label="Catalog"
			aria-busy={loading}
		>
			<div class="k-tr k-tr--head" role="row">
				<span role="columnheader">Series</span>
				<span role="columnheader">Type</span>
				<span role="columnheader">Status</span>
				<span role="columnheader">Chapters</span>
				<span role="columnheader">Source</span>
				<span role="columnheader">Scan</span>
				<span role="columnheader" class="k-td--end">Actions</span>
			</div>
			{#each series as s (s.id)}
				<div class="k-tr" role="row">
					<div role="cell" class="series">
						<Cover src={s.coverUrl ? assetSrc(s.coverUrl) : null} />
						<div class="titles">
							<a class="title" href={href(`/series/${s.id}`)}>{s.title}</a>
							<span class="detail k-truncate">
								<span class="k-mono">#{s.id}</span>{s.author ? ` · ${s.author}` : ''}
							</span>
						</div>
					</div>
					<span role="cell" data-label="Type">{sentence(s.type)}</span>
					<span role="cell" data-label="Status">
						<span class="k-badge" data-tone={STATUS_TONE[s.status]}>{sentence(s.status)}</span>
					</span>
					<span role="cell" class="k-num" data-label="Chapters">{s.chapterCount}</span>
					<div role="cell" data-label="Source">
						<div class="stack">
							{#if isCanonical(s)}
								<span
									class="src"
									title="A search lists catalogue works, not the series of one source."
								>
									<span class="src-name k-truncate">Catalogue work</span>
									<span class="k-mono k-muted k-truncate">{s.id}</span>
								</span>
							{:else if provenance[s.id]}
								{#if provenance[s.id].workId && provenance[s.id].sources.length > 0}
									{#each provenance[s.id].sources as src (`${src.sourceType}:${src.sourceId}:${src.sourceKey}`)}
										<span
											class="src"
											title="{src.extension?.pkgName ?? src.sourceType}, source {src.sourceId}"
										>
											<span class="src-name k-truncate">{extLabel(src)}</span>
											<span class="k-mono k-muted k-truncate">{src.sourceId}</span>
										</span>
									{/each}
								{:else if !addMsg[s.id]}
									<span class="detail">Not in the catalogue</span>
								{/if}
							{:else if !addMsg[s.id]}
								<span class="detail">{provenancePending ? 'Checking' : 'Unknown'}</span>
							{/if}
							{#if addMsg[s.id]}
								<span class="src">
									<span class="k-badge" data-tone="success">{decisionLabel(addMsg[s.id])}</span>
									<span class="k-mono k-muted k-truncate">{addMsg[s.id].workId}</span>
								</span>
							{/if}
						</div>
					</div>
					<div role="cell" data-label="Scan">
						<div class="stack">
							{#if s.scan.paused}
								<span class="k-badge" data-tone="warning">Paused</span>
							{:else}
								<span class="k-badge" data-tone="success">Scanning</span>
							{/if}
							<span class="detail k-num">{cadence(s)}</span>
							{#if !s.scan.paused}
								<span class="detail k-num">Every {s.scan.pollEveryMinutes}m when overdue</span>
							{/if}
						</div>
					</div>
					<div role="cell" class="k-td--end">
						{#if mergeSource}
							{#if mergeSource.seriesId === s.id}
								<span class="k-badge" data-tone="danger">Will be deleted</span>
							{:else if canBeTarget(s)}
								<Button size="sm" variant="primary" onclick={() => pickTarget(s)}
									>Keep this one</Button
								>
							{:else if workIdOf(s) === mergeSource.workId}
								<span class="detail">Same work</span>
							{/if}
						{:else}
							{#if !isCanonical(s)}
								<Button size="sm" title="Edit scan settings" onclick={() => openEditor(s)}
									>Edit</Button
								>
							{:else}
								<Button
									size="sm"
									href={href(`/series/${s.id}`)}
									title="Scan timing and status are set per source series, and a catalogue work has none. Edit its details on the series page."
									>Edit metadata</Button
								>
							{/if}
							{#if !isCanonical(s)}
								{#if s.scan.paused}
									<Button size="sm" busy={pausing === s.id} onclick={() => setPaused(s, false)}
										>Unpause</Button
									>
								{:else}
									<Button size="sm" busy={pausing === s.id} onclick={() => setPaused(s, true)}
										>Pause</Button
									>
								{/if}
							{/if}
							{#if canAdd(s)}
								<Button size="sm" busy={addingId === s.id} onclick={() => onAdd(s)}>Add</Button>
							{/if}
							{#if workIdOf(s)}
								<Button
									size="sm"
									onclick={() => startMerge(s)}
									title="Merge this work into another one. This one is deleted.">Merge…</Button
								>
							{/if}
						{/if}
					</div>
				</div>
			{/each}
		</div>
	</LoadState>

	{#if page > 1 || hasNextPage}
		<Pager
			{page}
			hasNext={hasNextPage}
			busy={loading}
			detail={pagerDetail}
			onpage={(n) => goPage(n - page)}
		/>
	{/if}
</div>

{#if editing}
	<Dialog
		variant="drawer"
		title="Scan settings"
		description="{editing.title} (#{editing.id})"
		onclose={closeEditor}
	>
		<div class="k-stack">
			<label class="k-field">
				<span class="k-label">Status</span>
				<select
					class="k-select"
					autocomplete="off"
					id="scan-status"
					name="status"
					bind:value={fStatus}
				>
					<option value="source">Use the status from the source</option>
					{#each STATUS_OPTIONS as opt (opt)}
						<option value={opt}>{sentence(opt)}</option>
					{/each}
				</select>
				<span class="k-hint">Replaces the status the source reports for this series.</span>
			</label>

			<div class="k-field">
				<span class="k-label">Scanning</span>
				<div class="seg">
					<Segmented
						label="Scanner"
						value={fScanner}
						options={SCANNER_OPTIONS}
						onchange={(v) => (fScanner = v)}
					/>
				</div>
				<span class="k-hint">
					Auto stops scanning a series that is completed, on hiatus or cancelled. The other two
					apply whatever the status is.
				</span>
			</div>

			<label class="k-field">
				<span class="k-label">Hours between scans</span>
				<input
					class="k-input"
					id="scan-interval"
					name="interval"
					autocomplete="off"
					type="text"
					inputmode="decimal"
					placeholder="Automatic"
					aria-invalid={intervalError ? 'true' : undefined}
					aria-describedby="scan-interval-note"
					bind:value={fInterval}
				/>
				{#if intervalError}
					<span class="field-error" id="scan-interval-note">{intervalError}</span>
				{:else}
					<span class="k-hint" id="scan-interval-note">
						Leave blank to use the average time between chapters.
					</span>
				{/if}
			</label>

			<label class="k-field">
				<span class="k-label">Minutes between checks when a chapter is overdue</span>
				<input
					class="k-input"
					id="scan-poll"
					name="poll"
					autocomplete="off"
					type="text"
					inputmode="numeric"
					placeholder="30"
					aria-invalid={pollError ? 'true' : undefined}
					aria-describedby="scan-poll-note"
					bind:value={fPoll}
				/>
				{#if pollError}
					<span class="field-error" id="scan-poll-note">{pollError}</span>
				{:else}
					<span class="k-hint" id="scan-poll-note">Leave blank to use the default.</span>
				{/if}
			</label>

			<p class="k-hint">
				{editing.scan.lastScannedAt
					? `Last scanned ${new Date(editing.scan.lastScannedAt).toLocaleString()}.`
					: 'Never scanned.'}
				Scan now checks the source straight away, even when scanning is paused or not yet due.
			</p>

			{#if saveError}
				<div class="k-notice" data-tone="danger" role="alert">
					<Icon name="alert" />
					<div class="k-notice__body">{saveError}</div>
				</div>
			{/if}
			{#if scanError}
				<div class="k-notice" data-tone="danger" role="alert">
					<Icon name="alert" />
					<div class="k-notice__body">Scan failed. {scanError}</div>
				</div>
			{/if}
		</div>
		{#snippet footer()}
			<Button icon="refresh" busy={scanning} onclick={scanNow}>Scan now</Button>
			<span class="k-spacer"></span>
			<Button variant="primary" busy={saving} onclick={save}>Save settings</Button>
		{/snippet}
	</Dialog>
{/if}

{#if mergeSource && mergeTarget}
	<Dialog size="lg" title="Merge these works?" onclose={backToTargets}>
		<div class="k-stack">
			<p class="merge-lede">
				<strong>{mergeSource.title}</strong> will be deleted. Its sources and everything readers
				saved on it (library entries, reading progress, reviews and comments) move to
				<strong>{mergeTarget.title}</strong>. Check that both are the same series. This cannot be
				undone.
			</p>
			<div class="sides">
				{@render mergeSide(mergeSource, false)}
				<span class="sides-arrow"><Icon name="chevron-right" size={20} /></span>
				{@render mergeSide(mergeTarget, true)}
			</div>
			{#if mergeError}
				<div class="k-notice" data-tone="danger" role="alert">
					<Icon name="alert" />
					<div class="k-notice__body">The merge failed and nothing was changed. {mergeError}</div>
				</div>
			{/if}
		</div>
		{#snippet footer()}
			<Button variant="ghost" disabled={merging} onclick={backToTargets}>Back</Button>
			<Button variant="danger-solid" busy={merging} onclick={confirmMerge}
				>Merge and delete source</Button
			>
		{/snippet}
	</Dialog>
{/if}

<style>
	.k-table {
		--cols: minmax(0, 2.4fr) 68px 100px 80px minmax(0, 1.2fr) 150px 204px;
	}
	.find {
		display: grid;
		gap: var(--k-space-3);
		min-width: 0;
	}
	.k-search {
		max-width: 440px;
	}
	.mode {
		max-width: 86ch;
		font-size: var(--k-text-sm);
		line-height: 1.6;
	}
	.k-notice strong {
		color: var(--k-text-bright);
	}

	/* ---- rows ---- */
	.series {
		display: flex;
		align-items: center;
		gap: var(--k-space-3);
	}
	.titles {
		display: grid;
		gap: 2px;
		min-width: 0;
	}
	.title {
		display: -webkit-box;
		-webkit-box-orient: vertical;
		-webkit-line-clamp: 2;
		line-clamp: 2;
		overflow: hidden;
		overflow-wrap: anywhere;
		color: var(--k-text-bright);
		font-weight: 700;
		line-height: 1.35;
	}
	.title:hover {
		text-decoration: underline;
		text-underline-offset: 3px;
	}
	.detail {
		color: var(--k-text-dim);
		font-size: var(--k-text-xs);
	}
	.stack {
		display: grid;
		justify-items: start;
		gap: 4px;
		min-width: 0;
	}
	.src {
		display: grid;
		gap: 2px;
		min-width: 0;
		max-width: 100%;
	}
	/* A badge keeps its own width; the text lines beside it fill the cell and truncate. */
	.src > .k-badge {
		justify-self: start;
	}
	.src-name {
		color: var(--k-text-1);
		font-weight: 600;
	}
	.k-td--end {
		align-items: center;
	}

	/* ---- scan settings drawer ---- */
	.seg {
		display: flex;
		min-width: 0;
	}
	.field-error {
		color: var(--k-danger);
		font-size: var(--k-text-xs);
		line-height: 1.55;
	}

	/* ---- merge confirm ---- */
	.merge-lede {
		color: var(--k-text-2);
		font-size: var(--k-text-md);
		line-height: 1.65;
	}
	.merge-lede strong {
		color: var(--k-text-bright);
	}
	.sides {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
		align-items: stretch;
		gap: var(--k-space-3);
	}
	.side {
		display: grid;
		align-content: start;
		justify-items: start;
		gap: var(--k-space-2);
		min-width: 0;
		padding: var(--k-space-4);
		border: 1px solid var(--k-border-2);
		border-radius: var(--k-radius);
		background: var(--k-surface);
	}
	.side-title {
		color: var(--k-text-bright);
		font-size: var(--k-text-lg);
		font-weight: 700;
		line-height: 1.3;
		overflow-wrap: anywhere;
	}
	.side-id {
		overflow-wrap: anywhere;
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.sides-arrow {
		display: grid;
		place-items: center;
		color: var(--k-text-dim);
	}

	/* Between the phone layout and a full-width desktop the seven columns do not fit:
	   each row becomes a three-column block with captioned cells. */
	@media (min-width: 721px) and (max-width: 1360px) {
		.k-table {
			--cols: repeat(3, minmax(0, 1fr));
		}
		.k-tr--head {
			display: none;
		}
		.k-tr {
			align-items: start;
		}
		.series,
		.k-td--end {
			grid-column: 1 / -1;
		}
		.k-td--end {
			justify-content: flex-start;
		}
		.k-tr > [data-label]::before {
			content: attr(data-label);
			display: block;
			margin-bottom: 4px;
			color: var(--k-text-dim);
			font-size: 11px;
			font-weight: 700;
			letter-spacing: 0.05em;
			text-transform: uppercase;
		}
	}
	@media (max-width: 720px) {
		.stack {
			justify-items: end;
		}
		.src > .k-badge {
			justify-self: end;
		}
		.sides {
			grid-template-columns: minmax(0, 1fr);
		}
		.sides-arrow {
			transform: rotate(90deg);
		}
	}
</style>
