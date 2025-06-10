/**
 * Admin data layer — catalog listing + override writes, straight through the
 * unified backend (no mock fallback: the console requires the real API).
 */
import type {
	AdminUser,
	CoverIssue,
	BulkAddResult,
	CanonicalUpdate,
	ExtensionInfo,
	Id,
	MatchResult,
	MergeCandidate,
	MergeWorksResult,
	Paginated,
	Series,
	SeriesSourceGroup,
	SeriesStatus,
	SourceBrowsePage,
	SourceBrowseType,
	SourceIngestJob,
	SourceInfo,
	WorkReviewDetail,
	WorkSource,
} from '@komika/types';
import type {
	AdminChapter,
	ChapterOverrideInput,
	SeriesAdminInput,
	SeriesAdminMeta,
	SeriesMetadataInput,
} from '@komika/api';
import { backend } from './context';

/**
 * Fail a long admin mutation client-side after `ms` so a stalled connection can't pin
 * the console on "Saving…" forever.
 *
 * This is a UI-level deadline only: the GraphQL client in `@komika/api` takes no
 * `AbortSignal`, so the underlying request is NOT cancelled and the server-side work
 * keeps running — the message says so. Real cancellation needs an `AbortSignal`
 * plumbed through `createBackend`/`gql()` in packages/api.
 */
function withTimeout<T>(op: Promise<T>, ms: number, what: string): Promise<T> {
	let timer: ReturnType<typeof setTimeout>;
	const deadline = new Promise<never>((_, reject) => {
		timer = setTimeout(
			() =>
				reject(
					new Error(
						`${what} did not respond within ${Math.round(ms / 1000)}s. It may still be running on the server — refresh to check before retrying.`,
					),
				),
			ms,
		);
	});
	return Promise.race([op, deadline]).finally(() => clearTimeout(timer)) as Promise<T>;
}

/**
 * The console asks for NSFW-inclusive results on EVERY catalogue read, regardless of
 * the signed-in admin's own `show_nsfw`.
 *
 * It is a per-request override, not a preference: the server honours it only for an
 * admin (anonymous callers are forced to false before it is consulted, ordinary users
 * fall through to their stored preference), so it can't bypass the gate. Without it an
 * opted-out admin's console silently omits every NSFW-flagged work — including the
 * ~2.5k mainstream titles that are WRONGLY flagged, which are precisely the records
 * this console exists to correct. The header's `NSFW on/off` pill still governs the
 * account PREFERENCE (source browsing / extension install / ingest are gated on it
 * server-side, and it is what the reader honours) — the two are deliberately separate.
 */
const ADMIN_INCLUDE_NSFW = true;

/**
 * The catalog the console manages, ONE server page at a time. A text query hits the
 * live source index; an empty query serves the whole persisted catalogue — both
 * paginated SERVER-SIDE by {@link backend.search} (page/pageSize), so the console
 * never pulls the unbounded full library into memory. Returns the paginated envelope
 * (`items` + `page` + `hasNextPage` + `total`) so the caller can drive its pager off
 * the server's own paging metadata.
 *
 * Always NSFW-inclusive — see {@link ADMIN_INCLUDE_NSFW}.
 */
export async function loadCatalog(query: string, page = 1): Promise<Paginated<Series>> {
	return backend.search(query.trim(), page, undefined, ADMIN_INCLUDE_NSFW);
}

/** Persist per-series overrides (whole-state) and return the recomputed series. */
export async function saveSeriesAdmin(input: SeriesAdminInput): Promise<Series> {
	if (!backend.updateSeriesAdmin) throw new Error('Admin API is unavailable on this backend.');
	return backend.updateSeriesAdmin(input);
}

/** Force an immediate re-scan of one series and return its refreshed state. */
export async function triggerScan(seriesId: string): Promise<Series> {
	if (!backend.triggerScan) throw new Error('Manual scan is unavailable on this backend.');
	return backend.triggerScan(seriesId);
}

/** Paginated user list for the user-management console. */
export async function loadUsers(page = 1): Promise<Paginated<AdminUser>> {
	if (!backend.users) throw new Error('User management is unavailable on this backend.');
	return backend.users(page);
}

/** Suspend or restore a user account. */
export async function setUserBanned(userId: string, banned: boolean): Promise<void> {
	if (!backend.banUser) throw new Error('User management is unavailable on this backend.');
	await backend.banUser(userId, banned);
}

/** Grant or revoke a user's admin flag; returns the updated user. */
export async function setUserAdmin(userId: string, isAdmin: boolean): Promise<AdminUser> {
	if (!backend.setUserAdmin) throw new Error('User management is unavailable on this backend.');
	return backend.setUserAdmin(userId, isAdmin);
}

/**
 * Set the signed-in admin's own NSFW visibility preference, returning the new value.
 * This is not a cosmetic setting for the console: `sourceBrowse`, `installExtension`
 * and `startSourceIngest` all REFUSE an opted-out admin. It no longer affects what the
 * console LISTS, though — catalogue search and canonical updates are read with the
 * {@link ADMIN_INCLUDE_NSFW} per-request override, so the records stay reachable
 * either way.
 */
export async function setShowNsfw(value: boolean): Promise<boolean> {
	if (!backend.setShowNsfw) throw new Error('NSFW preference is unavailable on this backend.');
	return backend.setShowNsfw(value);
}

/** Rows per {@link loadMergeQueue} page (the server's own default; max 200). */
export const MERGE_QUEUE_PAGE_SIZE = 50;

/**
 * Pending dedup matches awaiting manual review (CATALOGUE.md §4), ONE server page at
 * a time — the backlog runs to ~10k rows now that refused auto-consolidation pairs are
 * routed here, so it is never pulled whole. `limit` is clamped server-side to 1..=200.
 */
export async function loadMergeQueue(
	page = 1,
	limit = MERGE_QUEUE_PAGE_SIZE,
): Promise<Paginated<MergeCandidate>> {
	if (!backend.mergeQueue) throw new Error('Dedup review is unavailable on this backend.');
	return backend.mergeQueue(page, limit);
}

/**
 * Resolve a dedup review. `accept` merges the source series into the candidate
 * work; rejecting keeps it as a distinct first-class work. Returns true when the
 * row was closed.
 */
export async function resolveMergeCandidate(id: string, accept: boolean): Promise<boolean> {
	if (!backend.resolveMergeCandidate)
		throw new Error('Dedup review is unavailable on this backend.');
	return backend.resolveMergeCandidate(id, accept);
}

/**
 * Expand one side of a dedup candidate for the review panel. Deliberately NOT
 * {@link loadSeriesDetail}: that routes `w_` ids through `canonicalSeries`, which
 * errors on any work without a MangaDex anchor and hides NSFW works — between them
 * that is most of the review queue, so the rows needing the closest look are exactly
 * the ones it would refuse.
 */
export async function loadWorkReviewDetail(workId: string): Promise<WorkReviewDetail> {
	if (!backend.workReviewDetail)
		throw new Error('Work review detail is unavailable on this backend.');
	return backend.workReviewDetail(workId);
}

/** Cover "Bugs" panel: works whose cover the crawl couldn't process, paginated. */
export async function loadCoverIssues(page = 1): Promise<Paginated<CoverIssue>> {
	if (!backend.coverIssues) throw new Error('Cover issues are unavailable on this backend.');
	return backend.coverIssues(page);
}

/** Re-attempt cover processing for one work. Returns true if a cover was stored. */
export async function retryCover(workId: string): Promise<boolean> {
	if (!backend.retryCover) throw new Error('Cover retry is unavailable on this backend.');
	return backend.retryCover(workId);
}

/** Replace one work's cover from an uploaded image. Returns the new cover URL. */
export async function uploadCover(workId: string, file: Blob): Promise<string> {
	if (!backend.uploadCover) throw new Error('Cover upload is unavailable on this backend.');
	return backend.uploadCover(workId, file);
}

/**
 * Fold one canonical work into another (admin D1): re-point the source work's
 * source_series mappings + user data to the target, then DELETE the source work.
 * The target survives as canonical. IRREVERSIBLE.
 */
export async function mergeWorks(sourceWorkId: Id, targetWorkId: Id): Promise<MergeWorksResult> {
	if (!backend.mergeWorks) throw new Error('Work merge is unavailable on this backend.');
	return backend.mergeWorks(sourceWorkId, targetWorkId);
}

/**
 * Run the Tier-2 dedup add flow for a Suwayomi manga id: link it to a canonical
 * work (auto-merge / queue for review / create new), idempotently. Returns the
 * matcher's decision.
 */
export async function addSourceSeries(suwayomiMangaId: string): Promise<MatchResult> {
	if (!backend.addSourceSeries)
		throw new Error('Add source series is unavailable on this backend.');
	return backend.addSourceSeries(suwayomiMangaId);
}

/**
 * Recently-updated mirrored MangaDex works + their latest stored chapter, from the
 * canonical `chapter` mirror (CATALOGUE.md §6). A monitoring feed for the mirror —
 * these works are not reader-openable yet.
 *
 * Always NSFW-inclusive — see {@link ADMIN_INCLUDE_NSFW}.
 */
export async function loadCanonicalUpdates(page = 1): Promise<CanonicalUpdate[]> {
	if (!backend.canonicalUpdates)
		throw new Error('Catalogue updates are unavailable on this backend.');
	return backend.canonicalUpdates(page, ADMIN_INCLUDE_NSFW);
}

// ---- Sources & Extensions console (EXT-2) -----------------------------------

/**
 * Every Keiyoushi/Mihon extension known to the Suwayomi engine (installed or
 * not). First use auto-seeds the curated Keiyoushi store; NSFW extensions are
 * hidden unless the admin opted in via show_nsfw (CATALOGUE.md §2).
 * `refresh: true` re-fetches the store indexes so hasUpdate/versions are fresh.
 */
export async function loadExtensions(refresh = false): Promise<ExtensionInfo[]> {
	if (!backend.extensions) throw new Error('Extension management is unavailable on this backend.');
	return backend.extensions(refresh);
}

/** Register an extension repo by index URL; returns the extension count after refresh. */
export async function addExtensionRepo(indexUrl: string): Promise<number> {
	if (!backend.addExtensionRepo)
		throw new Error('Extension management is unavailable on this backend.');
	return backend.addExtensionRepo(indexUrl);
}

/** Install a store extension onto the Suwayomi engine (NSFW-gated server-side). */
export async function installExtension(pkgName: string): Promise<ExtensionInfo> {
	if (!backend.installExtension)
		throw new Error('Extension management is unavailable on this backend.');
	return backend.installExtension(pkgName);
}

/** Uninstall an extension from the Suwayomi engine. */
export async function uninstallExtension(pkgName: string): Promise<ExtensionInfo> {
	if (!backend.uninstallExtension)
		throw new Error('Extension management is unavailable on this backend.');
	return backend.uninstallExtension(pkgName);
}

/** Update an installed extension to the store's latest version. */
export async function updateExtension(pkgName: string): Promise<ExtensionInfo> {
	if (!backend.updateExtension)
		throw new Error('Extension management is unavailable on this backend.');
	return backend.updateExtension(pkgName);
}

/** The installed Suwayomi sources — the picker feeding {@link browseSource}. */
export async function loadSources(): Promise<SourceInfo[]> {
	if (!backend.sources) throw new Error('Source browsing is unavailable on this backend.');
	return backend.sources();
}

/** Browse/search one source's catalogue (paged) for the bulk-ingest picker. */
export async function browseSource(
	sourceId: Id,
	type: SourceBrowseType,
	page = 1,
	query?: string,
): Promise<SourceBrowsePage> {
	if (!backend.sourceBrowse) throw new Error('Source browsing is unavailable on this backend.');
	return backend.sourceBrowse(sourceId, type, page, query);
}

/**
 * Bulk Tier-2 catalogue ingest: library-track each Suwayomi manga and run the
 * dedup add flow. Per-id failures never abort the batch; at most 100 ids.
 */
export async function bulkAddSourceSeries(suwayomiMangaIds: Id[]): Promise<BulkAddResult> {
	if (!backend.bulkAddSourceSeries)
		throw new Error('Bulk catalogue ingest is unavailable on this backend.');
	return backend.bulkAddSourceSeries(suwayomiMangaIds);
}

/**
 * Catalogue provenance for many Suwayomi series at once: which canonical work
 * each is linked to and every source mapping (with extension coordinates) on
 * that work. One group per id, in input order; max 200 ids per call.
 */
export async function loadSeriesSources(seriesIds: Id[]): Promise<SeriesSourceGroup[]> {
	if (!backend.seriesSourcesBatch)
		throw new Error('Catalogue provenance is unavailable on this backend.');
	return backend.seriesSourcesBatch(seriesIds);
}

/**
 * Pause or unpause one series' scanning (targeted override — leaves the other
 * admin overrides alone). Unpausing triggers an immediate server-side re-scan;
 * returns the recomputed series.
 */
export async function setSeriesPaused(seriesId: Id, paused: boolean): Promise<Series> {
	if (!backend.setSeriesPaused) throw new Error('Pause management is unavailable on this backend.');
	return backend.setSeriesPaused(seriesId, paused);
}

/**
 * The "add all from this source" ingest jobs (S1), newest first. Pass
 * `active: true` for only the currently-running ones — poll it for live
 * progress while a job runs.
 */
export async function loadSourceIngestJobs(active = false): Promise<SourceIngestJob[]> {
	if (!backend.sourceIngestJobs) throw new Error('Source ingest is unavailable on this backend.');
	return backend.sourceIngestJobs(active);
}

/**
 * Start a background ingest that walks a source's whole catalogue through the
 * Tier-2 dedup add flow. Refused while one is already running for the source,
 * and for an NSFW source unless the admin opted in.
 */
export async function startSourceIngest(sourceId: Id): Promise<SourceIngestJob> {
	if (!backend.startSourceIngest) throw new Error('Source ingest is unavailable on this backend.');
	return backend.startSourceIngest(sourceId);
}

/** Request cancellation of a running ingest job; progress so far is preserved. */
export async function cancelSourceIngest(jobId: Id): Promise<SourceIngestJob> {
	if (!backend.cancelSourceIngest) throw new Error('Source ingest is unavailable on this backend.');
	return backend.cancelSourceIngest(jobId);
}

/**
 * Start one ingest job per installed source of an extension (F1). NSFW sources
 * are skipped for an opted-out admin; a source already running returns its
 * existing job instead of erroring. Errors only when no source matches. Returns
 * every started + already-running job.
 */
export async function startExtensionIngest(pkgName: Id): Promise<SourceIngestJob[]> {
	if (!backend.startExtensionIngest)
		throw new Error('Extension ingest is unavailable on this backend.');
	return backend.startExtensionIngest(pkgName);
}

/** Cancel every running ingest job for an extension's sources; returns the cancelled jobs. */
export async function cancelExtensionIngest(pkgName: Id): Promise<SourceIngestJob[]> {
	if (!backend.cancelExtensionIngest)
		throw new Error('Extension ingest is unavailable on this backend.');
	return backend.cancelExtensionIngest(pkgName);
}

/**
 * Subscribe/unsubscribe an extension for background source-sync: while subscribed, the
 * server periodically re-walks the extension's sources to auto-discover newly-added
 * series and keep enrolled series in the library (so they keep updating). Enabling
 * kicks an immediate sync pass server-side. Returns the new subscribed state.
 */
export async function setExtensionSubscription(pkgName: Id, subscribed: boolean): Promise<boolean> {
	if (!backend.setExtensionSubscription)
		throw new Error('Extension sync is unavailable on this backend.');
	return backend.setExtensionSubscription(pkgName, subscribed);
}

/**
 * Maintenance: materialize the whole Suwayomi library into the DB read-cache.
 * Series metadata is written synchronously (the returned count); per-series
 * chapter lists fill in a server-side background task. Admin-gated server-side.
 * Returns how many series were persisted.
 */
export async function persistCatalogue(): Promise<number> {
	if (!backend.persistCatalogue)
		throw new Error('Catalogue persistence is unavailable on this backend.');
	return withTimeout(backend.persistCatalogue(), 10 * 60_000, 'Catalogue persistence');
}

/**
 * Maintenance: materialize every canonical work's cover into the DB
 * (`work_cover_blob`) so the web reader serves covers from `/covers/{id}.webp`
 * instead of the Cloudflare image Worker. Kicks off a polite background crawl and
 * returns how many works are still uncached (queued) at kick-off. Admin-gated.
 */
export async function materializeCatalogueCovers(): Promise<number> {
	if (!backend.materializeCatalogueCovers)
		throw new Error('Cover materialization is unavailable on this backend.');
	return withTimeout(backend.materializeCatalogueCovers(), 5 * 60_000, 'Cover materialization');
}

// ---- Series-detail editor (metadata + chapters + rescan) --------------------

/**
 * Load one series for the detail page. A `w_` id routes to the canonical path
 * (`canonicalSeries`, an optional method — guarded with a readable error); a numeric
 * Suwayomi id routes to `series`.
 */
export async function loadSeriesDetail(seriesId: string): Promise<Series> {
	if (seriesId.startsWith('w_')) {
		if (!backend.canonicalSeries)
			throw new Error('Canonical series lookup is unavailable on this backend.');
		return backend.canonicalSeries(seriesId);
	}
	return backend.series(seriesId);
}

/** Raw metadata-override state of a series' canonical work (pinned vs derived). */
export async function loadSeriesAdminMeta(seriesId: string): Promise<SeriesAdminMeta> {
	if (!backend.seriesAdminMeta)
		throw new Error('Series metadata editing is unavailable on this backend.');
	return backend.seriesAdminMeta(seriesId);
}

/** Save metadata overrides (title/description/type/nsfw/tags); returns the series. */
export async function saveSeriesMetadata(input: SeriesMetadataInput): Promise<Series> {
	if (!backend.updateSeriesMetadata)
		throw new Error('Series metadata editing is unavailable on this backend.');
	return backend.updateSeriesMetadata(input);
}

/** Add an alternative title; auto-merges any other work sharing it. Returns the series. */
export async function addSeriesAltTitle(id: string, title: string): Promise<Series> {
	if (!backend.addSeriesAltTitle)
		throw new Error('Alt-title editing is unavailable on this backend.');
	return backend.addSeriesAltTitle(id, title);
}

/** Remove an alternative title. Returns the recomputed series. */
export async function removeSeriesAltTitle(id: string, title: string): Promise<Series> {
	if (!backend.removeSeriesAltTitle)
		throw new Error('Alt-title editing is unavailable on this backend.');
	return backend.removeSeriesAltTitle(id, title);
}

/**
 * Merge one batch of the duplicate-work backlog — up to `limit` clusters per call.
 * Returns how many works were merged away (0 when the backlog is clear).
 *
 * DESTRUCTIVE: each cluster folds via `merge_works`, which physically DELETEs the
 * losing work. Clusters are grouped by NORMALIZED ALIAS (alternative titles included),
 * so this is broader than "same exact title" — callers must confirm it and bound the
 * number of batches rather than looping until the server returns zero.
 */
export async function consolidateDuplicates(limit?: number): Promise<number> {
	if (!backend.consolidateExactDuplicates)
		throw new Error('Duplicate consolidation is unavailable on this backend.');
	return withTimeout(
		backend.consolidateExactDuplicates(limit),
		3 * 60_000,
		'Duplicate consolidation',
	);
}

/** The source mappings for one canonical work (per-source rescan + provenance). */
export async function loadWorkSources(workId: string): Promise<WorkSource[]> {
	if (!backend.workSources) throw new Error('Work sources are unavailable on this backend.');
	return backend.workSources(workId);
}

/** A work's aggregated chapters WITH override state (hidden/renamed), unfiltered. */
export async function loadWorkChaptersAdmin(workId: string): Promise<AdminChapter[]> {
	if (!backend.workChaptersAdmin)
		throw new Error('Chapter editing is unavailable on this backend.');
	return backend.workChaptersAdmin(workId);
}

/** Soft-hide (reversible) or rename one chapter of a work. */
export async function setChapterOverride(input: ChapterOverrideInput): Promise<boolean> {
	if (!backend.setChapterOverride)
		throw new Error('Chapter editing is unavailable on this backend.');
	return backend.setChapterOverride(input);
}

/** Force an immediate re-scan of every Suwayomi source of a work; returns count. */
export async function rescanWork(workId: string): Promise<number> {
	if (!backend.rescanWork) throw new Error('Rescan is unavailable on this backend.');
	return backend.rescanWork(workId);
}

export const COMIC_TYPE_OPTIONS = ['MANGA', 'MANHWA', 'MANHUA', 'WEBTOON', 'COMIC'] as const;

export const STATUS_OPTIONS: SeriesStatus[] = [
	'ONGOING',
	'COMPLETED',
	'HIATUS',
	'CANCELLED',
	'UNKNOWN',
];
