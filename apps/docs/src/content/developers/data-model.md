---
title: Data model
description: Understand the database schema, table relationships and migrations for the server inside your admin app.
---

Komiq is self-hostable: the admin app runs the server and gives you its management portal, and the reader app connects to a server. That server stores user accounts, authentication sessions, reading progress, community discussions and its canonical comic catalogue in PostgreSQL 18. This page explains storage and migrations.

## Main database and cover store

The server separates durable application records from refetchable images:

1. **Main database**: `DATABASE_URL` is a PostgreSQL connection URL. The standalone default is `postgres://localhost/komika`; Docker and desktop supervisors supply their own endpoint and application role. It holds accounts, sessions, reading history, social data, the canonical catalogue, the cover index and uploaded covers.
2. **Cover store**: `KOMIKA_COVERS_DIR` holds cover images and extension icons as files. It defaults to `covers` beside the covers SQLite file: `/data/covers` in Docker and `server/covers` in the desktop app's data folder. `COVERS_DATABASE_URL` still places the data-directory lock, the default covers directory and the old cache read by the one-time copy. Its standalone default is `sqlite://covers.sqlite3`; Docker uses `sqlite:///data/covers.sqlite3`.

Main backups use PostgreSQL tools. The global deployment packages pgBackRest, a backup manager that archives database files and transaction logs to object storage. Self-host Docker uses `pg_dump`, a consistent logical export. The desktop app saves a local dump when the newest is at least 20 hours old and keeps seven. These include the cover index and uploaded covers. Cover files are not backed up. After a database restore, missing files refill from sources on request, or through the background cover crawl and warmer after garbage collection notices them. Uploads return from PostgreSQL.

Files are addressed by their SHA-256 content hash at `objects/<2 hex>/<sha256>` and written once. `tmp/` holds files being written. Only the owner server writes covers under its data-directory lock. Garbage collection removes files older than 24 hours with no entry in the index.

### Connections and native types

`sqlx` manages asynchronous PostgreSQL pools through `apps/server/src/database.rs`. Reader sessions are query-only and use UTC. Inspect the pool settings in code before tuning the PostgreSQL connection limit.

The main schema uses native PostgreSQL values: UUID account identities, timezone-aware timestamps, Boolean flags and JSON values where the application treats them as structured data. Prefixed work IDs and other non-UUID identifiers stay text. API timestamps use UTC with six fractional digits and `+00:00`.

SQLite cover connections and recovery are used only while copying the old cache. PostgreSQL planner maintenance uses `ANALYZE`.

### Old installations

The server cannot read or convert an old `komika.sqlite3` main database. It refuses to start while that file is in the data directory and PostgreSQL is empty. If PostgreSQL already holds data, it warns that the old file is ignored and starts. Read [Old SQLite install](/self-host/update-and-backup#old-sqlite-install) to start fresh.

## Core tables and relationships

Foreign keys connect sources, works, chapters and users:

```flow
source_series -> work: maps upstream source to canonical work
chapter -> source_series: links chapter records to source series
work_alias -> work: indexes alternate titles for search and matching
work_external_id -> work: tracks external IDs like AniList or MangaDex
work_field_pin -> work: preserves admin metadata overrides via triggers
user_library -> users: records user shelf status and favorites
canonical_progress -> users: tracks chapter read progress for works
reviews -> users: links user ratings and reviews to works
```

### Canonical catalog tables

Catalogue works are independent of source copies:

- `work`: the central table for a logical series. It uses a prefixed primary key (`w_...`). It holds titles, descriptions, credits, classification fields and a perceptual cover hash.
- `source_series`: represents a concrete series on an upstream source such as MangaDex or Suwayomi. It references `work(id)` with cascade deletion. It enforces uniqueness on `(source_type, source_id, source_key)`.
- `source_extension`: one row per Suwayomi source, keyed by source id. It holds the extension package name, the repository URL and the source's NSFW flag. The server writes it when an extension is installed or updated and on the scheduled scan. `chapterOrigin` and ingestion read it.
- `source_nsfw_pin`: one row per source that an administrator marked adult with `markSourceNsfw`. Ingestion reads it, so a series added later from that source is stored adult.
- `source_rate_limit`: one row per source that an administrator limited with `setSourceRateLimit`. It holds the allowed requests and the window in seconds.
- `scanlator_tag`: one row per scanlator name that an administrator tagged with `setScanlatorAi`. It holds the case-folded name and the level, `ai` or `partial`. The tag applies on read to every chapter credited to that name. A joint credit takes the strongest tag of its members.
- `source_ai_tag`: one row per Suwayomi source that an administrator tagged with `setSourceAi`, for a site that translates its own series and credits no group. It holds the source id and the level. A chapter copy takes the stronger of its source's tag and its credit's tag.
- `content_block_rule`: one row per content block rule. `tags` and `ratings` hold sorted, comma-joined tag ids and rating words. A rule matches a work that has all of its tags and, when it names ratings, one of them. A `NULL` `user_id` marks a site-wide rule. Any other value is that user's own rule, deleted with the user.
- `chapter`: mirrors chapter metadata for each source series. It references `source_series(id)` with cascade deletion and enforces uniqueness on `(source_series_id, external_id)`. Columns include `number`, `volume`, `title`, `scanlator`, `published_at`, `readable_at`, `external_url` and a normalized `chapter_key`.
- `work_alias`: stores alternate titles and romanized names. It references `work(id)` and provides a normalized title index for fuzzy matching and search.
- `work_external_id`: maps canonical works to external database identifiers from services like AniList, MyAnimeList, MangaUpdates, Kitsu and MangaDex. The primary key is `(provider, external_id)`.
- `merge_candidate`: records potential duplicate matches between a `source_series` and a candidate `work(id)`. It stores a confidence score and a detection method, allowing administrators to review and resolve candidates.
- `work_field_pin`: records administrator overrides for source-managed fields like author, artist, year and status. Database triggers automatically restore pinned values if an automated synchronization pass updates the underlying `work` row.
- `engine_generation`, `engine_manga_key` and `engine_chapter_key`: the source engine's own manga and chapter keys and web links, added by migration `0005_engine_keys.sql`. Keys are stored per generation, and each generation row has a state of `active`, `quarantined` or `detached`; the server adopts or creates one `active` generation at start. `chapterOrigin` reads these rows.
- `engine_numeric_state` and `suwayomi_progress_detached`, added by migration `0006_engine_fence.sql`: the numeric epoch that fences queued progress writes after an engine change, and the extension-source reading progress set aside when an administrator starts a new engine generation.
- `engine_detached_row`, added by migration `0008_engine_detached_row.sql`: numeric library, review, view-count and per-series scan-setting rows that a new engine generation set aside. Rows whose series had a work moved to that work instead. Each row keeps its old generation, its source table and the original row as JSON.
- `media_policy` and `media_takedown`, added by migration `0009_media_policy.sql`: the policy epoch, which moves with every site-wide block rule or takedown change, and the operator takedowns of works and chapter copies. Read [Takedowns](/admin/takedowns).
- `progress_untick`, added by migration `0010_progress_untick.sql`: one row per explicit untick of one chapter copy (user id, chapter id, time). A new engine generation deletes the records of numeric chapter ids.
- `work_chapter`, `work_chapter_link` and `work_chapter_state`, added by migration `0012_work_chapter.sql`: the work's own chapter list. `work_chapter` holds one entry per chapter of a work (id `wc_...`): its `match_key`, its place in the list (`sort_key`, a decimal), its number and label, a `special` flag and the administrator's `hidden` flag and `title_override`. `work_chapter_link` links each `chapter` row (a source copy) to one entry. `work_chapter_state` holds `ready` (every copy of the work is linked) and `list_rev`, a counter that grows with every change to the list. Read [Canonical works](/developers/canonical-works) for how copies are matched.

### Metadata trust tables

Migration `0011_source_metadata.sql` adds these tables and one column:

- `source_metadata_bar`: one row per `(source_id, field_group)`, with `created_at`. A row bars a source from title, description, credits, tags, cover or status. No row means allowed.
- `work_metadata_contribution`: one value per mapping and group. Its primary key is `(source_series_id, field_group, slot, lang, value_key)`. The stored generated column `value_key` is `md5(value)`. It also stores `ord`, optional JSON `detail`, `attribution` and `observed_at`.

- `work.cover_source`: the source that supplied the published cover. Empty for uploads and for covers published before the column existed.
- `work_cover_revocation`: works whose source cover was removed by a cover bar. The old cover cache is never used for them again.

Contributions reference `source_series(id)` with cascade deletion. They have no `work_id`; the mapping supplies it. Rows follow mappings through merges and splits. An empty slot and value mark an observed empty group. Missing rows mean no observation. Attribution is `observed`, `inferred` or `unknown`.

Read [Canonical works](/developers/canonical-works) for ranking and projection rules.

### Identity and social tables

- `users`: user accounts with `id`, unique `username`, unique `email`, `password_hash` (Argon2id), `is_admin`, `is_banned`, `show_nsfw`, display profile fields and `must_change_password`.
- `sessions`: active authentication sessions. The primary key `token` stores a lowercase hexadecimal SHA-256 hash of the bearer token rather than plaintext. It references `users(id)` with cascade deletion and stores a timezone-aware `expires_at` timestamp.
- `reviews`: user ratings and written reviews for works. Scores range from 1 to 10. A unique constraint on `(series_id, user_id)` ensures each user has at most one review per work.
- `comments`: discussion comments attached to chapters or series. Comments support markdown formatting and spoiler tags.
- `comment_media`: image attachments uploaded for comments. Binary image data is stored as WebP blobs.
- `comment_votes`: user likes and dislikes on comments, keyed by `(comment_id, user_id)`.
- `notifications`: in-app notifications for comment replies and comment like milestones.

### Library and reading progress tables

Reading history and personal collections are stored per user:

- `user_library`: links a user to a series (`user_id`, `series_id`). It stores shelf status and an `is_favorite` flag.
- `canonical_progress`: records reading progress for canonical `w_...` works. Keyed by `(user_id, chapter_id)`, it stores `last_page_read`, a `read` boolean flag and an `updated_at` timestamp.
- `suwayomi_progress`: records reading progress for numeric Suwayomi-sourced series. It mirrors the structure of `canonical_progress` for non-canonical sources.
- `work_progress` (migration 0013): one row per reader and work. `through_entry_id` is the chapter-list entry the reader has read through; `revision` goes up by one on every saved change.
- `work_chapter_mark` (migration 0013): the reader's exceptions to that pointer, one row per reader and entry. `read = false` is an untick; `read = true` is an explicit read the pointer does not cover, such as a special chapter or a chapter above the pointer.
- Read rule on a ready work: a mark wins; else the entry is read when a linked copy has a read row, or when it is not special and sorts at or before the pointer's entry. `canonical_progress` and `suwayomi_progress` still hold page positions and explicit reads. Before a copy disappears (vanished at its source, a deleted source, a new engine generation), an explicit read the pointer does not cover becomes a mark.
- `progress_untick`: the catch-up record (see `setProgress` in the GraphQL page). It holds one row per explicit untick of one chapter copy: the user id, the chapter id and the time. The server skips a chapter that has such a record when it marks earlier chapters read.
- Catch-up (works whose chapter list is not ready only): finishing a chapter (`setProgress` with `read: true`) marks the lower-numbered chapters of the same source read. For a MangaDex chapter that means all English MangaDex chapters of the work. For a Suwayomi chapter it means the cached chapters of that one manga. The server writes only missing rows and rows that are not read yet. Rows already read are not touched, so they take no lock and cause no write. It skips chapters with a `progress_untick` record, and every copy with the same chapter key in the same work. Reading or ticking a copy deletes the record for that chapter key. When the drain makes a work ready, it turns the work's untick records into `read = false` marks. A new engine generation does the same for the numeric untick records of works that are not ready yet, before it removes them.
- `series_views` and `series_view_bucket`: tracks chapter read counts aggregated across rolling hourly time buckets to calculate trending feeds.

### Materialized query tables

Precomputed tables support browse and search:

- `browse_catalogue`: a wide table containing every browsable canonical work. It stores filter and ranking fields so browse queries avoid repeated joins.
- `feed_series_updates`: a materialized feed table storing recent chapter releases ordered by upstream publication dates.
- `work_search_alias`: normalized titles and aliases with PostgreSQL full-text vectors and `pg_trgm` indexes for typo candidates.
- `instance_setting`: a key-value store holding runtime configuration changed from the admin console, such as registration toggles and instance names.

## Cover index and uploads

PostgreSQL stores two cover tables:

- `cover_object`: one index row per `(asset_key, kind)`. The asset identifies a work, Suwayomi manga or extension icon. Kind is `master`, `w192` or `w320`. Rows hold `sha256`, `byte_length`, `version` and, for a resized derivative, `master_sha256`, the master it came from. The index and the work's cover version change in one transaction.
- `cover_upload`: the processed master bytes of an admin-uploaded cover, keyed by `work_id`, with its hash and creation time. These bytes are included in database backups and dumps so uploads survive loss of the cover files.

## Legacy cover copy source

The old `covers.sqlite3` cache is a copy source, not the active store. Its tables hold:

- `work_cover_blob`: canonical work cover bytes, version and timestamp, keyed by `work_id`.
- `suwayomi_cover_blob`: Suwayomi cover bytes, keyed by numeric manga ID.
- `cover_variant`: resized covers keyed by `(asset_key, width)`.
- `cover_maintenance`: old maintenance progress cursors.
- `extension_icon_blob`: PNG icon bytes keyed by package name.

The server copies these assets into files once, in the background, and resumes after a restart. It serves covers not yet reached from SQLite. Allow free disk space for about twice the cache size while both exist. After `cover copy finished` appears in the server log, the server no longer opens the old file. It stays on disk for the owner to delete. Covers uploaded before this update copy like any cover, but are not in `cover_upload` or database backups: the old cache did not identify uploads.

## How migrations work

The main migration history lives in `apps/server/migrations_pg/`. It begins with the PostgreSQL baseline. The files in `apps/server/migrations/` are frozen SQLite history that only historical tests use; never apply them to PostgreSQL.

### Execution at startup

`apps/server/src/db.rs` applies main migrations with SQLx's PostgreSQL migrator. `_sqlx_migrations` records versions and checksums, so changing an applied historical migration causes a startup error. Add a new migration for a new schema change.

The legacy covers schema is initialized and checked only while the cover copy needs it.

### Migration changes

Keep applied migrations immutable. Add new files for schema changes and backfills. Test the resulting constraints and queries on PostgreSQL.

## Where to look

- `apps/server/migrations_pg/`: PostgreSQL baseline and migration history.
- `apps/server/migrations/`: frozen SQLite migration history for historical tests.
- `apps/server/src/media_policy.rs`: the policy epoch, takedowns and the in-memory denied set.
- `apps/server/src/db.rs`: PostgreSQL initialization, migration runners and legacy SQLite cover recovery.
- `apps/server/src/catalog/`: Rust data access logic for canonical works, aliases, external IDs and duplicate merging.
- `apps/server/src/catalog/work_metadata.rs`: contributions, source bars and effective fields.
- `apps/server/migrations_pg/0011_source_metadata.sql`: metadata trust table constraints.
- `apps/server/src/cover.rs`: cover fetching, processing and transactional publication.
- `apps/server/src/cover_store.rs`: file storage, PostgreSQL index and upload recovery.
- `apps/server/src/cover_copy.rs`: resumable copy from the old SQLite cover cache.
- `apps/server/src/browse.rs`: queries and index interactions against the materialized `browse_catalogue` table.
- `apps/server/src/auth.rs`: user authentication queries and SHA-256 session token hashing.
