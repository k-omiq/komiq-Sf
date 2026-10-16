//! Which of the 63 planned tables the reverse export copies, and why.
//!
//! The rule: a table is copied when people write its rows through the API
//! (readers or admins) and each row stands alone. A table that the scanner,
//! the catalogue sync or a refresh job writes is not copied: the old build
//! fetches or rebuilds it. Admin curation that is stored in a catalogue row,
//! or in a table that only makes sense beside the catalogue state it changed,
//! is not copied either: the tool never edits a catalogue row, so every
//! catalogue row of the output is exactly the row of the kept file. One copied
//! table has a second half in the catalogue, `source_nsfw_pin`: the report
//! lists the pins whose effect on the works is missing (`export::SourcePins`).
//! One catalogue table holds rows an admin enters by hand that nothing marks
//! as such, `work_alias`: the report counts its changes (`export::AliasChanges`).
use crate::pg_import::plan::{self, Table};

/// The SQLite migration levels the output can have: production (0121) and the
/// comment-tombstone step (0122).
pub(super) const TARGETS: [i64; 2] = [121, 122];

/// PostgreSQL-only user tables that the old SQLite schema cannot preserve.
/// Nonempty tables in this disposition prevent rollback.
pub(super) const UNSUPPORTED_POSTGRES_ONLY: &[(&str, &str)] = &[
    (
        "media_takedown",
        "a rollback that dropped operator takedowns would serve taken-down content again",
    ),
    (
        "suwayomi_progress_detached",
        "detached numeric reading progress has no representation in SQLite",
    ),
    (
        "engine_detached_row",
        "detached numeric library, review and admin rows have no representation in SQLite",
    ),
    (
        "engine_numeric_state",
        "SQLite cannot preserve the generation fence for queued numeric writes",
    ),
    (
        "scanlator_rating",
        "reader ratings have no representation in the kept SQLite schema",
    ),
];

/// Omitted even when populated, with counts in the report: engine-derived identities
/// and untick records whose loss only returns the old build's behaviour. The chapters
/// stay unread in the export; the old build may read them again on a later finish.
pub(super) const REBUILDABLE_POSTGRES_ONLY: &[(&str, &str)] = &[
    (
        "media_policy",
        "the policy epoch starts again from the next policy change",
    ),
    (
        "progress_untick",
        "untick records: the chapters stay unread; the old build may read them again on a later finish",
    ),
    (
        "media_object",
        "object metadata is rebuilt by the next ingest",
    ),
    (
        "media_copy",
        "copy identities are rebuilt by the next ingest",
    ),
    (
        "media_manifest",
        "manifest revisions are rebuilt by the next ingest",
    ),
    (
        "media_manifest_page",
        "ordered pages are rebuilt by the next ingest",
    ),
    (
        "engine_generation",
        "adopted again when the PostgreSQL server starts",
    ),
    (
        "engine_manga_key",
        "source-native manga keys refill from the engine",
    ),
    (
        "engine_chapter_key",
        "source-native chapter keys refill from the engine",
    ),
];

/// Derived search and migration/import state have no user rows to export.
pub(super) const REQUIRED_POSTGRES_INTERNAL: &[&str] = &[
    "work_search_alias",
    "_sqlx_migrations",
    "komika_import_state",
    "komika_import_table",
];
pub(super) const OPTIONAL_POSTGRES_INTERNAL: &[&str] =
    &["komika_startup_import", "komika_server_started"];

/// (table, copied, reason). The record `docs/audits/postgres-reverse-export.md`
/// repeats this list; `classification_covers_the_import_plan` keeps it complete.
pub(super) const TABLES: &[(&str, bool, &str)] = &[
    (
        "all_mangadex_uuid",
        false,
        "catalogue identity ledger, written by the catalogue sync",
    ),
    (
        "catalogue_sync_state",
        false,
        "sync cursors; the kept cursors match the kept catalogue",
    ),
    (
        "cover_source_backoff",
        false,
        "cover crawl state, rebuilt by the crawl",
    ),
    (
        "extension_subscription",
        true,
        "an admin subscribes to an extension by hand",
    ),
    (
        "extension_update_log",
        false,
        "log written by the extension updater",
    ),
    (
        "feed_genre_facet",
        false,
        "derived from the catalogue by a refresh job",
    ),
    ("feed_reconcile_report", false, "report of a refresh job"),
    ("instance_setting", true, "settings an admin enters by hand"),
    (
        "maintenance_flag",
        false,
        "one-shot job markers; they describe the kept file's own data",
    ),
    (
        "scanlator_tag",
        true,
        "an admin tags a scanlation group by hand",
    ),
    (
        "series_admin",
        true,
        "per-series scan overrides an admin enters by hand",
    ),
    ("series_scan_state", false, "scanner state"),
    (
        "series_view_bucket",
        true,
        "view counts from reader traffic; nothing can fetch them again",
    ),
    (
        "series_views",
        true,
        "view counts from reader traffic; nothing can fetch them again",
    ),
    ("source_ai_tag", true, "an admin tags a source by hand"),
    (
        "source_extension",
        false,
        "catalogue: source to extension map, fetched from the engine",
    ),
    (
        "source_health_bucket",
        false,
        "telemetry written by the fetch paths",
    ),
    (
        "source_ingest_job",
        false,
        "history of ingest jobs; their result is catalogue data",
    ),
    ("source_latest_snapshot", false, "scanner state"),
    (
        "source_nsfw_pin",
        true,
        "an admin marks a source adult by hand; its effect on the works is reported, not copied",
    ),
    (
        "source_rate_limit",
        true,
        "rate limits an admin enters by hand",
    ),
    ("source_scan_outage", false, "scanner state"),
    ("suwayomi_chapter", false, "catalogue cache of the engine"),
    ("suwayomi_series", false, "catalogue cache of the engine"),
    ("sync_state", false, "sync cursor"),
    ("tag", false, "catalogue"),
    ("users", true, "accounts"),
    (
        "work",
        false,
        "catalogue; its four admin override columns are lost with it",
    ),
    ("work_redirect", false, "catalogue identity: merges"),
    (
        "work_split",
        false,
        "catalogue identity: an admin split belongs to the catalogue state it changed",
    ),
    (
        "browse_catalogue",
        false,
        "derived from the catalogue by a refresh job",
    ),
    ("canonical_progress", true, "reading progress"),
    (
        "chapter_override",
        true,
        "an admin hides or renames a chapter by hand",
    ),
    ("comments", true, "comments"),
    (
        "content_block_rule",
        true,
        "block rules a reader or an admin enters by hand",
    ),
    (
        "feed_series_updates",
        false,
        "derived from the catalogue by a refresh job",
    ),
    (
        "feed_updates",
        false,
        "derived from the catalogue by a refresh job",
    ),
    (
        "release_event",
        false,
        "release ledger derived from catalogue chapters",
    ),
    (
        "report",
        true,
        "reports from readers and the admin's triage notes",
    ),
    ("reviews", true, "reviews"),
    ("sessions", true, "sessions"),
    ("source_series", false, "catalogue"),
    ("suwayomi_cover_issue", false, "cover crawl state"),
    ("suwayomi_progress", true, "reading progress"),
    ("user_activity", true, "activity feed of an account"),
    ("user_avatars", true, "avatars"),
    ("user_library", true, "libraries"),
    (
        "work_alias",
        false,
        "catalogue; titles an admin adds or removes by hand are lost with it and counted in the report",
    ),
    ("work_alias_token", false, "derived from work_alias"),
    ("work_cover", false, "catalogue"),
    ("work_cover_issue", false, "cover crawl state"),
    ("work_credit", false, "catalogue"),
    ("work_description", false, "catalogue"),
    ("work_external_id", false, "catalogue"),
    ("work_stats", false, "catalogue: upstream statistics"),
    ("work_tag", false, "catalogue"),
    ("work_tracker_link", false, "catalogue"),
    (
        "work_field_pin",
        false,
        "admin choice paired with a work column by the work_pin triggers",
    ),
    ("chapter", false, "catalogue"),
    ("comment_media", true, "images attached to comments"),
    ("comment_votes", true, "votes on comments"),
    (
        "merge_candidate",
        false,
        "catalogue identity: the merge queue",
    ),
    ("notifications", true, "notifications"),
];

pub(super) fn is_copied(table: &str) -> bool {
    TABLES
        .iter()
        .any(|(name, copied, _)| *name == table && *copied)
}

/// The copied tables in the import plan's order (a parent before its child),
/// with the columns the target level has. Migration 0122 added
/// `comments.deleted_by`; a 0121 file has no such column.
pub(super) fn copied(target: i64) -> Vec<Table> {
    let mut tables = plan::tables()
        .into_iter()
        .filter(|t| is_copied(&t.name))
        .collect::<Vec<_>>();
    if target < 122 {
        for table in tables.iter_mut().filter(|t| t.name == "comments") {
            table.columns.retain(|c| c.name != "deleted_by");
            table.checks.retain(|check| !check.contains("deleted_by"));
        }
    }
    tables
}

/// A reference by value that no foreign key declares, from a copied table to
/// a catalogue row. The old build keeps such a row and shows a dead entry
/// (`merge_works` in `catalog/mod.rs` names these columns), so the export
/// keeps the row and reports it. A row whose work PostgreSQL made by merging
/// exactly one work of the kept file into a newer one gets that kept work's
/// id (`export::work_references`). Only `w_` work ids are checked: a numeric
/// series id is the engine's and resolves there, and chapter ids come from the
/// upstream source and return with the scanner.
pub(super) const WORK_REFERENCES: &[(&str, &str, &str)] = &[
    ("user_library", "series_id", "1"),
    ("reviews", "series_id", "1"),
    ("canonical_progress", "work_id", "1"),
    ("comments", "target_id", "target_type = 'series'"),
    ("notifications", "target_id", "target_type = 'series'"),
    ("user_activity", "target_id", "target_type = 'series'"),
    ("series_admin", "series_id", "1"),
    ("series_views", "series_key", "1"),
    ("series_view_bucket", "series_key", "1"),
    ("report", "subject_id", "1"),
    ("report", "subject_id_secondary", "1"),
];
