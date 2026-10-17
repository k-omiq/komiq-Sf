/**
 * Admin data layer — catalog listing + override writes, straight through the
 * unified backend (no mock fallback: the console requires the real API).
 */
import type {
	AdminUser,
	CoverIssue,
	BulkAddResult,
	CanonicalUpdate,
	Chapter,
	ExtensionInfo,
	Id,
	MatchResult,
	MergeCandidate,
	DuplicateSweepResult,
	MergeWorksResult,
	Paginated,
	Report,
	ReportKind,
	ReportPage,
	ReportStatus,
	Series,
	SeriesSourceGroup,
	SeriesStatus,
	SourceBrowsePage,
	SourceBrowseType,
	SourceIngestJob,
	SourceInfo,
	SourceScanHealth,
	ExtensionAutoUpdate,
	TagInfo,
	WorkReviewDetail,
	WorkSource,
} from '@komika/types';
import type {
	AccessMode,
	AccessPreflight,
	AccessStatus,
	BlockRule,
	BrowseSource,
	CertificateStatus,
	AdminChapter,
	ChapterOverrideInput,
	CloudflareConnection,
	CloudflareSetupInput,
	CloudflareTokenCheck,
	CreateOwnerInput,
	CreateUserInput,
	ExtensionRepo,
	EngineGeneration,
	MangadexSetting,
	ScanlatorAi,
	ScanlatorPage,
	ScanlatorQuery,
	ScanlatorSite,
	Session,
	SourceRateLimit,
	SeriesAdminInput,
	SeriesAdminMeta,
	SeriesMetadataInput,
	WorkChapterEdit,
	WorkChapterEntry,
	WorkChapterList,
} from '@komika/api';
import { backend } from './context';
import { assetSrc } from './config';
import { isUnknownEngineStateField } from './engine-generation';

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

/**
 * Delete a comment and its whole reply subtree. IRREVERSIBLE — there is no tombstone.
 *
 * This is what an abuse report is FOR, so the Reports page needs it; the reader-side
 * moderator UI had the only other button. The report survives the deletion because it
 * snapshots the body and author at submit time.
 */
export async function deleteComment(commentId: Id): Promise<boolean> {
	if (!backend.deleteComment) throw new Error('Comment moderation is unavailable on this backend.');
	return backend.deleteComment(commentId);
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

/**
 * Reader-report queue (reader Support → Report an issue), newest first.
 *
 * `status`/`kind` null mean "every value". `total` is the FILTERED count (for the pager);
 * `openCount` on the same envelope is always the global open backlog, so the header can
 * show the real size of the queue while looking at a filtered slice of it.
 */
export async function loadReports(
	status: ReportStatus | null = 'OPEN',
	kind: ReportKind | null = null,
	page = 1,
): Promise<ReportPage> {
	if (!backend.reports) throw new Error('Reports are unavailable on this backend.');
	return backend.reports(status, kind, page);
}

/** Triage one report: resolve / reject, or reopen. `note` is merged, not cleared. */
export async function resolveReport(
	reportId: Id,
	status: ReportStatus,
	note: string | null = null,
): Promise<Report> {
	if (!backend.resolveReport) throw new Error('Reports are unavailable on this backend.');
	return backend.resolveReport(reportId, status, note);
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
 * Every Mihon extension known to the Suwayomi engine (installed or not). No
 * store is seeded — the list is empty until a repo is added. NSFW extensions are
 * always listed here, whatever the admin's own show_nsfw preference: an admin has
 * to see an extension to manage it.
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

/** The extension repos configured on the engine. */
export async function loadExtensionRepos(): Promise<ExtensionRepo[]> {
	if (!backend.extensionRepos)
		throw new Error('Extension management is unavailable on this backend.');
	return backend.extensionRepos();
}

/** Remove an extension repo by the index URL `loadExtensionRepos` reported. */
export async function removeExtensionRepo(indexUrl: string): Promise<boolean> {
	if (!backend.removeExtensionRepo)
		throw new Error('Extension management is unavailable on this backend.');
	return backend.removeExtensionRepo(indexUrl);
}

/** The MangaDex switch and whether the server's environment fixes it. */
export async function loadMangadexSetting(): Promise<MangadexSetting> {
	if (!backend.mangadexSetting) setupUnavailable();
	return backend.mangadexSetting();
}

/** Null means this backend or server predates engine-state reporting. */
export async function loadEngineGenerations(): Promise<EngineGeneration[] | null> {
	if (!backend.engineGenerationState) return null;
	try {
		return await backend.engineGenerationState();
	} catch (err) {
		if (isUnknownEngineStateField(err)) return null;
		throw err;
	}
}

/** Set aside the old extension-source data and return the updated generations. */
export async function startNewEngineGeneration(generationId: string): Promise<EngineGeneration[]> {
	if (!backend.startNewEngineGeneration) setupUnavailable();
	return backend.startNewEngineGeneration(generationId);
}

/** Turn MangaDex on or off; returns the new effective value. */
export async function setMangadexEnabled(enabled: boolean): Promise<boolean> {
	if (!backend.setMangadexEnabled) setupUnavailable();
	return backend.setMangadexEnabled(enabled);
}

/** Show or hide MangaDex's rating for every reader; returns the new effective value. */
export async function setShowMangadexRatings(shown: boolean): Promise<boolean> {
	if (!backend.setShowMangadexRatings) setupUnavailable();
	return backend.setShowMangadexRatings(shown);
}

/** Every per-source scan rate limit in force, the server's own sources first. */
export async function loadSourceRateLimits(): Promise<SourceRateLimit[]> {
	if (!backend.sourceRateLimits) setupUnavailable();
	return backend.sourceRateLimits();
}

/** Add or replace the scan rate limit of one source; the server applies it at once. */
export async function setSourceRateLimit(
	sourceId: Id,
	requests: number,
	windowSecs: number,
): Promise<SourceRateLimit> {
	if (!backend.setSourceRateLimit) setupUnavailable();
	return backend.setSourceRateLimit(sourceId, requests, windowSecs);
}

/** Remove an admin's limit; the source falls back to the server's default, if any. */
export async function removeSourceRateLimit(sourceId: Id): Promise<boolean> {
	if (!backend.removeSourceRateLimit) setupUnavailable();
	return backend.removeSourceRateLimit(sourceId);
}

/** The message the server uses when the source engine is down or still starting. */
export const ENGINE_UNREACHABLE = 'The source engine is not reachable.';

/** True when `err` is the server's "engine not reachable" answer — a state to show
 * calmly (the engine may still be starting), not an error wall. */
export function isEngineUnreachable(err: unknown): boolean {
	return err instanceof Error && err.message.includes(ENGINE_UNREACHABLE);
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
 * The deliberately small reader used from Sources → Catalogue. It reads the
 * source entry directly by its Suwayomi manga id, so previewing a comic does not
 * add it to the canonical catalogue or mutate any library state.
 */
export interface SourceReaderView {
	seriesId: string;
	title: string;
	chapters: Chapter[];
	chapter: Chapter | null;
	externalUrl: string | null;
	pages: { index: number; url: string }[];
}

/** Only allow the same ordinary web links the reader app accepts for off-site chapters. */
function externalChapterUrl(value: string | null | undefined): string | null {
	if (!value) return null;
	try {
		const url = new URL(value);
		return url.protocol === 'http:' || url.protocol === 'https:' ? url.toString() : null;
	} catch {
		return null;
	}
}

/**
 * Load one source chapter for the admin preview. The chapter choice matches the
 * reader app: an explicit id wins, otherwise the first unread chapter (then the
 * first chapter) is opened. Suwayomi page URLs normally point at the API's image
 * proxy; {@link assetSrc} also makes a root-relative URL resolve to that origin.
 */
export async function loadSourceReader(
	seriesId: string,
	title: string,
	chapterId?: string | null,
): Promise<SourceReaderView> {
	const chapters = [...(await backend.chapters(seriesId))].sort((a, b) => a.number - b.number);
	const chapter =
		(chapterId ? chapters.find((candidate) => String(candidate.id) === chapterId) : null) ??
		chapters.find((candidate) => !candidate.read) ??
		chapters[0] ??
		null;
	const externalUrl = externalChapterUrl(chapter?.externalUrl);
	const domainPages = externalUrl ? [] : chapter ? await backend.pages(chapter.id) : [];

	return {
		seriesId,
		title,
		chapters,
		chapter,
		externalUrl,
		pages: domainPages.map((page) => ({ index: page.index, url: assetSrc(page.sourceUrl) })),
	};
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
 * Per-source scan health (Phase E4.3), worst source first.
 *
 * The console had no honest signal for "is this source working?": every Suwayomi fetch
 * path falls back to reading Suwayomi's own database when the upstream fetch fails, so a
 * dead source's scans were recorded as successes. `cachedFallback` is that case, now
 * counted; `zeroChapterSeries` across a whole source means it has never once worked.
 */
export async function loadSourceScanHealth(): Promise<SourceScanHealth[]> {
	if (!backend.sourceScanHealth) throw new Error('Scan health is unavailable on this backend.');
	return backend.sourceScanHealth();
}

/** The extension updater's status and per-extension history. */
export async function loadExtensionAutoUpdate(): Promise<ExtensionAutoUpdate> {
	if (!backend.extensionAutoUpdate)
		throw new Error('Extension update status is unavailable on this backend.');
	return backend.extensionAutoUpdate();
}

/** Start an extension update pass now; it runs server-side in the background. */
export async function runExtensionUpdates(): Promise<ExtensionAutoUpdate> {
	if (!backend.runExtensionUpdates)
		throw new Error('Extension updates are unavailable on this backend.');
	return backend.runExtensionUpdates();
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
 * Sort one batch of the duplicate backlog — up to `limit` waiting reviews, then up to
 * `limit` groups of works that share a title. Each pair is merged, closed as two
 * different works, or left in the review queue. `done` marks the end of a run.
 *
 * DESTRUCTIVE: a merge folds via `merge_works`, which physically DELETEs the losing
 * work. Callers must confirm it and bound the number of batches.
 */
export async function consolidateDuplicates(limit?: number): Promise<DuplicateSweepResult> {
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

/** Older backends leave the editor on the existing aggregate chapter list. */
export async function loadWorkChapterList(workId: string): Promise<WorkChapterList | undefined> {
	return backend.workChapterList?.(workId);
}

/** Edit the work's entry so its linked source copies keep the same list identity. */
export async function editWorkChapter(
	entryId: string,
	edit: WorkChapterEdit,
): Promise<WorkChapterEntry> {
	if (!backend.setWorkChapter) throw new Error('Chapter editing is unavailable on this backend.');
	return backend.setWorkChapter(entryId, edit);
}

/** Relink one source copy, or give it its own entry when entryId is null. */
export async function relinkChapterCopy(
	chapterId: string,
	entryId: string | null,
): Promise<boolean> {
	if (!backend.linkChapterCopy) throw new Error('Chapter editing is unavailable on this backend.');
	return backend.linkChapterCopy(chapterId, entryId);
}

/** Keep one entry's identity while moving the dropped entry's copies into it. */
export async function mergeChapterEntries(keepId: string, dropId: string): Promise<boolean> {
	if (!backend.mergeWorkChapters)
		throw new Error('Chapter editing is unavailable on this backend.');
	return backend.mergeWorkChapters(keepId, dropId);
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

// ---- scanlators ---------------------------------------------------------------

/**
 * One page of the scanlator list (50 rows, most chapters first), each with its AI tag.
 * The server counts the list with one slow scan and reuses it for ten minutes, so the
 * first call can take a few seconds.
 */
export async function loadScanlators(query: ScanlatorQuery = {}): Promise<ScanlatorPage> {
	if (!backend.scanlators) throw new Error('Scanlator tags are unavailable on this backend.');
	return backend.scanlators(query);
}

/** Tag a scanlator as using AI; `null` removes the tag. False when nothing changed. */
export async function setScanlatorAi(name: string, ai: ScanlatorAi | null): Promise<boolean> {
	if (!backend.setScanlatorAi) throw new Error('Scanlator tags are unavailable on this backend.');
	return backend.setScanlatorAi(name, ai);
}

/**
 * The sources as scanlators of their own, biggest first, each with its AI tag. A source
 * is listed once a series has it. The list is not paged.
 */
export async function loadScanlatorSites(): Promise<ScanlatorSite[]> {
	if (!backend.scanlatorSites) throw new Error('Scanlator tags are unavailable on this backend.');
	return backend.scanlatorSites();
}

/** Tag a whole source as using AI; `null` removes the tag. False when nothing changed. */
export async function setSourceAi(sourceId: string, ai: ScanlatorAi | null): Promise<boolean> {
	if (!backend.setSourceAi) throw new Error('Scanlator tags are unavailable on this backend.');
	return backend.setSourceAi(sourceId, ai);
}

/**
 * The sources the scanlator list can be filtered by, biggest first.
 *
 * Always NSFW-inclusive — see {@link ADMIN_INCLUDE_NSFW}.
 */
export async function loadBrowseSources(): Promise<BrowseSource[]> {
	if (!backend.browseSources) throw new Error('Source filters are unavailable on this backend.');
	return backend.browseSources(ADMIN_INCLUDE_NSFW);
}

// ---- blocked content ----------------------------------------------------------

/** The site-wide content block rules, in saved order. */
export async function loadSiteBlockRules(): Promise<BlockRule[]> {
	if (!backend.siteBlockRules) throw new Error('Block rules are unavailable on this backend.');
	return backend.siteBlockRules();
}

/** Replace the whole list of site-wide block rules; returns the list the server stored. */
export async function setSiteBlockRules(rules: BlockRule[]): Promise<BlockRule[]> {
	if (!backend.setSiteBlockRules) throw new Error('Block rules are unavailable on this backend.');
	return backend.setSiteBlockRules(rules);
}

/**
 * The tags a block rule can name.
 *
 * NOT NSFW-inclusive, unlike the lists above: the query takes no override, so it follows
 * the admin's own "Show NSFW content" setting. A tag only adult series carry is left
 * out while that setting is off.
 */
export async function loadTags(): Promise<TagInfo[]> {
	if (!backend.tags) throw new Error('Tags are unavailable on this backend.');
	return backend.tags();
}

// ---- first-run setup ---------------------------------------------------------

function setupUnavailable(): never {
	throw new Error('This server does not support setup. Update the server and try again.');
}

function accountsUnavailable(): never {
	throw new Error('This server cannot manage accounts. Update the server and try again.');
}

/** Create the owner (first admin) with the setup code from the server log. */
export async function createOwner(input: CreateOwnerInput): Promise<Session> {
	if (!backend.createOwner) setupUnavailable();
	return backend.createOwner(input);
}

/** Open or close public sign-up; resolves to the new effective value. */
export async function setRegistrationOpen(open: boolean): Promise<boolean> {
	if (!backend.setRegistrationOpen) accountsUnavailable();
	return backend.setRegistrationOpen(open);
}

/** Create an account directly; the user picks their own password at first sign-in. */
export async function createUser(input: CreateUserInput): Promise<AdminUser> {
	if (!backend.createUser) accountsUnavailable();
	return backend.createUser(input);
}

/**
 * Give another account a new temporary password. Signs them out everywhere; they
 * pick their own at next sign-in.
 */
export async function resetUserPassword(userId: Id, password: string): Promise<void> {
	if (!backend.resetUserPassword) accountsUnavailable();
	await backend.resetUserPassword(userId, password);
}

// ---- access + Cloudflare setup ----------------------------------------------
// The API token passes through these calls only. Nothing here stores it.

function accessUnavailable(): never {
	throw new Error('This server cannot set up access yet. Update the server and try again.');
}

/** Settings › Access, and the live state of a Cloudflare setup or remove run. */
export async function loadAccessStatus(): Promise<AccessStatus> {
	if (!backend.accessStatus) accessUnavailable();
	return backend.accessStatus();
}

/**
 * The server's address for other devices on the home network, when the console's own
 * address is this computer's loopback. Null when there is none to show: the server only
 * answers on its own computer, runs in a container, or predates the question.
 */
export async function loadLanAddress(): Promise<string | null> {
	try {
		return (await backend.lanAddresses?.())?.[0] ?? null;
	} catch {
		return null;
	}
}

/**
 * Record the wizard's access choice (and a domain the owner set up by hand). With
 * `encrypt`, direct mode gets a certificate first: the answer can take a minute.
 */
export async function setAccessMode(
	mode: AccessMode,
	domain?: string | null,
	encrypt?: boolean,
): Promise<AccessStatus> {
	if (!backend.setAccessMode) accessUnavailable();
	return backend.setAccessMode(mode, domain, encrypt);
}

/**
 * Whether the owner must be created in a terminal on the server: this console talks
 * to it over plain HTTP from the internet, where the server refuses `createOwner`.
 * False on a server that predates the check (it does not know the field). Null when the
 * question got no answer: the caller must not show the owner form on a guess.
 */
export async function loadSetupNeedsTerminal(): Promise<boolean | null> {
	if (!backend.setupNeedsTerminal) return false;
	try {
		return await backend.setupNeedsTerminal();
	} catch (err) {
		const message = err instanceof Error ? err.message : '';
		return /(Unknown|Cannot query) field "setupNeedsTerminal"/.test(message) ? false : null;
	}
}

/**
 * The certificate of direct mode with HTTPS. Null when there is nothing to show: that
 * mode is off, or the server predates it.
 */
export async function loadCertificateStatus(): Promise<CertificateStatus | null> {
	try {
		return (await backend.certificateStatus?.()) ?? null;
	} catch {
		return null;
	}
}

/**
 * What the server knows about its machine before access setup: the tunnel connector,
 * the HTTPS ports, the public address. Null when there is nothing to show (the server
 * predates the question, or did not answer): the screens then leave those lines out.
 */
export async function loadAccessPreflight(): Promise<AccessPreflight | null> {
	try {
		return (await backend.accessPreflight?.()) ?? null;
	} catch {
		return null;
	}
}

/**
 * The last time a device on the home network reached this server since it started, or
 * null when none has yet. `undefined` when the server cannot say (it predates the
 * field): stop asking. Any other failure rejects, e.g. the server is restarting; ask
 * again later.
 */
export async function loadLanClientSeenAt(): Promise<string | null | undefined> {
	if (!backend.lanClientSeenAt) return undefined;
	try {
		return await backend.lanClientSeenAt();
	} catch (err) {
		if (err instanceof Error && err.message.includes('Unknown field "lanClientSeenAt"')) {
			return undefined;
		}
		throw err;
	}
}

/**
 * Check the token and find the zone. `connection` also asks which permission rows the
 * token seems to lack for it (`missing`, advice only; empty on an older server).
 */
export async function checkCloudflareToken(
	token: string,
	domain: string,
	connection?: CloudflareConnection,
): Promise<CloudflareTokenCheck> {
	if (!backend.checkCloudflareToken) accessUnavailable();
	return backend.checkCloudflareToken(token, domain, connection);
}

export async function startCloudflareSetup(input: CloudflareSetupInput): Promise<AccessStatus> {
	if (!backend.startCloudflareSetup) accessUnavailable();
	return backend.startCloudflareSetup(input);
}

export async function retryCloudflareSetup(token: string): Promise<AccessStatus> {
	if (!backend.retryCloudflareSetup) accessUnavailable();
	return backend.retryCloudflareSetup(token);
}

export async function removeCloudflareSetup(token: string): Promise<AccessStatus> {
	if (!backend.removeCloudflareSetup) accessUnavailable();
	return backend.removeCloudflareSetup(token);
}
