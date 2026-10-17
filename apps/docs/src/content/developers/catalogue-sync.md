---
title: Catalogue sync
description: Understand how the server inside the admin app fills its catalogue, mirrors MangaDex and schedules source scans.
---

Komiq is self-hostable: the admin app runs the server and gives you its management portal, and the reader app connects to a server. The server maintains its catalogue from two data streams: a direct MangaDex mirror and sources tracked through [Suwayomi, the source engine](/developers/suwayomi) extensions. This page explains how its background workers ingest titles, poll for chapter releases and adapt scan schedules.

## Overview

The catalogue architecture separates metadata storage from image delivery. The server stores series records, chapter lists and author credits in PostgreSQL. When you read a chapter, page images stream live through the image proxy rather than taking up disk space on the server.

```flow
MangaDex API -> Catalogue sync: fetches metadata and chapter feed
Suwayomi source -> Discovery loop: checks top ranks on latest listings
Scanner -> Suwayomi source: fetches chapter lists for due series
Catalogue sync -> Database: writes canonical works and chapters
```

Two independent sync subsystems keep this data fresh:

1. The MangaDex mirror syncs the global MangaDex catalogue and ingests a live chapter firehose.
2. The source scanner monitors installed Suwayomi extensions for new titles and new chapters.

## MangaDex mirror

The MangaDex mirror communicates directly with `https://api.mangadex.org` using an HTTP client in `apps/server/src/mangadex.rs`. It does not route requests through Suwayomi. Connecting directly provides access to creation timestamps, external tracker identifiers, and the full list of localized title aliases.

### Rate limiting

MangaDex limits API traffic by IP address. Because multiple server processes or reader instances can share an outbound IP address, the server regulates requests with token buckets:

- The main API bucket allows 4 requests per second by default (`MANGADEX_RATE_PER_SEC`). This stays safely below the upstream ceiling of 5 requests per second.
- The `/at-home` endpoint uses a separate bucket capped at 40 requests per minute (`MANGADEX_ATHOME_PER_MIN`).

All outgoing requests send a configurable User-Agent header defined by `MANGADEX_USER_AGENT`.

### The usage switch and crawl mode

MangaDex access is governed by two independent controls: the usage switch and the crawl mode.

The usage switch (`mangadex_enabled`) decides whether the server may contact MangaDex for reading, cover images, metadata enrichment, or proxying. The server evaluates this switch in order:

1. The `KOMIKA_MANGADEX` environment variable. If set to `on` or `off`, it locks the switch.
2. The `CATALOGUE_SYNC` environment variable. If set to `on`, it forces MangaDex usage on.
3. The stored setting in the `instance_setting` table, toggled from the admin console under [Settings and maintenance](/admin/settings).
4. The built-in default: `off` when `KOMIKA_WEB_DIR` is set to a non-empty value, and `on` otherwise. The Docker image and desktop admin app set this variable to serve the web apps.

When the usage switch is off, all MangaDex API calls fail immediately inside the client without opening a network connection.

The crawl mode decides whether the server runs the background sweep tasks. The server evaluates the crawl mode as follows:

- If `CATALOGUE_SYNC` is `on`, the crawl loop is spawned and runs while the usage switch is on.
- If `CATALOGUE_SYNC` is unset and `KOMIKA_WEB_DIR` has a non-empty value, the crawl loop follows the usage switch.
- If `CATALOGUE_SYNC` is `off`, the crawl loop never runs. It also never runs when `CATALOGUE_SYNC` is unset and `KOMIKA_WEB_DIR` is unset or empty.

This separation allows a fleet of servers to read from MangaDex while only one primary node executes the catalogue crawl.

### Pagination and windowing

MangaDex limits pagination offsets to 10,000 items. To crawl the entire catalogue without hitting this limit, `sync_catalogue` in `apps/server/src/mangadex.rs` slides a timestamp window:

1. The initial seed requests pages of 100 series ordered by creation date (`createdAt`).
2. When the offset reaches 9,900 (`WINDOW_OFFSET_CAP`), the crawler resets the offset to 0 and updates the `createdAtSince` query parameter to the timestamp of the last seen item.
3. The crawler records this progress after every page in `set_seed_progress`. If the server restarts, it resumes from the latest checkpoint.
4. Once the initial seed finishes, recurring sync cycles switch to `updatedAtSince` windows to process recently edited entries.

## The sync scheduler

The recurring sync supervisor in `apps/server/src/mangadex.rs` runs three jobs on separate schedules within one background loop:

| Task             | Default interval | Environment variable           | Purpose                                             |
| ---------------- | ---------------- | ------------------------------ | --------------------------------------------------- |
| Catalogue sweep  | 6 hours          | `CATALOGUE_SYNC_INTERVAL_SECS` | Ingests new and updated series metadata.            |
| Chapter firehose | 15 minutes       | `CHAPTER_SYNC_INTERVAL_SECS`   | Ingests recently published English chapters.        |
| Feed reconciler  | 24 hours         | `FEED_RECONCILE_INTERVAL_SECS` | Verifies updates feed integrity against the ledger. |

To avoid a resource burst during startup, the tasks delay their first runs. The feed reconciler starts at 20 seconds, catalogue sync at 60 seconds and chapter sync at 150 seconds. The reconciler's first run is also the start-up build of the updates feed, Browse and search tables. A start builds them once, plus one more time 5 minutes later if that build failed or the release ledger was still being filled.

The chapter firehose requests chapters in English (`translatedLanguage[]=en`). When a chapter lands, the incremental feed writer updates `feed_series_updates` at the end of each page.

## Source scanning through Suwayomi

Komiq's Suwayomi integration does not use a global chapter firehose. It discovers new titles and chapter releases through source synchronization, latest-listing discovery, and adaptive scanning.

### Source synchronization

The source sync supervisor in `apps/server/src/sync.rs` runs every 24 hours by default (`SOURCE_SYNC_INTERVAL_SECONDS`). It reconciles enrolled series, then discovers titles through extensions enabled in `extension_subscription`:

- **Reconcile**: It ensures every enrolled series has a tracking row in `series_scan_state`, and verifies that Suwayomi marks the series as part of its library.
- **Discover**: It walks the latest updates list (`FetchType::Latest`) for each subscribed English source. It ingests newly found series sequentially with [One entry per series](/developers/canonical-works) deduplication.

The walk pauses 750 milliseconds between browse pages (`PAGE_DELAY_MS`). The crawler stops after 2 consecutive fully known pages (`STOP_AFTER_KNOWN_PAGES`) once it has seen at least 30 distinct series (`LATEST_TRIGGER_WINDOW`), when the source has no next page, or at a maximum of 10 pages by default (`SOURCE_SYNC_MAX_PAGES`).

### LATEST-diff discovery

To detect new chapters without waiting for the daily sync, the discovery supervisor in `apps/server/src/discovery.rs` polls subscribed sources every 15 minutes (`DISCOVERY_INTERVAL_SECS`).

Because sources sort their latest listings by release time, an uploaded chapter pushes a series to the top of the list. The discovery loop takes page 1 of each source and diffs the first 40 entries (`SNAPSHOT_WINDOW`) against the previous snapshot:

- If a series entered the window or moved up in rank, it is flagged for a chapter check.
- The supervisor calls `scanner::trigger_due_now` for flagged series; only existing scan-state rows are made due.
- The adaptive scanner checks due series on its next tick.

Each poll uses one browse request per source. The first poll stores a baseline and triggers nothing. A series that updates without changing rank relies on its scheduled scan.

## The adaptive scanner

The background scanner in `apps/server/src/scanner.rs` runs a loop per source, ticking every 60 seconds (`SOURCE_TICK_SECONDS`). `SCAN_TICK_SECONDS` is a legacy setting and does not control these loops. Each loop queries `series_scan_state` for its source's rows where `next_scan_at` is less than or equal to the current time. It does not scan the whole library on each run.

### Concurrency and rate caps

Outbound requests from the scanner are bounded at three levels:

- Global concurrency: `SCAN_GLOBAL_CONCURRENCY` limits simultaneous scans across all sources (default 12).
- Per-source concurrency: each source loop runs at most 3 simultaneous scans (`SCAN_CONCURRENCY`).
- Per-source request caps: the optional `SCAN_SOURCE_RATE_LIMITS` environment variable sets a rolling rate limit for a specific source ID. For example, `source_id=10/300` allows 10 requests every 5 minutes. An administrator can add the same kind of limit at runtime with `setSourceRateLimit`, stored in `source_rate_limit`. The supervisor reads that table on each reconcile and re-paces the running loop without a restart. For a source that both name, the stored row wins. Removing the row restores the environment's limit. The limit is one per-process budget (`source_budget.rs`) shared by every background consumer of that source: scans, discovery, source sync, source ingest, the cover crawl and warmer, and media ingest. A scan or ingest item costs `requests_per_scan`; a listing page, sync request, cover or archived page costs 1. Media ingest never waits: it skips the page when the budget has no room.

### Scan intervals

Active series follow scheduling tiers based on comic format and release recency:

- **Active fast-release formats**: manhwa, manhua, and webtoons with a chapter released within the last 14 days (`DORMANT_AFTER_DAYS`) are checked every 3 hours (`TIER_FAST_ACTIVE_HOURS`).
- **Dormant fast-release formats**: manhwa, manhua, and webtoons with no chapter in 14 days are checked every 24 hours (`TIER_FAST_DORMANT_HOURS`).
- **Manga and western comics**: series follow a 12-hour cadence (`TIER_MANGA_HOURS`).
- **Paused series**: series marked as completed, cancelled, or on hiatus are parked for 14 days (`PAUSED_PARK_HOURS`). Source sync can wake parked series that appear in its latest-listing walk. Discovery can wake them when they enter its snapshot window or move up in rank.

Explicit interval overrides are clamped between 1 hour (`HARD_MIN_INTERVAL_HOURS`) and 14 days (`MAX_INTERVAL_HOURS`).

Steady scan intervals include random jitter of plus or minus 10 percent (`SCHEDULE_JITTER_FRACTION`), tightened to 5 percent for intervals of 3 hours or less (`tier_jitter_fraction`). Paused parks also include random spread. This jitter prevents requests from bunching together on the hour.

## Where to look

- `apps/server/src/mangadex.rs`: MangaDex API client, token buckets, catalogue seed crawler, and recurring sync tasks.
- `apps/server/src/scanner.rs`: adaptive scan loop, format tiers, concurrency semaphores, and chapter detection.
- `apps/server/src/discovery.rs`: 15-minute latest listing rank diffing and fast-path scan triggers.
- `apps/server/src/sync.rs`: daily source sync, library reconciliation, and latest listing pagination.
- `apps/server/src/instance.rs`: resolution logic for MangaDex usage and crawl mode.
- `apps/server/src/config.rs`: environment variable parsing and default interval constants.
