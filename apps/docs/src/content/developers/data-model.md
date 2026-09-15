---
title: Data model
description: Understand the database schema, table relationships and migrations for the server inside your admin app.
---

Komiq is self-hostable: the admin app runs the server and gives you its management portal, and the reader app connects to a server. That server stores user accounts, authentication sessions, reading progress, community discussions and its canonical comic catalogue in SQLite. This page explains its database files, table relationships and migrations at startup.

## Dual-database architecture

The server separates persistent application data from transient cover images by maintaining two SQLite databases:

1. **Main database**: configured with `DATABASE_URL` (default `sqlite://komika.sqlite3`). This database holds accounts, sessions, reading history, social data and the canonical catalog. When configured, Litestream replicates this file continuously to object storage for automated backup.
2. **Cover cache database**: configured with `COVERS_DATABASE_URL` (defaulting to a sibling file named `covers.sqlite3`). This database holds WebP image binaries for comic covers and PNG blobs for extension icons.

Cover images are large binary blobs that can be re-downloaded from upstream sources. Keeping them in a separate database excludes them from Litestream backups of the main database. If the cover database is lost, the server re-populates it on demand without affecting user data.

### Connection pools and PRAGMA settings

Both databases are opened with asynchronous connection pools managed by `sqlx`. The server configures specific SQLite PRAGMAs in `apps/server/src/db.rs` to optimize concurrency and memory usage:

| Setting           | Main pool | Cover pool | Rationale                                                    |
| ----------------- | --------- | ---------- | ------------------------------------------------------------ |
| `max_connections` | 8         | 4          | Bounds thread pool concurrency for each workload             |
| `journal_mode`    | `WAL`     | `WAL`      | Allows concurrent readers alongside background writers       |
| `synchronous`     | `NORMAL`  | `NORMAL`   | WAL-safe durability setting that reduces disk sync latency   |
| `busy_timeout`    | 15s       | 15s        | Absorbs write locks during intensive sync and scan passes    |
| `foreign_keys`    | `ON`      | `OFF`      | Enforces referential integrity on the main relational schema |
| `mmap_size`       | 1.5 GiB   | 256 MB     | Memory-maps B-tree pages into process address space          |
| `cache_size`      | -16000    | -16000     | Allocates roughly 16 MB of RAM for the SQLite page cache     |
| `temp_store`      | `MEMORY`  | `MEMORY`   | Keeps temporary sorting tables and aggregations in RAM       |

### Query planner statistics

SQLite relies on the `sqlite_stat1` table to choose efficient indexes. Frequent inserts during catalog synchronization can make planner statistics stale.

A background supervisor runs `ANALYZE` every 6 hours on volatile tables: `browse_catalogue`, `feed_updates`, `feed_series_updates`, `source_series`, `suwayomi_series`, `series_scan_state`, `merge_candidate` and `notifications`. This keeps the query planner accurate without relying on sampled approximations.

## Core tables and relationships

The database schema connects sources, canonical works, chapters and user data through foreign keys:

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

The catalog models logical works independently from upstream source copies:

- `work`: the central table for a logical series. It uses a prefixed primary key (`w_...`). Columns include `primary_title`, `description`, `year`, `original_language`, `status`, `demographic`, `content_rating`, `is_nsfw`, `author`, `artist` and `cover_phash` (a perceptual hash used for cover-based matching).
- `source_series`: represents a concrete series on an upstream source such as MangaDex or Suwayomi. It references `work(id)` with cascade deletion. It enforces uniqueness on `(source_type, source_id, source_key)`.
- `source_extension`: one row per Suwayomi source, keyed by source id. It holds the extension package name, the repository URL and the source's NSFW flag. The server writes it when an extension is installed or updated and on the scheduled scan. `chapterOrigin` and ingestion read it.
- `source_nsfw_pin`: one row per source that an administrator marked adult with `markSourceNsfw`. Ingestion reads it, so a series added later from that source is stored adult. The scheduled scan does not rewrite it.
- `source_rate_limit`: one row per source that an administrator limited with `setSourceRateLimit`. It holds the allowed requests and the window in seconds. The scan supervisor reads it on each reconcile.
- `scanlator_tag`: one row per scanlator name that an administrator tagged with `setScanlatorAi`. It holds the case-folded name and the level, `ai` or `partial`. The tag applies on read to every chapter credited to that name. A joint credit takes the strongest tag of its members.
- `chapter`: mirrors chapter metadata for each source series. It references `source_series(id)` with cascade deletion and enforces uniqueness on `(source_series_id, external_id)`. Columns include `number`, `volume`, `title`, `scanlator`, `published_at`, `readable_at`, `external_url` and a normalized `chapter_key`.
- `work_alias`: stores alternate titles and romanized names. It references `work(id)` and provides a normalized title index for fuzzy matching and search.
- `work_external_id`: maps canonical works to external database identifiers from services like AniList, MyAnimeList, MangaUpdates, Kitsu and MangaDex. The primary key is `(provider, external_id)`.
- `merge_candidate`: records potential duplicate matches between a `source_series` and a candidate `work(id)`. It stores a confidence score and a detection method, allowing administrators to review and resolve candidates.
- `work_field_pin`: records administrator overrides for source-managed fields like author, artist, year and status. Database triggers automatically restore pinned values if an automated synchronization pass updates the underlying `work` row.

### Identity and social tables

These tables manage users, sessions, comments and ratings:

- `users`: user accounts with `id`, unique `username`, unique `email`, `password_hash` (Argon2id), `is_admin`, `is_banned`, `show_nsfw`, display profile fields and `must_change_password`.
- `sessions`: active authentication sessions. The primary key `token` stores a lowercase hexadecimal SHA-256 hash of the bearer token rather than plaintext. It references `users(id)` with cascade deletion and stores an ISO 8601 UTC `expires_at` timestamp.
- `reviews`: user ratings and written reviews for works. Scores range from 1 to 10. A unique constraint on `(series_id, user_id)` ensures each user has at most one review per work.
- `comments`: discussion comments attached to chapters or series. Comments support markdown formatting and spoiler tags.
- `comment_media`: image attachments uploaded for comments. Binary image data is stored as WebP blobs.
- `comment_votes`: user likes and dislikes on comments, keyed by `(comment_id, user_id)`.
- `notifications`: in-app notifications for comment replies and comment like milestones.

### Library and reading progress tables

Reading history and personal collections are stored per user:

- `user_library`: links a user to a series (`user_id`, `series_id`). It stores an optional shelf `status` (`reading`, `completed`, `onhold`, `plan`) and an `is_favorite` boolean flag.
- `canonical_progress`: records reading progress for canonical `w_...` works. Keyed by `(user_id, chapter_id)`, it stores `last_page_read`, a `read` boolean flag and an `updated_at` timestamp.
- `suwayomi_progress`: records reading progress for numeric Suwayomi-sourced series. It mirrors the structure of `canonical_progress` for non-canonical sources.
- `series_views` and `series_view_bucket`: tracks chapter read counts aggregated across rolling hourly time buckets to calculate trending feeds.

### Materialized query tables

To keep browse and search screens fast, the database maintains pre-computed summary tables:

- `browse_catalogue`: a wide table containing every browsable canonical work. It includes denormalized columns for demographic, language, year, rating, follow counts and format flags. Complex browse filters query this table directly to avoid multi-table joins during pagination.
- `feed_series_updates`: a materialized feed table storing recent chapter releases ordered by upstream publication dates.
- `work_fts`: an SQLite FTS5 virtual table indexing titles and aliases for full-text search.
- `instance_setting`: a key-value store holding runtime configuration changed from the admin console, such as registration toggles and instance names.

## Cover cache database schema

The separate `covers.sqlite3` database contains tables created inline during startup in `apps/server/src/db.rs`:

- `work_cover_blob`: primary storage for canonical work covers. Keyed by `work_id`, it stores the downscaled WebP binary blob, a version integer and an update timestamp.
- `suwayomi_cover_blob`: cached WebP covers for Suwayomi manga, keyed by numeric Suwayomi manga ID.
- `cover_variant`: resized derivatives of master covers for specific display widths (such as thumbnail cards). Keyed by `(asset_key, width)`.
- `cover_maintenance`: durable progress cursors for background maintenance passes over the cover database.
- `extension_icon_blob`: PNG binary blobs for extension icons, keyed by package name.

## How migrations work

Schema migrations are written in SQL and stored in `apps/server/migrations/`.

Each migration file begins with a four-digit sequential prefix followed by an explanatory name:

```text
apps/server/migrations/
├── 0001_init.sql
├── 0002_series_admin.sql
├── 0003_series_scan_state.sql
├── 0004_user_ban.sql
├── 0005_canonical_works.sql
└── ...
```

### Execution at startup

When the server initializes in `apps/server/src/db.rs`, it runs migrations automatically using `sqlx::migrate!`:

```rust
sqlx::migrate!("./migrations").run(&pool).await?;
```

The migration runner inspects the internal `_sqlx_migrations` table in the database. For each file in the migrations folder:

1. It checks whether the migration version number has already been applied.
2. It verifies the SHA-384 checksum of applied migrations to detect unexpected modifications to historical files.
3. If a migration is new, it executes the SQL statements within a database transaction and records the result in `_sqlx_migrations`.

### Migration design principles

The project follows strict migration guidelines to protect data integrity:

- **Strictly additive changes**: migrations add new columns with `ALTER TABLE ADD COLUMN` or create new tables. Existing columns are not renamed or dropped without compatibility layers.
- **Deterministic backfills**: when introducing new relational structures, migrations include `INSERT ... SELECT` statements to backfill placeholder rows from existing records.
- **Triggers for backward compatibility**: migrations create database triggers to synchronize overlapping data across older and newer table structures during multi-step deployments.
- **Explicit indexing**: every foreign key and query filter column receives an explicit index to prevent table scans during pagination.

## Where to look

- `apps/server/migrations/`: chronological SQL migration scripts defining the database schema.
- `apps/server/src/db.rs`: SQLite connection pool configuration, PRAGMA settings and migration runner invocation.
- `apps/server/src/catalog/`: Rust data access logic for canonical works, aliases, external IDs and duplicate merging.
- `apps/server/src/cover.rs`: database read and write operations for cover blobs and image variants.
- `apps/server/src/browse.rs`: queries and index interactions against the materialized `browse_catalogue` table.
- `apps/server/src/auth.rs`: user authentication queries and SHA-256 session token hashing.
