---
title: Data model
description: Understand the database schema, table relationships and migrations for the server inside your admin app.
---

Komiq is self-hostable: the admin app runs the server and gives you its management portal, and the reader app connects to a server. That server stores user accounts, authentication sessions, reading progress, community discussions and its canonical comic catalogue in PostgreSQL 18. This page explains storage and migrations.

## Main database and cover cache

The server separates durable application records from refetchable images:

1. **Main database**: `DATABASE_URL` is a PostgreSQL connection URL. The standalone default is `postgres://localhost/komika`; Docker and desktop supervisors supply their own endpoint and application role. It holds accounts, sessions, reading history, social data and the canonical catalogue.
2. **Cover cache**: `COVERS_DATABASE_URL` is a SQLite URL. The standalone default is `sqlite://covers.sqlite3`. It holds WebP covers and PNG extension icons. Docker sets `sqlite:///data/covers.sqlite3`.

Main backups use PostgreSQL tools. The global deployment packages pgBackRest, a backup manager that archives database files and transaction logs to object storage. Self-host Docker uses `pg_dump`, a consistent logical export. The desktop app saves a local dump when the newest is at least 20 hours old and keeps seven. Covers are excluded because they can be fetched again.

### Connections and native types

`sqlx` manages asynchronous PostgreSQL pools through `apps/server/src/database.rs`. Reader sessions are query-only and use UTC. Inspect the pool settings in code before tuning the PostgreSQL connection limit.

The main schema uses native PostgreSQL values: UUID account identities, timezone-aware timestamps, Boolean flags and JSON values where the application treats them as structured data. Prefixed work IDs and other non-UUID identifiers stay text. API timestamps use UTC with six fractional digits and `+00:00`.

Covers retain SQLite connection settings and recovery in `apps/server/src/db.rs`. PostgreSQL planner maintenance uses `ANALYZE`; the cover cache retains its separate SQLite maintenance path.

### Automatic conversion

Before `db::init`, every start runs `pg_import::startup::run`. It looks for the old main SQLite file at `KOMIKA_SQLITE_IMPORT_PATH` (absolute) or `komika.sqlite3` beside the covers file. It decides from that file, a record beside it (`<file>.import-work-state`) and what PostgreSQL holds:

- Nothing to import: no output.
- A file and an empty PostgreSQL: copy, migrate and repair a work copy, import, verify, then rename the original to `<file>.imported-<UTC time>` and serve.
- A file and a PostgreSQL in use: one warning; the file stays.
- An unfinished import resumes, or restarts once if the file, the migrations or the plan changed. PostgreSQL rows are reset only when `public.komika_startup_import` proves that this record began the import. A start that only finishes an interrupted archive rename ends with a `sqlite import: complete:` line.
- Anything else: one `sqlite import: error:` line that names the files and the ways out. Nothing changes.

The server never deletes a file that may be the last copy of the owner's data. A saved copy that differs from the file now at the original name is moved to `<file>.import-kept-<UTC time>` with a warning. A SQLite migration newer than this server knows, or a PostgreSQL migration it does not embed, is refused as from a newer release. `public.komika_server_started` is written before a normal start, and no import starts after that. Work files need 8 times the source size plus 256 MiB free. On Unix the source folder must belong to the server user and not be group- or world-writable.

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
- `engine_generation`, `engine_manga_key` and `engine_chapter_key`: the source engine's own manga and chapter keys and web links, added by migration `0005_engine_keys.sql`. Keys are stored per generation, and each generation row has a state of `active`, `quarantined` or `detached`; the server adopts or creates one `active` generation at start. `chapterOrigin` reads these rows. The tables exist only in PostgreSQL, and the reverse export to SQLite leaves them out because the server rebuilds them from the engine.
- `engine_numeric_state` and `suwayomi_progress_detached`, added by migration `0006_engine_fence.sql`: the numeric epoch that fences queued progress writes after an engine change, and the extension-source reading progress set aside when an administrator starts a new engine generation. The reverse export to SQLite refuses while either holds rows.

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
- `series_views` and `series_view_bucket`: tracks chapter read counts aggregated across rolling hourly time buckets to calculate trending feeds.

### Materialized query tables

Precomputed tables support browse and search:

- `browse_catalogue`: a wide table containing every browsable canonical work. It stores filter and ranking fields so browse queries avoid repeated joins.
- `feed_series_updates`: a materialized feed table storing recent chapter releases ordered by upstream publication dates.
- `work_search_alias`: normalized titles and aliases with PostgreSQL full-text vectors and `pg_trgm` indexes for typo candidates.
- `instance_setting`: a key-value store holding runtime configuration changed from the admin console, such as registration toggles and instance names.

## Cover cache database schema

`apps/server/src/db.rs` creates these tables in `covers.sqlite3`:

- `work_cover_blob`: primary storage for canonical work covers. Keyed by `work_id`, it stores the downscaled WebP binary blob, a version integer and an update timestamp.
- `suwayomi_cover_blob`: cached WebP covers for Suwayomi manga, keyed by numeric Suwayomi manga ID.
- `cover_variant`: resized derivatives of master covers for specific display widths (such as thumbnail cards). Keyed by `(asset_key, width)`.
- `cover_maintenance`: durable progress cursors for background maintenance passes over the cover database.
- `extension_icon_blob`: PNG binary blobs for extension icons, keyed by package name.

## How migrations work

The main migration history lives in `apps/server/migrations_pg/`. It begins with the PostgreSQL baseline. The files in `apps/server/migrations/` are frozen SQLite history for old import sources and historical tests; never apply them to PostgreSQL.

### Execution at startup

`apps/server/src/db.rs` applies main migrations with SQLx's PostgreSQL migrator. `_sqlx_migrations` records versions and checksums, so changing an applied historical migration causes a startup error. Add a new migration for a new schema change.

The importer prepares the old source schema using the frozen SQLite migrator on a temporary copy. Type conversion and canonical checksums live in `apps/server/src/pg_import/`. The separate covers schema is initialized and checked on its own SQLite connection.

### Migration changes

Keep applied migrations immutable. Add new files for schema changes and backfills. Test the resulting constraints and queries on PostgreSQL. Covers and old import sources keep their own SQLite history; a PostgreSQL change must not rewrite that source history.

## Where to look

- `apps/server/migrations_pg/`: PostgreSQL baseline and migration history.
- `apps/server/migrations/`: frozen SQLite source migration history.
- `apps/server/src/pg_import/`: type conversion, import, verification and the startup decision (`startup.rs`).
- `apps/server/src/db.rs`: PostgreSQL initialization, migration runners and SQLite cover recovery.
- `apps/server/src/catalog/`: Rust data access logic for canonical works, aliases, external IDs and duplicate merging.
- `apps/server/src/cover.rs`: database read and write operations for cover blobs and image variants.
- `apps/server/src/browse.rs`: queries and index interactions against the materialized `browse_catalogue` table.
- `apps/server/src/auth.rs`: user authentication queries and SHA-256 session token hashing.
