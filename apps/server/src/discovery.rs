//! Phase E5 — LATEST-diff discovery.
//!
//! ## The problem this replaces
//!
//! The scanner polls every Suwayomi series on a timer and re-fetches its whole chapter
//! list looking for something new. Measured honestly on post-E4 production logs (§8e):
//! **~22,292 live `fetchMangaAndChapters` mutations/day discover ~270 new chapters — a
//! 1.26% hit rate, ~82 fetches per detection.** ~98.8% of the scanner's upstream work
//! finds nothing. Every one of those fetches makes Suwayomi go out to a scanlator site.
//!
//! ## The signal we were throwing away
//!
//! A source's LATEST listing is ordered **by the source itself, newest chapter first**.
//! That ordering is a push signal: when a series gets a new chapter it jumps to (or near)
//! the top, so comparing page 1's order against the previous snapshot reveals exactly
//! which series changed — one page fetch tells us what ~20 individual scans would.
//!
//! This is E2 generalised. E2 (`sync::sync_source_latest`) already reads the same LATEST
//! walk to wake *paused* series that merely APPEAR in it. E5 watches *position change*
//! across *all* statuses, on a tight (~15 min) cadence of its own, and reuses E2's exact
//! sink: [`scanner::trigger_due_now`], which schedules a series due-now so the ordinary
//! scan tick picks it up with all its failure/backoff/persist handling intact. E5 never
//! scans inline.
//!
//! ## What flags a series
//!
//! Diffing this poll's ordered id list against the stored snapshot, a series is flagged if
//! it **entered** the window or **moved up** in rank. A new chapter is the only thing that
//! moves a series up in an upload-time ordering; passively-displaced series move *down* and
//! are not flagged. The one false negative — a rank-1 series updating again without moving —
//! is caught by the slow baseline poll (Phase E's safety net). The first poll of a source
//! (no prior snapshot) baselines and triggers nothing.
//!
//! ## What E5 does NOT change
//!
//! The per-series baseline poll still runs; E5 is a fast push signal layered on top, not a
//! replacement for correctness. Slowing that baseline to a 7-day safety net (where the big
//! saving lands) is Phase E's job, gated on E5 being proven in production first.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::graphql::AppState;
use crate::suwayomi::FetchType;

/// How many page-1 entries to keep in the snapshot and diff against. A genuine new chapter
/// lands a series at rank 0–2, well inside this; the window only bounds how much tail churn
/// (a series oscillating around the boundary as page size varies) can spuriously flag. At
/// measured churn (< 9 updated series/day even on the busiest scanlator source) the
/// boundary is stable across 15-min ticks, so false positives here are rare — and benign:
/// a spurious flag costs exactly one authoritative targeted scan that then finds nothing.
const SNAPSHOT_WINDOW: usize = 40;

/// Supervised spawn: restart the discovery loop if it panics, mirroring `sync::spawn` and
/// `scanner::spawn`. One panicking poll must not take discovery down for the process life.
pub fn spawn(state: Arc<AppState>, interval_seconds: u64, shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_loop(state.clone(), interval_seconds, shutdown.clone()));
            match handle.await {
                Ok(()) => break,
                Err(e) if e.is_panic() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    tracing::error!("discovery loop panicked; restarting in 30s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if *shutdown.borrow() {
                        break;
                    }
                }
                Err(_) => break, // cancelled
            }
        }
    });
}

async fn run_loop(
    state: Arc<AppState>,
    interval_seconds: u64,
    mut shutdown: watch::Receiver<bool>,
) {
    // Start 90s behind the scanner (30s) and source-sync (30s) so the boot burst of
    // upstream fetches is spread out and Suwayomi has finished coming up.
    let mut next_at = Instant::now() + Duration::from_secs(90);
    tracing::info!(interval_seconds, "discovery scheduler started");
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next_at) => {
                // Race the pass against shutdown: a pass is one upstream fetch per source
                // with a throttle between them, so it can take a few seconds and must not
                // swallow a SIGTERM.
                let inner = shutdown.clone();
                let mut watcher = shutdown.clone();
                tokio::select! {
                    _ = discovery_pass(&state, &inner) => {}
                    _ = watcher.changed() => {
                        if *watcher.borrow() {
                            tracing::info!("discovery: shutdown during pass — abandoning");
                            break;
                        }
                    }
                }
                if *inner.borrow() {
                    break;
                }
                next_at = Instant::now() + Duration::from_secs(interval_seconds);
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("discovery scheduler stopping");
                    break;
                }
            }
        }
    }
}

/// One discovery pass over every subscribed English source.
async fn discovery_pass(state: &AppState, shutdown: &watch::Receiver<bool>) {
    let source_ids = match subscribed_source_ids(state).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "discovery: could not enumerate sources — skipping pass");
            return;
        }
    };
    // AN EMPTY SOURCE LIST IS A FAULT, NOT A QUIET NORMAL.
    //
    // E5 is entirely gated on this list. Every way it can come back empty — no rows in
    // `extension_subscription`, every subscription breaker-disabled, a source whose `lang`
    // tag is not the literal `en`, a `pkg_name` shape change, or Suwayomi simply not having
    // the extension installed — makes the whole phase do nothing, and the arithmetic below
    // stays perfectly correct while doing it. That is exactly the failure §8i caught in
    // Phase E: green tests over a pure function, a dead lookup underneath. So the empty case
    // must be the loudest line discovery prints, not the quietest.
    if source_ids.is_empty() {
        tracing::warn!(
            "discovery: resolved ZERO subscribed English sources — E5 polled nothing this pass \
             and is inert (see the preceding warning for which predicate came up empty)"
        );
        return;
    }
    let mut sources = 0u64;
    let mut total_flagged = 0u64;
    let mut total_triggered = 0u64;
    for source_id in &source_ids {
        if *shutdown.borrow() {
            break;
        }
        match poll_source(state, source_id).await {
            Ok((flagged, triggered)) => {
                sources += 1;
                total_flagged += flagged;
                total_triggered += triggered;
            }
            Err(e) => {
                // A single broken/challenging source must not abort the pass; E4's outage
                // detection owns source health, not discovery.
                tracing::warn!(source_id, error = %e, "discovery: source poll failed");
            }
        }
        // Gentle spacing between upstream fetches, matching the source-sync walk.
        tokio::time::sleep(Duration::from_millis(crate::sync::PAGE_DELAY_MS)).await;
    }
    // EVERY completed pass reports at `info!`, including the "nothing moved" one, and it
    // always carries `sources`. "Nothing moved" and "E5 never ran" used to be
    // indistinguishable at production log level — the quiet branch was `debug!` and the loud
    // one fired only when `flagged > 0` — so an inert phase looked exactly like a calm one.
    // The source count is the observable that separates them: it can only be non-zero if
    // `subscribed_extensions` → `list_sources` → the `en`/`pkg_name` filters all matched.
    if sources == 0 && !*shutdown.borrow() {
        // We had sources to poll and every one of them failed (each already logged above), so
        // no diff ran at all. Excluded: a pass cut short by shutdown, which reaches here with
        // 0 polled for an entirely ordinary reason and must not cry wolf on every SIGTERM.
        tracing::warn!(
            candidates = source_ids.len(),
            "discovery pass polled ZERO of its subscribed sources — no diff ran"
        );
    } else {
        tracing::info!(
            sources,
            candidates = source_ids.len(),
            flagged = total_flagged,
            triggered = total_triggered,
            "discovery pass complete"
        );
    }
}

/// Poll page 1 of one source's LATEST, diff against the snapshot, enqueue the movers, and
/// store the new snapshot. Returns `(flagged, triggered)` — flagged = series that moved up
/// or entered, triggered = of those, how many were enrolled series we actually woke.
async fn poll_source(state: &AppState, source_id: &str) -> anyhow::Result<(u64, u64)> {
    let (_has_next, mangas) = state
        .suwayomi
        .browse_source(source_id, FetchType::Latest, 1, None)
        .await?;
    if mangas.is_empty() {
        // An empty LATEST from a subscribed source is a health signal, not a diff input:
        // do not overwrite a good snapshot with nothing (that would re-baseline and flag
        // the whole page next time). Leave the snapshot as-is.
        return Ok((0, 0));
    }

    let curr: Vec<i64> = mangas.iter().map(|m| m.id).take(SNAPSHOT_WINDOW).collect();

    let prev = crate::catalog::source_latest_snapshot(&state.pool, source_id).await?;

    // First sighting of this source: establish the baseline, trigger nothing.
    let Some(prev) = prev else {
        crate::catalog::put_source_latest_snapshot(&state.pool, source_id, &curr).await?;
        tracing::debug!(
            source_id,
            window = curr.len(),
            "discovery: baselined new source"
        );
        return Ok((0, 0));
    };

    let flagged = moved_up_or_entered(&prev, &curr);

    // ORDER MATTERS, and it is trigger-then-snapshot — AT LEAST ONCE.
    //
    // Advancing the snapshot first makes the write the commit point: anything that stops us
    // between the two loses the detection outright, because the next diff compares against
    // the already-advanced order and sees nothing move. That is not a theoretical crash
    // window — `discovery_pass` is deliberately raced against shutdown in `run_loop`, so this
    // future is DROPPED mid-poll on every SIGTERM that lands inside a pass.
    //
    // Triggering first inverts the failure: an interrupted poll re-flags the same movers on
    // the next pass and re-triggers them. `trigger_due_now` is an idempotent
    // `next_scan_at = <due-now sentinel>` UPDATE, so a duplicate costs at most one targeted
    // scan — against a lost chapter waiting out the 12–24 h safety-net tier.
    let mut triggered = 0u64;
    if !flagged.is_empty() {
        let ids: Vec<String> = flagged.iter().map(|id| id.to_string()).collect();
        triggered = crate::scanner::trigger_due_now(&state.pool, &ids).await;
        tracing::debug!(
            source_id,
            flagged = flagged.len(),
            triggered,
            "discovery: source movers enqueued"
        );
    }
    // Persist the new order regardless of what we triggered, so the next diff is against
    // reality rather than re-reporting the same movers forever.
    crate::catalog::put_source_latest_snapshot(&state.pool, source_id, &curr).await?;
    Ok((flagged.len() as u64, triggered))
}

/// The Suwayomi source ids to poll: the English source of every subscribed (breaker-enabled)
/// extension. Same set E2's daily LATEST walk covers — only subscribed extensions expose a
/// trustworthy LATEST; unsubscribed sources fall back to the baseline poll.
/// # Why this function narrates its own emptiness
///
/// The predicate is deliberately IDENTICAL to `sync::sync_extension`'s (`lang == "en"` plus a
/// `pkg_name` match against the subscription table) — that one is production-proven and E5
/// must cover the same set, so this must not diverge from it. But `sync` runs per-extension
/// and reports per-extension; E5 collapses the whole catalogue into one list, and a list that
/// silently comes back empty disables the phase with no trace. So each way it can empty out
/// says which one it was, at `warn!`.
async fn subscribed_source_ids(state: &AppState) -> anyhow::Result<Vec<String>> {
    let subs = crate::catalog::subscribed_extensions(&state.pool).await?;
    if subs.is_empty() {
        tracing::warn!(
            "discovery: no enabled rows in extension_subscription — nothing to poll. Either \
             nothing is subscribed or every subscription tripped SUBSCRIPTION_FAILURE_LIMIT"
        );
        return Ok(Vec::new());
    }
    let subbed: std::collections::HashSet<&str> = subs.iter().map(|s| s.as_str()).collect();
    let sources = state.suwayomi.list_sources().await?;
    let installed = sources.len();
    // Count the two predicates separately so the warning can say WHICH one emptied the list:
    // "subscribed but not installed" and "installed but not tagged `en`" need different fixes.
    let pkg_matches = sources
        .iter()
        .filter(|s| s.pkg_name.as_deref().is_some_and(|p| subbed.contains(p)))
        .count();
    let ids: Vec<String> = sources
        .into_iter()
        .filter(|s| s.lang == "en")
        .filter(|s| s.pkg_name.as_deref().is_some_and(|p| subbed.contains(p)))
        .map(|s| s.id)
        .collect();
    if ids.is_empty() {
        tracing::warn!(
            subscribed = subs.len(),
            installed,
            pkg_matches,
            "discovery: subscribed extensions matched no English source — E5 will poll nothing. \
             pkg_matches=0 means the subscribed pkg_names are not installed in Suwayomi; \
             pkg_matches>0 means they are, but no source under them is tagged lang=\"en\""
        );
    } else {
        tracing::debug!(
            subscribed = subs.len(),
            installed,
            resolved = ids.len(),
            "discovery: resolved subscribed English sources"
        );
    }
    Ok(ids)
}

/// Pure diff: manga ids that gained new content since the previous snapshot — they
/// **entered** the window or **moved up** in rank. Both lists are LATEST order, newest
/// first. A series that merely slid *down* (displaced by something newer above it) is not
/// flagged; that is the whole reason position-diff works.
///
/// ## Why "entered" is bounded by the PREVIOUS window's depth
///
/// `browse_source` returns whatever page size the extension chooses — `fetchSourceManga`
/// has no page-size parameter (`suwayomi.rs`, and E2's own implementation note says the
/// same), so page 1 can legitimately return 20 items on one poll and 40 on the next.
/// Without a guard, the naive rule ("absent from `prev` ⇒ entered") reads a page that grew
/// from 20 to 40 as **20 simultaneous new chapters** and fires 20 targeted
/// `fetchMangaAndChapters` mutations that find nothing — the exact waste E5 exists to
/// remove, re-introduced at up to 96 ticks/day × every source.
///
/// So an absent id counts as "entered" only when it landed at a rank the previous snapshot
/// actually covered (`new_rank < prev.len()`), i.e. it genuinely displaced something we had
/// seen. Anything below that depth is unobserved tail, not news.
///
/// This costs no real detections: a new chapter puts a series at rank 0–2 in an upload-time
/// ordering, and `prev` is only ever shorter than that if the source returned fewer than
/// three entries at all. Shrinking pages are safe for the same reason — the top of the list
/// is always comparable.
fn moved_up_or_entered(prev: &[i64], curr: &[i64]) -> Vec<i64> {
    let prev_rank: HashMap<i64, usize> = prev.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    curr.iter()
        .enumerate()
        .filter(|(new_rank, id)| match prev_rank.get(*id) {
            // Entered the window — but only inside the depth `prev` observed (see above).
            None => *new_rank < prev.len(),
            Some(old_rank) => new_rank < old_rank, // moved up
        })
        .map(|(_, id)| *id)
        .collect()
}

/// A minimal in-process Suwayomi GraphQL origin, so a discovery pass can be driven END TO
/// END — real `list_sources`, real `browse_source`, real `catalog` reads/writes, real
/// `trigger_due_now` — against a migrated pool.
///
/// ## Why this exists at all
///
/// §8i caught Phase E's tier engine shipping INERT while its tests were green: every test
/// handed the engine a hand-built input, so nothing ever exercised the lookup underneath,
/// and the lookup queried a column that did not exist. E5 had the identical shape — seven
/// tests, all of them over the pure `moved_up_or_entered` diff, none of them over the wiring
/// that decides whether the diff is ever called. The wiring is the part that can silently
/// evaporate (see [`subscribed_source_ids`]), so the wiring is what needs a test.
///
/// Raw TCP rather than a mock-server crate, matching `suwayomi::testsrv` — the suite gains
/// no dependency. The narrowest stubbable seam reachable from this file is the HTTP origin
/// itself: `SuwayomiClient` is a concrete struct (not a trait), and widening it to a trait
/// would mean editing `suwayomi.rs`.
#[cfg(test)]
mod fake_suwayomi {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub struct FakeSuwayomi {
        pub base_url: String,
        /// How many `fetchSourceManga` calls have been served — proof that a pass actually
        /// reached upstream rather than short-circuiting on an empty source list.
        latest_calls: Arc<AtomicUsize>,
    }

    impl FakeSuwayomi {
        pub fn latest_calls(&self) -> usize {
            self.latest_calls.load(Ordering::SeqCst)
        }
    }

    /// One manga in Suwayomi's `MangaFields` wire shape. Every non-`#[serde(default)]`
    /// field of `SuwayomiManga` must be present or `parse_records` silently drops the
    /// record — which would leave `mangas` empty and make the test pass vacuously.
    pub fn manga(id: i64, source_id: &str) -> Value {
        json!({
            "id": id,
            "title": format!("Series {id}"),
            "url": null,
            "thumbnailUrl": null,
            "author": null,
            "artist": null,
            "description": null,
            "genre": [],
            "status": "ONGOING",
            "inLibrary": true,
            "inLibraryAt": null,
            "lastFetchedAt": null,
            "sourceId": source_id,
            "source": { "lang": "en" },
            "chapters": { "totalCount": 1 },
        })
    }

    /// One node in the `sources` wire shape.
    pub fn source(id: &str, lang: &str, pkg: &str) -> Value {
        json!({
            "id": id,
            "name": format!("Fake {id}"),
            "displayName": format!("Fake {id}"),
            "lang": lang,
            "isNsfw": false,
            "iconUrl": null,
            "extension": { "pkgName": pkg },
        })
    }

    /// Serve `sources` to the `sources` query, and the Nth entry of `latest_pages` (as an
    /// ordered LATEST page) to the Nth `fetchSourceManga` mutation. Running off the end of
    /// the script serves an empty page.
    pub async fn spawn(sources: Vec<Value>, latest_pages: Vec<Vec<i64>>) -> FakeSuwayomi {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(AtomicUsize::new(0));
        let (c, pages, srcs) = (calls.clone(), Arc::new(latest_pages), Arc::new(sources));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (c, pages, srcs) = (c.clone(), pages.clone(), srcs.clone());
                tokio::spawn(async move {
                    let req = read_request(&mut sock).await;
                    let body = if req.contains("fetchSourceManga") {
                        let n = c.fetch_add(1, Ordering::SeqCst);
                        let ids = pages.get(n).cloned().unwrap_or_default();
                        let mangas: Vec<Value> =
                            ids.iter().map(|id| manga(*id, "unused")).collect();
                        json!({ "data": { "fetchSourceManga": {
                            "hasNextPage": false, "mangas": mangas } } })
                    } else {
                        json!({ "data": { "sources": { "nodes": srcs.as_slice() } } })
                    }
                    .to_string();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        FakeSuwayomi {
            base_url: format!("http://127.0.0.1:{port}"),
            latest_calls: calls,
        }
    }

    /// Read one whole HTTP request (head + `Content-Length` body). Reading only the head
    /// would leave the GraphQL document unread, and the document is how we tell a `sources`
    /// query from a `fetchSourceManga` mutation.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let len = head
                .split("content-length:")
                .nth(1)
                .and_then(|s| s.lines().next())
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_chapter_moves_a_series_to_the_top_and_flags_only_it() {
        // C got a new chapter and jumped from rank 2 to rank 0; A and B slid down.
        let prev = vec![10, 20, 30, 40];
        let curr = vec![30, 10, 20, 40];
        assert_eq!(moved_up_or_entered(&prev, &curr), vec![30]);
    }

    #[test]
    fn an_entrant_is_flagged() {
        // 99 entered the window at the top; the tail (40) fell off. Only 99 is new.
        let prev = vec![10, 20, 30, 40];
        let curr = vec![99, 10, 20, 30];
        assert_eq!(moved_up_or_entered(&prev, &curr), vec![99]);
    }

    #[test]
    fn a_pure_downward_shift_flags_nothing() {
        // Nothing moved up: identical order.
        let prev = vec![10, 20, 30];
        let curr = vec![10, 20, 30];
        assert!(moved_up_or_entered(&prev, &curr).is_empty());
    }

    #[test]
    fn simultaneous_updates_flag_all_movers() {
        // D and E both updated; they enter above the old head, everything else shifts down.
        let prev = vec![10, 20, 30];
        let curr = vec![50, 40, 10, 20];
        assert_eq!(moved_up_or_entered(&prev, &curr), vec![50, 40]);
    }

    #[test]
    fn a_page_that_grows_does_not_flag_the_newly_visible_tail() {
        // The source returned 3 entries last poll and 6 this poll with the SAME order —
        // nothing published, the page just got longer. The naive rule flagged 40, 50 and 60
        // as "entered" and burned three targeted scans; the depth guard flags nothing.
        let prev = vec![10, 20, 30];
        let curr = vec![10, 20, 30, 40, 50, 60];
        assert!(moved_up_or_entered(&prev, &curr).is_empty());
    }

    #[test]
    fn a_page_that_shrinks_then_grows_does_not_flag_the_returning_tail() {
        // Poll 1 saw 6, poll 2 saw only 3 (short page — no flags, snapshot narrows to 3),
        // poll 3 sees 6 again. The three that "reappear" are old tail, not news.
        let full = vec![10, 20, 30, 40, 50, 60];
        let short = vec![10, 20, 30];
        assert!(moved_up_or_entered(&full, &short).is_empty());
        assert!(moved_up_or_entered(&short, &full).is_empty());
    }

    #[test]
    fn a_genuine_entrant_is_still_flagged_after_the_page_shrank() {
        // Even against a 3-entry snapshot, a real new chapter arrives at rank 0 and is
        // inside the observed depth — the guard must not cost a detection.
        let short = vec![10, 20, 30];
        let curr = vec![99, 10, 20, 30, 40, 50];
        assert_eq!(moved_up_or_entered(&short, &curr), vec![99]);
    }

    #[test]
    fn rank_one_repeat_is_the_known_false_negative() {
        // The head updates again but nothing moved above it, so it keeps rank 0 and is NOT
        // flagged. Documented behaviour — the baseline safety net catches this case.
        let prev = vec![10, 20, 30];
        let curr = vec![10, 20, 30];
        assert!(moved_up_or_entered(&prev, &curr).is_empty());
    }

    // =====================================================================================
    // WIRING TESTS
    //
    // Every test above this line feeds `moved_up_or_entered` a hand-built pair of lists.
    // That is precisely the coverage shape §8i found on Phase E's tier engine, where the
    // arithmetic was proven correct and the engine was DEAD — its lookup queried a column
    // that did not exist, `.ok()?` swallowed the error, and 450 tests stayed green.
    //
    // The tests below drive the parts a hand-built input can never reach: resolving the
    // source list, hitting upstream, reading and writing the snapshot, and enqueueing the
    // movers. The load-bearing assertion is that the resolved source list is NON-EMPTY —
    // that is the assertion the Phase E bug class walks into.
    // =====================================================================================

    use sqlx::SqlitePool;

    const SUBSCRIBED_PKG: &str = "eu.kanade.tachiyomi.extension.en.fake";
    const EN_SOURCE: &str = "9001";
    /// A next-scan far enough out that a flip to the due-now sentinel can only have come
    /// from `trigger_due_now`.
    const FUTURE: &str = "2999-01-01T00:00:00+00:00";

    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// A minimal `AppState` around a migrated pool and a fake Suwayomi origin. Discovery
    /// touches only `pool` and `suwayomi`; the rest are defaults so the struct is whole.
    fn state(pool: SqlitePool, suwayomi_base: &str) -> AppState {
        AppState {
            pool: pool.clone(),
            cover_pool: pool,
            suwayomi: crate::suwayomi::SuwayomiClient::new(suwayomi_base.to_string(), None, None),
            mangadex: Arc::new(crate::mangadex::MangaDexClient::new("test-ua", 5.0, 40.0)),
            admin_users: vec![],
            scan_health: std::sync::Mutex::new(crate::graphql::ScanHealth::default()),
            auth_limiter: crate::graphql::RateLimiter::new(100, 60),
            federated_limiter: crate::graphql::RateLimiter::new(100, 60),
            session_ttl_secs: 3600,
            series_inflight: crate::graphql::KeyedLocks::default(),
            chapters_inflight: crate::graphql::KeyedLocks::default(),
            cover_crawl_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
        }
    }

    async fn subscribe(pool: &SqlitePool, pkg: &str) {
        sqlx::query("INSERT INTO extension_subscription (pkg_name, created_at) VALUES (?, ?)")
            .bind(pkg)
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(pool)
            .await
            .unwrap();
    }

    async fn enrol(pool: &SqlitePool, id: i64) {
        sqlx::query(
            "INSERT INTO series_scan_state (series_id, next_scan_at, updated_at) \
             VALUES (?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(FUTURE)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn next_scan_at(pool: &SqlitePool, id: i64) -> String {
        sqlx::query_scalar("SELECT next_scan_at FROM series_scan_state WHERE series_id = ?")
            .bind(id.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The source list the fake engine reports: one subscribed English source (the only one
    /// E5 may poll), plus the two decoys each filter exists to reject.
    fn source_nodes() -> Vec<serde_json::Value> {
        vec![
            fake_suwayomi::source(EN_SOURCE, "en", SUBSCRIBED_PKG),
            // Same subscribed extension, different language — `all.mangadex` ships ~70 of
            // these under one pkg. The `lang == "en"` filter must drop it.
            fake_suwayomi::source("9002", "ja", SUBSCRIBED_PKG),
            // English, but nobody subscribed to it. The pkg filter must drop it.
            fake_suwayomi::source("9003", "en", "com.other.unsubscribed"),
        ]
    }

    /// THE test this module was missing: a real pass, upstream and all.
    ///
    /// Poll 1 baselines. Poll 2 shows series 30 jumping from rank 2 to rank 0 — a new
    /// chapter — while 10 and 20 merely slide down. Only 30 may be woken.
    #[tokio::test]
    async fn a_real_discovery_pass_resolves_sources_diffs_and_wakes_only_the_mover() {
        let pool = migrated_pool().await;
        subscribe(&pool, SUBSCRIBED_PKG).await;
        for id in [10, 20, 30] {
            enrol(&pool, id).await;
        }
        let engine =
            fake_suwayomi::spawn(source_nodes(), vec![vec![10, 20, 30], vec![30, 10, 20]]).await;
        let st = state(pool.clone(), &engine.base_url);
        let (_tx, shutdown) = watch::channel(false);

        // ---- THE ASSERTION THAT CATCHES AN INERT E5 -------------------------------------
        // If `subscribed_extensions` returns nothing, or the `lang`/`pkg_name` predicates
        // stop matching (a `en-us` tag, a pkg-name shape change, an empty subscription
        // table), this list is empty and every line below still passes vacuously — the whole
        // phase does nothing while the suite stays green. Nothing asserted this before.
        let resolved = subscribed_source_ids(&st).await.unwrap();
        assert!(
            !resolved.is_empty(),
            "discovery resolved ZERO sources from a subscribed English extension — E5 is inert"
        );
        assert_eq!(
            resolved,
            vec![EN_SOURCE.to_string()],
            "exactly the subscribed English source: the ja sibling and the unsubscribed \
             English source must both be filtered out"
        );

        // ---- Poll 1: baseline, trigger nothing ------------------------------------------
        discovery_pass(&st, &shutdown).await;
        assert_eq!(
            engine.latest_calls(),
            1,
            "the pass must actually fetch LATEST — one call, for the one resolved source"
        );
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, EN_SOURCE)
                .await
                .unwrap(),
            Some(vec![10, 20, 30]),
            "poll 1 must persist the order it saw, or poll 2 has nothing to diff against"
        );
        for id in [10, 20, 30] {
            assert_eq!(
                next_scan_at(&pool, id).await,
                FUTURE,
                "the first sighting of a source baselines and triggers nothing (series {id})"
            );
        }

        // ---- Poll 2: 30 moved up; only 30 is woken --------------------------------------
        discovery_pass(&st, &shutdown).await;
        assert_eq!(engine.latest_calls(), 2);
        assert_eq!(
            next_scan_at(&pool, 30).await,
            crate::scanner::DUE_NOW_SENTINEL,
            "series 30 jumped rank 2 → 0, which only a new chapter does: it must be due now"
        );
        for id in [10, 20] {
            assert_eq!(
                next_scan_at(&pool, id).await,
                FUTURE,
                "series {id} only slid DOWN — waking it is exactly the wasted scan E5 removes"
            );
        }
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, EN_SOURCE)
                .await
                .unwrap(),
            Some(vec![30, 10, 20]),
            "the snapshot must advance, or the same movers re-trigger forever"
        );
    }

    /// The two ways the source list silently empties out. Each of these is a live E5
    /// outage; each used to produce NOT ONE line of production log.
    #[tokio::test]
    async fn an_unmatched_subscription_resolves_no_sources() {
        // (a) Nothing subscribed at all — the engine is never even asked.
        let pool = migrated_pool().await;
        let engine = fake_suwayomi::spawn(source_nodes(), vec![]).await;
        let st = state(pool.clone(), &engine.base_url);
        assert!(subscribed_source_ids(&st).await.unwrap().is_empty());

        // A pass over an empty list must not reach upstream, and must not overwrite or
        // invent any snapshot.
        let (_tx, shutdown) = watch::channel(false);
        discovery_pass(&st, &shutdown).await;
        assert_eq!(
            engine.latest_calls(),
            0,
            "an empty source list must not reach upstream at all"
        );

        // (b) Subscribed to something Suwayomi does not expose under any English source.
        // This is the shape a pkg-name change or a stale subscription takes, and it is
        // indistinguishable from health without the warning this returns.
        let pool = migrated_pool().await;
        subscribe(&pool, "com.nonexistent.extension").await;
        let st = state(pool, &engine.base_url);
        assert!(
            subscribed_source_ids(&st).await.unwrap().is_empty(),
            "a subscription with no matching installed source resolves nothing"
        );
    }

    /// A language tag that is not the literal `en` disables the phase entirely. Pinned
    /// because the predicate is a bare string equality shared with `sync::sync_extension`
    /// (production-proven, and deliberately NOT diverged from here) — so the failure mode
    /// is real, and the fix is at the source's tag, not in this filter.
    #[tokio::test]
    async fn a_lang_tag_that_is_not_exactly_en_makes_the_phase_inert() {
        let pool = migrated_pool().await;
        subscribe(&pool, SUBSCRIBED_PKG).await;
        let engine = fake_suwayomi::spawn(
            vec![fake_suwayomi::source(EN_SOURCE, "en-us", SUBSCRIBED_PKG)],
            vec![vec![10, 20, 30]],
        )
        .await;
        let st = state(pool, &engine.base_url);
        assert!(
            subscribed_source_ids(&st).await.unwrap().is_empty(),
            "`en-us` is not `en` — documenting the sharp edge, not endorsing it"
        );
    }

    /// `catalog::source_latest_snapshot` / `put_source_latest_snapshot` had NO tests. They
    /// are E5's entire memory: if the round-trip breaks, every pass re-baselines, nothing
    /// is ever flagged, and the phase is silently inert forever. (They live in
    /// `catalog/mod.rs`, which this agent does not own — so the coverage is written from
    /// this side, against a real migrated pool.)
    #[tokio::test]
    async fn the_snapshot_round_trips_and_is_overwritten_in_place() {
        let pool = migrated_pool().await;
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, "src-1")
                .await
                .unwrap(),
            None,
            "an unseen source has no snapshot — this is what makes poll 1 a baseline"
        );

        let ids = vec![7, -3, 0, i64::MAX, i64::MIN];
        crate::catalog::put_source_latest_snapshot(&pool, "src-1", &ids)
            .await
            .unwrap();
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, "src-1")
                .await
                .unwrap(),
            Some(ids),
            "ORDER and full i64 range must survive the JSON round-trip: the order IS the signal"
        );

        // The upsert must replace, not accumulate: a second row per source would make the
        // read non-deterministic.
        crate::catalog::put_source_latest_snapshot(&pool, "src-1", &[1, 2])
            .await
            .unwrap();
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, "src-1")
                .await
                .unwrap(),
            Some(vec![1, 2])
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_latest_snapshot")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            rows, 1,
            "ON CONFLICT must update in place, not insert a twin"
        );

        // An empty snapshot is a legitimate stored value and must NOT read back as "no
        // snapshot" — `None` means "never seen", which re-baselines.
        crate::catalog::put_source_latest_snapshot(&pool, "src-2", &[])
            .await
            .unwrap();
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, "src-2")
                .await
                .unwrap(),
            Some(vec![])
        );
    }

    /// The documented degradation: `source_latest_snapshot` ends in `.ok()`, so a malformed
    /// `ordered_ids` reads back as `None` rather than erroring.
    ///
    /// THIS IS THE §8i FAILURE MODE, and it is worth naming precisely: `None` is
    /// indistinguishable from "never seen", so a source whose row is corrupt re-baselines on
    /// EVERY pass and can never flag anything — permanently, silently, with the pass still
    /// reporting success. The behaviour is deliberate (a bad row must not abort the pass) and
    /// is pinned here; what it costs is a real gap, reported upward rather than patched in a
    /// file this agent does not own.
    #[tokio::test]
    async fn a_malformed_snapshot_degrades_to_no_snapshot() {
        let pool = migrated_pool().await;
        for junk in ["not json", "{}", "[1, \"two\"]", ""] {
            sqlx::query(
                "INSERT INTO source_latest_snapshot (source_id, ordered_ids, captured_at) \
                 VALUES ('junk', ?, '2026-01-01T00:00:00+00:00') \
                 ON CONFLICT(source_id) DO UPDATE SET ordered_ids = excluded.ordered_ids",
            )
            .bind(junk)
            .execute(&pool)
            .await
            .unwrap();
            assert_eq!(
                crate::catalog::source_latest_snapshot(&pool, "junk")
                    .await
                    .unwrap(),
                None,
                "malformed {junk:?} must degrade to no snapshot, not abort the pass"
            );
        }
    }

    /// An empty LATEST page must NOT clobber a good snapshot. Without this, a source having
    /// a bad minute re-baselines to nothing and then flags its entire page on the next poll
    /// — a burst of targeted scans that find nothing, i.e. the exact waste E5 exists to
    /// remove. Driven through the real fetch path, not the pure diff.
    #[tokio::test]
    async fn an_empty_latest_page_leaves_the_snapshot_alone() {
        let pool = migrated_pool().await;
        subscribe(&pool, SUBSCRIBED_PKG).await;
        for id in [10, 20, 30] {
            enrol(&pool, id).await;
        }
        // Poll 1 sees a real page; poll 2 sees nothing; poll 3 sees the same page again.
        let engine = fake_suwayomi::spawn(
            source_nodes(),
            vec![vec![10, 20, 30], vec![], vec![10, 20, 30]],
        )
        .await;
        let st = state(pool.clone(), &engine.base_url);
        let (_tx, shutdown) = watch::channel(false);

        discovery_pass(&st, &shutdown).await;
        discovery_pass(&st, &shutdown).await;
        assert_eq!(
            crate::catalog::source_latest_snapshot(&pool, EN_SOURCE)
                .await
                .unwrap(),
            Some(vec![10, 20, 30]),
            "an empty page is a health signal, not a diff input — the snapshot must survive"
        );

        discovery_pass(&st, &shutdown).await;
        for id in [10, 20, 30] {
            assert_eq!(
                next_scan_at(&pool, id).await,
                FUTURE,
                "series {id} never moved; the empty poll must not have re-baselined it into \
                 looking new"
            );
        }
    }
}
