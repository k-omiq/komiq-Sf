//! PostgreSQL recovery-chain readings for the loopback `/metrics` scrape: current
//! WAL directory size, archiver health, archive backlog, raw WAL generated and replication.
//!
//! `pg_ls_waldir()` and `pg_ls_archive_statusdir()` need an EXECUTE grant (or
//! `pg_monitor`) that an application role may not have. Each scrape asks
//! `has_function_privilege` first and calls a directory function only when it is
//! granted, so a missing grant writes no ERROR into the PostgreSQL log. A value that
//! could not be read is left out of the scrape (its HELP/TYPE lines stay) and
//! `komika_postgres_metric_available{metric=..} 0` says so; it is never shown as a
//! number such as -1. `pg_stat_archiver` and `pg_stat_wal` need no grant.

use std::fmt::{Display, Write};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use sqlx::PgConnection;

/// One scrape's readings; `None` is "not readable", never zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RecoveryMetrics {
    /// Sum of the files in `pg_wal` (`pg_ls_waldir()`).
    pub wal_dir_bytes: Option<i64>,
    /// `.ready` files in `pg_wal/archive_status`: segments not yet archived.
    pub archive_ready_files: Option<i64>,
    pub archiver: Option<ArchiverStats>,
    pub replication: Option<ReplicationStats>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ArchiverStats {
    pub archived_count: i64,
    pub failed_count: i64,
    pub last_archived_age_seconds: Option<i64>,
    pub last_failed_age_seconds: Option<i64>,
    /// The latest archive attempt failed: last failure newer than last success.
    pub failing: bool,
    /// `pg_stat_wal.wal_bytes`: WAL generated since the statistics reset.
    pub wal_generated_bytes: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ReplicationStats {
    pub current_wal_lsn_bytes: i64,
    pub standbys: Vec<StandbyStats>,
    pub slots: Vec<ReplicationSlotStats>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub(crate) struct StandbyStats {
    pub application_name: String,
    pub state: String,
    pub sent_lsn_bytes: Option<i64>,
    pub flush_lsn_bytes: Option<i64>,
    pub replay_lsn_bytes: Option<i64>,
    pub replay_lag_seconds: Option<f64>,
}

impl StandbyStats {
    fn replay_lag_seconds(&self) -> Option<f64> {
        self.replay_lag_seconds.or_else(|| {
            self.replay_lsn_bytes
                .filter(|lsn| Some(*lsn) == self.sent_lsn_bytes)
                .map(|_| 0.0)
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub(crate) struct ReplicationSlotStats {
    pub slot_name: String,
    pub active: bool,
    pub wal_status: Option<String>,
    pub retained_bytes: Option<i64>,
}

/// Whole seconds since a timestamp, never negative. NULL stays NULL (the sample
/// is left out): `GREATEST(0, NULL)` is 0 in PostgreSQL, hence the CASE.
macro_rules! age_seconds {
    ($column:literal) => {
        concat!(
            "CASE WHEN ",
            $column,
            " IS NOT NULL THEN GREATEST(0, FLOOR(EXTRACT(EPOCH FROM now() - ",
            $column,
            "))) END::bigint"
        )
    };
}

const STATS_SQL: &str = concat!(
    "SELECT a.archived_count, a.failed_count, ",
    age_seconds!("a.last_archived_time"),
    ", ",
    age_seconds!("a.last_failed_time"),
    ", (a.last_failed_time IS NOT NULL \
      AND (a.last_archived_time IS NULL OR a.last_failed_time > a.last_archived_time)), \
     w.wal_bytes::bigint, \
     has_function_privilege('pg_catalog.pg_ls_waldir()', 'EXECUTE'), \
     has_function_privilege('pg_catalog.pg_ls_archive_statusdir()', 'EXECUTE') \
     FROM pg_catalog.pg_stat_archiver a, pg_catalog.pg_stat_wal w"
);
const WAL_DIR_SQL: &str = "(SELECT COALESCE(SUM(size), 0)::bigint FROM pg_catalog.pg_ls_waldir())";
const READY_SQL: &str = "(SELECT COUNT(*) FROM pg_catalog.pg_ls_archive_statusdir() \
     WHERE name LIKE '%.ready')";

type StatsRow = (i64, i64, Option<i64>, Option<i64>, bool, i64, bool, bool);
type ReplicationRow = (
    i64,
    sqlx::types::Json<Vec<StandbyStats>>,
    sqlx::types::Json<Vec<ReplicationSlotStats>>,
);

const REPLICATION_ACCESS_SQL: &str = "SELECT pg_is_in_recovery(), \
    pg_has_role(current_user, 'pg_read_all_stats', 'USAGE') OR \
    (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user)";
const REPLICATION_SQL: &str = "WITH current_wal AS MATERIALIZED \
    (SELECT pg_current_wal_lsn() AS lsn), senders AS (\
     SELECT application_name, state, \
     pg_wal_lsn_diff(sent_lsn, '0/0')::bigint AS sent_lsn_bytes, \
     pg_wal_lsn_diff(flush_lsn, '0/0')::bigint AS flush_lsn_bytes, \
     pg_wal_lsn_diff(replay_lsn, '0/0')::bigint AS replay_lsn_bytes, \
     EXTRACT(EPOCH FROM replay_lag)::float8 AS replay_lag_seconds \
     FROM {source} WHERE state <> 'backup') \
     SELECT pg_wal_lsn_diff(w.lsn, '0/0')::bigint, \
     COALESCE((SELECT jsonb_agg(to_jsonb(r)) FROM senders r), '[]'::jsonb), \
     COALESCE((SELECT jsonb_agg(to_jsonb(s)) FROM (\
       SELECT slot_name, active, wal_status, \
       pg_wal_lsn_diff(w.lsn, restart_lsn)::bigint AS retained_bytes \
       FROM pg_catalog.pg_replication_slots WHERE slot_type = 'physical'\
     ) s), '[]'::jsonb) FROM current_wal w";

fn replication_sql(source: &str) -> String {
    REPLICATION_SQL.replace("{source}", source)
}

static WARNED_UNGRANTED: AtomicBool = AtomicBool::new(false);
static WARNED_REPLICATION_UNGRANTED: AtomicBool = AtomicBool::new(false);

/// Read every recovery value over one read-pool connection (at most four
/// statements). Errors other than a denied permission are recorded as read
/// database errors, as the other gauges do.
pub(crate) async fn collect(pool: &crate::db::Database) -> RecoveryMetrics {
    match pool.read().acquire().await {
        Ok(mut conn) => collect_on(&mut conn).await,
        Err(error) => {
            crate::db::record_read_database_error(&error);
            RecoveryMetrics::default()
        }
    }
}

pub(crate) async fn collect_on(conn: &mut PgConnection) -> RecoveryMetrics {
    let row: StatsRow = match sqlx::query_as(STATS_SQL).fetch_one(&mut *conn).await {
        Ok(row) => row,
        Err(error) => {
            record_unless_denied(&error);
            return RecoveryMetrics::default();
        }
    };
    let (archived, failed, archived_age, failed_age, failing, wal_generated, wal_ok, ready_ok) =
        row;
    let ungranted = !wal_ok || !ready_ok;
    if ungranted && !WARNED_UNGRANTED.swap(true, Relaxed) {
        tracing::warn!(
            pg_ls_waldir = wal_ok,
            pg_ls_archive_statusdir = ready_ok,
            "metrics: the database role cannot run a WAL directory function; \
             those /metrics values are left out (grant EXECUTE or pg_monitor)"
        );
    }
    let (wal_dir_bytes, archive_ready_files) = read_dirs(conn, wal_ok, ready_ok).await;
    let replication = read_replication(conn).await;
    RecoveryMetrics {
        wal_dir_bytes,
        archive_ready_files,
        archiver: Some(ArchiverStats {
            archived_count: archived,
            failed_count: failed,
            last_archived_age_seconds: archived_age,
            last_failed_age_seconds: failed_age,
            failing,
            wal_generated_bytes: wal_generated,
        }),
        replication,
    }
}

async fn read_replication(conn: &mut PgConnection) -> Option<ReplicationStats> {
    let (in_recovery, granted): (bool, bool) = match sqlx::query_as(REPLICATION_ACCESS_SQL)
        .fetch_one(&mut *conn)
        .await
    {
        Ok(row) => row,
        Err(error) => {
            record_unless_denied(&error);
            return None;
        }
    };
    if in_recovery {
        return None;
    }
    if !granted {
        if !WARNED_REPLICATION_UNGRANTED.swap(true, Relaxed) {
            tracing::warn!(
                "metrics: the database role cannot read other WAL senders' columns; \
                 replication /metrics values are left out (grant pg_read_all_stats)"
            );
        }
        return None;
    }
    let sql = replication_sql("pg_catalog.pg_stat_replication");
    match read_replication_stats(conn, &sql).await {
        Ok(stats) => Some(stats),
        Err(error) => {
            record_unless_denied(&error);
            None
        }
    }
}

async fn read_replication_stats(
    conn: &mut PgConnection,
    sql: &str,
) -> Result<ReplicationStats, sqlx::Error> {
    let (current_wal_lsn_bytes, sqlx::types::Json(standbys), sqlx::types::Json(slots)) =
        sqlx::query_as::<_, ReplicationRow>(sql)
            .fetch_one(conn)
            .await?;
    Ok(ReplicationStats {
        current_wal_lsn_bytes,
        standbys,
        slots,
    })
}

/// Both directory readings in one statement; a function that is not wanted is
/// never named in the SQL, because PostgreSQL checks EXECUTE when it plans it.
async fn read_dirs(conn: &mut PgConnection, wal: bool, ready: bool) -> (Option<i64>, Option<i64>) {
    if !wal && !ready {
        return (None, None);
    }
    let sql = format!(
        "SELECT {}, {}",
        if wal { WAL_DIR_SQL } else { "NULL::bigint" },
        if ready { READY_SQL } else { "NULL::bigint" },
    );
    match sqlx::query_as::<_, (Option<i64>, Option<i64>)>(&sql)
        .fetch_one(conn)
        .await
    {
        Ok(values) => values,
        Err(error) => {
            record_unless_denied(&error);
            (None, None)
        }
    }
}

/// SQLSTATE 42501 is a missing grant, a configuration state, not a failing read.
fn record_unless_denied(error: &sqlx::Error) {
    let denied = error
        .as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "42501");
    if !denied {
        crate::db::record_read_database_error(error);
    }
}

/// Prometheus text. `komika_sqlite_wal_bytes` keeps its historical name.
pub(crate) fn prometheus(metrics: &RecoveryMetrics) -> String {
    let mut out = String::new();
    family(
        &mut out,
        "komika_sqlite_wal_bytes",
        "gauge",
        "Current PostgreSQL WAL directory bytes (absent when pg_ls_waldir() is not granted)",
        metrics.wal_dir_bytes,
    );
    out.push_str(
        "# HELP komika_postgres_metric_available 1 when a PostgreSQL reading was readable this scrape, 0 when it was left out\n\
         # TYPE komika_postgres_metric_available gauge\n",
    );
    for (name, available) in [
        ("wal_bytes", metrics.wal_dir_bytes.is_some()),
        ("archive_ready_files", metrics.archive_ready_files.is_some()),
        ("archiver", metrics.archiver.is_some()),
        ("replication", metrics.replication.is_some()),
    ] {
        let _ = writeln!(
            out,
            "komika_postgres_metric_available{{metric=\"{name}\"}} {}",
            u8::from(available)
        );
    }
    family(
        &mut out,
        "komika_postgres_archive_ready_files",
        "gauge",
        "WAL segments waiting to be archived (.ready files in pg_wal/archive_status)",
        metrics.archive_ready_files,
    );
    let a = metrics.archiver.as_ref();
    family(
        &mut out,
        "komika_postgres_archived_total",
        "counter",
        "WAL files archived successfully (pg_stat_archiver.archived_count)",
        a.map(|a| a.archived_count),
    );
    family(
        &mut out,
        "komika_postgres_archive_failed_total",
        "counter",
        "Failed WAL archive attempts (pg_stat_archiver.failed_count)",
        a.map(|a| a.failed_count),
    );
    family(
        &mut out,
        "komika_postgres_archive_last_success_age_seconds",
        "gauge",
        "Seconds since the last successful WAL archive (absent when none)",
        a.and_then(|a| a.last_archived_age_seconds),
    );
    family(
        &mut out,
        "komika_postgres_archive_last_failure_age_seconds",
        "gauge",
        "Seconds since the last failed WAL archive attempt (absent when none)",
        a.and_then(|a| a.last_failed_age_seconds),
    );
    family(
        &mut out,
        "komika_postgres_archive_failing",
        "gauge",
        "1 when the last archive failure is newer than the last success",
        a.map(|a| i64::from(a.failing)),
    );
    family(
        &mut out,
        "komika_postgres_wal_generated_bytes_total",
        "counter",
        "WAL bytes generated since the statistics reset (pg_stat_wal.wal_bytes)",
        a.map(|a| a.wal_generated_bytes),
    );
    let r = metrics.replication.as_ref();
    family(
        &mut out,
        "komika_postgres_replication_standbys",
        "gauge",
        "WAL senders excluding backups",
        r.map(|r| r.standbys.len() as i64),
    );
    family(
        &mut out,
        "komika_postgres_current_wal_lsn_bytes",
        "gauge",
        "Current primary WAL position in bytes",
        r.map(|r| r.current_wal_lsn_bytes),
    );
    labelled_gauge(
        &mut out,
        "komika_postgres_replication_sent_lsn_bytes",
        "WAL position sent to the standby in bytes",
        "standby",
        r.into_iter()
            .flat_map(|r| &r.standbys)
            .map(|s| (s.application_name.as_str(), s.sent_lsn_bytes)),
    );
    labelled_gauge(
        &mut out,
        "komika_postgres_replication_flush_lsn_bytes",
        "WAL position flushed by the standby in bytes",
        "standby",
        r.into_iter()
            .flat_map(|r| &r.standbys)
            .map(|s| (s.application_name.as_str(), s.flush_lsn_bytes)),
    );
    labelled_gauge(
        &mut out,
        "komika_postgres_replication_replay_lsn_bytes",
        "WAL position replayed by the standby in bytes",
        "standby",
        r.into_iter()
            .flat_map(|r| &r.standbys)
            .map(|s| (s.application_name.as_str(), s.replay_lsn_bytes)),
    );
    labelled_gauge(
        &mut out,
        "komika_postgres_replication_replay_lag_seconds",
        "Standby replay lag in seconds",
        "standby",
        r.into_iter()
            .flat_map(|r| &r.standbys)
            .map(|s| (s.application_name.as_str(), s.replay_lag_seconds())),
    );
    labelled_gauge(
        &mut out,
        "komika_postgres_replication_slot_retained_bytes",
        "WAL bytes retained by the physical slot",
        "slot",
        r.into_iter()
            .flat_map(|r| &r.slots)
            .map(|s| (s.slot_name.as_str(), s.retained_bytes)),
    );
    labelled_gauge(
        &mut out,
        "komika_postgres_replication_slot_active",
        "1 when the physical slot is active",
        "slot",
        r.into_iter()
            .flat_map(|r| &r.slots)
            .map(|s| (s.slot_name.as_str(), Some(u8::from(s.active)))),
    );
    out
}

fn labelled_gauge<'a, T: Display>(
    out: &mut String,
    name: &str,
    help: &str,
    label: &str,
    values: impl Iterator<Item = (&'a str, Option<T>)>,
) {
    family(out, name, "gauge", help, None);
    for (value, reading) in values {
        if let Some(reading) = reading {
            let escaped = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            let _ = writeln!(out, "{name}{{{label}=\"{escaped}\"}} {reading}");
        }
    }
}

fn family(out: &mut String, name: &str, kind: &str, help: &str, value: Option<i64>) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    if let Some(value) = value {
        let _ = writeln!(out, "{name} {value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample<'a>(body: &'a str, name: &str) -> Option<&'a str> {
        body.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
    }

    #[test]
    fn unavailable_values_are_absent_not_negative() {
        let body = prometheus(&RecoveryMetrics::default());
        assert!(body.contains("# TYPE komika_sqlite_wal_bytes gauge\n"));
        assert_eq!(sample(&body, "komika_sqlite_wal_bytes"), None);
        assert_eq!(sample(&body, "komika_postgres_archived_total"), None);
        assert_eq!(sample(&body, "komika_postgres_archive_ready_files"), None);
        for metric in ["wal_bytes", "archive_ready_files", "archiver"] {
            let name = format!("komika_postgres_metric_available{{metric=\"{metric}\"}}");
            assert_eq!(sample(&body, &name), Some("0"), "{metric}");
        }
        assert!(!body.contains("-1"));
    }

    #[test]
    fn archiver_readings_render() {
        let body = prometheus(&RecoveryMetrics {
            wal_dir_bytes: Some(1_778_413_942),
            archive_ready_files: Some(3),
            archiver: Some(ArchiverStats {
                archived_count: 430,
                failed_count: 275,
                last_archived_age_seconds: Some(12),
                last_failed_age_seconds: None,
                failing: false,
                wal_generated_bytes: 9_000,
            }),
            ..Default::default()
        });
        assert_eq!(sample(&body, "komika_sqlite_wal_bytes"), Some("1778413942"));
        assert_eq!(
            sample(&body, "komika_postgres_archive_ready_files"),
            Some("3")
        );
        assert_eq!(sample(&body, "komika_postgres_archived_total"), Some("430"));
        assert_eq!(
            sample(&body, "komika_postgres_archive_failed_total"),
            Some("275")
        );
        assert_eq!(
            sample(&body, "komika_postgres_archive_last_success_age_seconds"),
            Some("12")
        );
        assert_eq!(
            sample(&body, "komika_postgres_archive_last_failure_age_seconds"),
            None
        );
        assert_eq!(sample(&body, "komika_postgres_archive_failing"), Some("0"));
        assert_eq!(
            sample(&body, "komika_postgres_wal_generated_bytes_total"),
            Some("9000")
        );
        assert_eq!(
            sample(
                &body,
                "komika_postgres_metric_available{metric=\"wal_bytes\"}"
            ),
            Some("1")
        );
    }

    #[test]
    fn unavailable_replication_values_are_absent() {
        let body = prometheus(&RecoveryMetrics::default());
        for name in [
            "komika_postgres_replication_standbys",
            "komika_postgres_current_wal_lsn_bytes",
            "komika_postgres_replication_sent_lsn_bytes",
            "komika_postgres_replication_flush_lsn_bytes",
            "komika_postgres_replication_replay_lsn_bytes",
            "komika_postgres_replication_replay_lag_seconds",
            "komika_postgres_replication_slot_retained_bytes",
            "komika_postgres_replication_slot_active",
        ] {
            assert!(body.contains(&format!("# HELP {name} ")));
            assert!(body.contains(&format!("# TYPE {name} gauge\n")));
            assert!(!body.lines().any(|line| line.starts_with(name)), "{name}");
        }
        assert_eq!(
            sample(
                &body,
                "komika_postgres_metric_available{metric=\"replication\"}"
            ),
            Some("0")
        );
        assert!(!body.contains("-1"));
    }

    #[test]
    fn replication_readings_render_with_escaped_labels_and_idle_lag() {
        let mut metrics = RecoveryMetrics {
            replication: Some(ReplicationStats {
                current_wal_lsn_bytes: 600,
                standbys: vec![
                    StandbyStats {
                        application_name: "node-\"a\\\n".into(),
                        state: "streaming".into(),
                        sent_lsn_bytes: Some(500),
                        flush_lsn_bytes: Some(500),
                        replay_lsn_bytes: Some(500),
                        replay_lag_seconds: None,
                    },
                    StandbyStats {
                        application_name: "node-b".into(),
                        state: "catchup".into(),
                        sent_lsn_bytes: Some(500),
                        flush_lsn_bytes: None,
                        replay_lsn_bytes: Some(400),
                        replay_lag_seconds: Some(1.25),
                    },
                ],
                slots: vec![ReplicationSlotStats {
                    slot_name: "slot-\"a\\\n".into(),
                    active: true,
                    wal_status: Some("reserved".into()),
                    retained_bytes: Some(200),
                }],
            }),
            ..Default::default()
        };
        let body = prometheus(&metrics);
        assert_eq!(
            sample(&body, "komika_postgres_replication_standbys"),
            Some("2")
        );
        assert_eq!(
            sample(&body, "komika_postgres_current_wal_lsn_bytes"),
            Some("600")
        );
        assert_eq!(
            sample(
                &body,
                "komika_postgres_metric_available{metric=\"replication\"}"
            ),
            Some("1")
        );
        for metric in ["sent", "flush", "replay"] {
            let name = format!(
                r#"komika_postgres_replication_{metric}_lsn_bytes{{standby="node-\"a\\\n"}}"#
            );
            assert_eq!(sample(&body, &name), Some("500"));
        }
        assert_eq!(
            sample(
                &body,
                r#"komika_postgres_replication_replay_lag_seconds{standby="node-\"a\\\n"}"#
            ),
            Some("0")
        );
        assert_eq!(
            sample(
                &body,
                "komika_postgres_replication_sent_lsn_bytes{standby=\"node-b\"}"
            ),
            Some("500")
        );
        assert_eq!(
            sample(
                &body,
                "komika_postgres_replication_flush_lsn_bytes{standby=\"node-b\"}"
            ),
            None
        );
        assert_eq!(
            sample(
                &body,
                "komika_postgres_replication_replay_lsn_bytes{standby=\"node-b\"}"
            ),
            Some("400")
        );
        let lag = "komika_postgres_replication_replay_lag_seconds{standby=\"node-b\"}";
        assert_eq!(sample(&body, lag), Some("1.25"));
        assert_eq!(
            sample(
                &body,
                r#"komika_postgres_replication_slot_retained_bytes{slot="slot-\"a\\\n"}"#
            ),
            Some("200")
        );
        let active = r#"komika_postgres_replication_slot_active{slot="slot-\"a\\\n"}"#;
        assert_eq!(sample(&body, active), Some("1"));

        metrics.replication.as_mut().unwrap().standbys[1].replay_lag_seconds = None;
        metrics.replication.as_mut().unwrap().slots[0].active = false;
        metrics.replication.as_mut().unwrap().slots[0].retained_bytes = None;
        let body = prometheus(&metrics);
        assert_eq!(sample(&body, lag), None);
        assert_eq!(sample(&body, active), Some("0"));
        assert_eq!(
            sample(
                &body,
                r#"komika_postgres_replication_slot_retained_bytes{slot="slot-\"a\\\n"}"#
            ),
            None
        );
    }

    #[test]
    fn readable_replication_without_standbys_renders_zero() {
        let body = prometheus(&RecoveryMetrics {
            replication: Some(ReplicationStats::default()),
            ..Default::default()
        });
        assert_eq!(
            sample(&body, "komika_postgres_replication_standbys"),
            Some("0")
        );
        assert_eq!(
            sample(
                &body,
                "komika_postgres_metric_available{metric=\"replication\"}"
            ),
            Some("1")
        );
    }

    #[test]
    fn null_replay_lag_requires_known_equal_lsns() {
        for (sent, replay, expected) in [
            (Some(500), Some(500), Some(0.0)),
            (Some(500), Some(400), None),
            (None, Some(500), None),
            (Some(500), None, None),
            (None, None, None),
        ] {
            assert_eq!(
                StandbyStats {
                    sent_lsn_bytes: sent,
                    replay_lsn_bytes: replay,
                    ..Default::default()
                }
                .replay_lag_seconds(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn replication_sql_parses_lsn_lag_and_excludes_backup() {
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let mut conn = pool.read().acquire().await.unwrap();
        let sql = replication_sql(
            "(VALUES ('node-a', 'streaming', '0/3000148'::pg_lsn, '0/3000120'::pg_lsn, \
             '0/3000100'::pg_lsn, '0/3000080'::pg_lsn, interval '1.25 seconds'), \
             ('node-starting', 'startup', NULL, NULL, NULL, NULL, NULL), \
             ('basebackup', 'backup', '0/3000148'::pg_lsn, NULL, NULL, NULL, NULL)) \
             AS r(application_name, state, sent_lsn, write_lsn, flush_lsn, replay_lsn, replay_lag)",
        );
        let stats = read_replication_stats(&mut conn, &sql).await.unwrap();
        drop(conn);
        pool.close().await;
        assert!(stats.current_wal_lsn_bytes >= 0);
        assert_eq!(stats.standbys.len(), 2);
        assert_eq!(
            stats
                .standbys
                .iter()
                .find(|s| s.application_name == "node-a"),
            Some(&StandbyStats {
                application_name: "node-a".into(),
                state: "streaming".into(),
                sent_lsn_bytes: Some(50_331_976),
                flush_lsn_bytes: Some(50_331_904),
                replay_lsn_bytes: Some(50_331_776),
                replay_lag_seconds: Some(1.25),
            })
        );
        assert_eq!(
            stats
                .standbys
                .iter()
                .find(|s| s.application_name == "node-starting"),
            Some(&StandbyStats {
                application_name: "node-starting".into(),
                state: "startup".into(),
                ..Default::default()
            })
        );
        assert!(stats.standbys.iter().all(|s| s.state != "backup"));
    }

    #[tokio::test]
    async fn replication_requires_stats_role_and_empty_primary_is_readable() {
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let mut conn = pool.read().acquire().await.unwrap();
        let (in_recovery, granted): (bool, bool) = sqlx::query_as(REPLICATION_ACCESS_SQL)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert!(!in_recovery);
        let initial = collect_on(&mut conn).await;
        assert_eq!(
            initial.replication.as_ref().map(|r| r.standbys.len()),
            granted.then_some(0)
        );

        let role = format!(
            "komika_replication_metrics_{}",
            uuid::Uuid::new_v4().simple()
        );
        sqlx::query(&format!("CREATE ROLE \"{role}\" NOLOGIN"))
            .execute(pool.write())
            .await
            .unwrap();
        sqlx::query(&format!("SET ROLE \"{role}\""))
            .execute(&mut *conn)
            .await
            .unwrap();
        let (denied, counted) =
            crate::db::test_counters::scope(async { collect_on(&mut conn).await }).await;
        sqlx::query(&format!("GRANT pg_read_all_stats TO \"{role}\""))
            .execute(pool.write())
            .await
            .unwrap();
        let regranted = collect_on(&mut conn).await;
        sqlx::query("RESET ROLE").execute(&mut *conn).await.unwrap();
        drop(conn);
        sqlx::query(&format!("DROP ROLE \"{role}\""))
            .execute(pool.write())
            .await
            .unwrap();
        pool.close().await;

        assert_eq!(denied.replication, None);
        assert_eq!(counted, Default::default());
        assert!(regranted.replication.unwrap().standbys.is_empty());
    }

    /// A missing archive time stays absent; a future one (clock skew) reads 0.
    #[tokio::test]
    async fn null_archive_time_is_absent_not_zero() {
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let sql = concat!(
            "SELECT ",
            age_seconds!("t.never"),
            ", ",
            age_seconds!("t.past"),
            ", ",
            age_seconds!("t.future"),
            " FROM (VALUES (NULL::timestamptz, now() - interval '90.6 seconds', \
             now() + interval '1 hour')) t(never, past, future)"
        );
        let ages: (Option<i64>, Option<i64>, Option<i64>) =
            sqlx::query_as(sql).fetch_one(pool.read()).await.unwrap();
        pool.close().await;
        assert_eq!(ages, (None, Some(90), Some(0)));
        assert!(STATS_SQL.contains(age_seconds!("a.last_failed_time")));
    }

    /// A role without the grants (as the hosted application role was) reads the
    /// archiver statistics, omits both directory values and never calls the
    /// functions; a direct call is denied and is not counted as a database error.
    #[tokio::test]
    async fn role_without_directory_grants_omits_wal_and_backlog() {
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let mut conn = pool.read().acquire().await.unwrap();
        let granted = collect_on(&mut conn).await;
        assert!(granted.wal_dir_bytes.is_some_and(|b| b >= 0));
        assert!(granted.archive_ready_files.is_some_and(|n| n >= 0));
        assert!(granted.archiver.is_some());

        let role = format!("komika_metrics_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE ROLE \"{role}\" NOLOGIN"))
            .execute(pool.write())
            .await
            .unwrap();
        sqlx::query(&format!("SET ROLE \"{role}\""))
            .execute(&mut *conn)
            .await
            .unwrap();
        let (denied, counted) =
            crate::db::test_counters::scope(async { collect_on(&mut conn).await }).await;
        let (forced, forced_counted) =
            crate::db::test_counters::scope(async { read_dirs(&mut conn, true, false).await })
                .await;
        // The least-privilege grant the hosted stack adds is enough.
        sqlx::query(&format!(
            "GRANT EXECUTE ON FUNCTION pg_catalog.pg_ls_waldir(), \
             pg_catalog.pg_ls_archive_statusdir() TO \"{role}\""
        ))
        .execute(pool.write())
        .await
        .unwrap();
        let regranted = collect_on(&mut conn).await;
        sqlx::query("RESET ROLE").execute(&mut *conn).await.unwrap();
        drop(conn);
        sqlx::query(&format!(
            "REVOKE EXECUTE ON FUNCTION pg_catalog.pg_ls_waldir(), \
             pg_catalog.pg_ls_archive_statusdir() FROM \"{role}\""
        ))
        .execute(pool.write())
        .await
        .unwrap();
        sqlx::query(&format!("DROP ROLE \"{role}\""))
            .execute(pool.write())
            .await
            .unwrap();
        pool.close().await;

        assert_eq!(denied.wal_dir_bytes, None);
        assert_eq!(denied.archive_ready_files, None);
        assert!(denied.archiver.is_some(), "pg_stat_archiver needs no grant");
        assert_eq!(counted, Default::default());
        assert_eq!(forced, (None, None));
        assert_eq!(forced_counted, Default::default());
        assert!(regranted.wal_dir_bytes.is_some_and(|b| b >= 0));
        assert!(regranted.archive_ready_files.is_some_and(|n| n >= 0));
        let body = prometheus(&denied);
        assert_eq!(sample(&body, "komika_sqlite_wal_bytes"), None);
        assert_eq!(
            sample(
                &body,
                "komika_postgres_metric_available{metric=\"wal_bytes\"}"
            ),
            Some("0")
        );
        assert!(sample(&body, "komika_postgres_archived_total").is_some());
    }
}
