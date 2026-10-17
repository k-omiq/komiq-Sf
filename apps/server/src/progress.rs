//! Reading progress on a work's chapter list. Copy rows keep their own resume positions.
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::catalog::work_chapter::EntryMove;
use crate::db::Database;

#[derive(Debug)]
pub enum WorkReadState {
    /// No viewer, no work, or a query failure: only the copy's own row applies.
    Empty,
    NotReady {
        keys: HashSet<String>,
        revision: i64,
        through: Option<String>,
    },
    Ready {
        entries: HashMap<String, bool>,
        copies: HashMap<(String, String), bool>,
        revision: i64,
        through: Option<String>,
    },
}

impl WorkReadState {
    pub fn copy_read(
        &self,
        source_type: &str,
        external_id: &str,
        spine_key: &str,
        own: bool,
    ) -> bool {
        match self {
            Self::Empty => own,
            Self::NotReady { keys, .. } => own || keys.contains(spine_key),
            Self::Ready { copies, .. } => copies
                .get(&(source_type.to_owned(), external_id.to_owned()))
                .copied()
                .unwrap_or(own),
        }
    }

    pub fn row_read(&self, entry_id: Option<&str>, key: &str) -> bool {
        match self {
            Self::Empty => false,
            Self::NotReady { keys, .. } => keys.contains(key),
            Self::Ready { entries, .. } => entry_id
                .and_then(|id| entries.get(id))
                .copied()
                .unwrap_or(false),
        }
    }

    pub fn viewer(&self) -> Option<(i64, Option<String>)> {
        match self {
            Self::Empty => None,
            Self::NotReady {
                revision, through, ..
            }
            | Self::Ready {
                revision, through, ..
            } => Some((*revision, through.clone())),
        }
    }
}

#[derive(sqlx::FromRow)]
struct ReadRow {
    kind: String,
    id: Option<String>,
    source_type: Option<String>,
    external_id: Option<String>,
    read: bool,
    revision: i64,
    through: Option<String>,
}

/// One snapshot, including readiness: a drain cannot switch the rule halfway through a read.
pub async fn work_read_state(
    pool: &Database,
    user_id: Option<&str>,
    work_id: &str,
) -> WorkReadState {
    let Some(uid) = user_id.and_then(|id| crate::db_types::uuid(id).ok()) else {
        return WorkReadState::Empty;
    };
    if work_id.is_empty() {
        return WorkReadState::Empty;
    }
    let rows: Result<Vec<ReadRow>, _> = sqlx::query_as(
        "WITH state AS ( \
             SELECT COALESCE(s.ready, false) AS ready, COALESCE(p.revision, 0) AS revision, \
                    p.through_entry_id AS through, pe.sort_key AS through_sort \
               FROM work w LEFT JOIN work_chapter_state s ON s.work_id = w.id \
               LEFT JOIN work_progress p ON p.work_id = w.id AND p.user_id = $1 \
               LEFT JOIN work_chapter pe ON pe.id = p.through_entry_id \
              WHERE w.id = $2 \
         ), keys AS ( \
             SELECT c.chapter_key \
               FROM canonical_progress cp \
               JOIN source_series ss ON ss.work_id = cp.work_id AND ss.source_type = 'mangadex' \
               JOIN chapter c ON c.source_series_id = ss.id AND c.external_id = cp.chapter_id \
              WHERE cp.user_id = $1 AND cp.work_id = $2 AND cp.read = TRUE \
                AND c.lang = 'en' AND c.chapter_key IS NOT NULL \
             UNION \
             SELECT c.chapter_key \
               FROM suwayomi_progress sp \
               JOIN source_series ss ON ss.source_type = 'suwayomi' \
                    AND ss.source_key = sp.series_id AND ss.work_id = $2 \
               JOIN chapter c ON c.source_series_id = ss.id AND c.external_id = sp.chapter_id \
              WHERE sp.user_id = $1 AND sp.read = TRUE AND c.chapter_key IS NOT NULL \
         ), explicit AS ( \
             SELECT l.work_chapter_id AS id FROM source_series ss \
               JOIN chapter c ON c.source_series_id = ss.id \
               JOIN work_chapter_link l ON l.chapter_id = c.id \
               JOIN canonical_progress cp ON ss.source_type = 'mangadex' \
                AND cp.chapter_id = c.external_id AND cp.work_id = $2 AND cp.user_id = $1 AND cp.read \
              WHERE ss.work_id = $2 \
             UNION \
             SELECT l.work_chapter_id FROM source_series ss \
               JOIN chapter c ON c.source_series_id = ss.id \
               JOIN work_chapter_link l ON l.chapter_id = c.id \
               JOIN suwayomi_progress sp ON ss.source_type = 'suwayomi' \
                AND sp.chapter_id = c.external_id AND sp.series_id = ss.source_key \
                AND sp.user_id = $1 AND sp.read WHERE ss.work_id = $2 \
         ), entries AS ( \
             SELECT e.id, COALESCE(m.read, x.id IS NOT NULL OR \
                    (s.through_sort IS NOT NULL AND e.sort_key <= s.through_sort AND NOT e.special)) AS read \
               FROM work_chapter e CROSS JOIN state s \
               LEFT JOIN work_chapter_mark m ON m.work_chapter_id = e.id AND m.user_id = $1 \
               LEFT JOIN explicit x ON x.id = e.id WHERE e.work_id = $2 AND s.ready \
         ) \
         SELECT 's'::text AS kind, NULL::text AS id, NULL::text AS source_type, \
                NULL::text AS external_id, s.ready AS read, s.revision, s.through FROM state s \
         UNION ALL SELECT 'k', k.chapter_key, NULL, NULL, true, 0, NULL \
           FROM keys k CROSS JOIN state s WHERE NOT s.ready \
         UNION ALL SELECT 'e', e.id, NULL, NULL, e.read, 0, NULL FROM entries e \
         UNION ALL SELECT 'c', e.id, ss.source_type, c.external_id, e.read, 0, NULL \
           FROM entries e JOIN work_chapter_link l ON l.work_chapter_id = e.id \
           JOIN chapter c ON c.id = l.chapter_id \
           JOIN source_series ss ON ss.id = c.source_series_id AND ss.work_id = $2",
    )
    .bind(uid)
    .bind(work_id)
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, work_id, "work_read_state query failed");
            return WorkReadState::Empty;
        }
    };
    let Some(state) = rows.iter().find(|r| r.kind == "s") else {
        return WorkReadState::Empty;
    };
    let (ready, revision, through) = (state.read, state.revision, state.through.clone());
    if !ready {
        return WorkReadState::NotReady {
            keys: rows
                .into_iter()
                .filter(|r| r.kind == "k")
                .filter_map(|r| r.id)
                .collect(),
            revision,
            through,
        };
    }
    let mut entries = HashMap::new();
    let mut copies = HashMap::new();
    for r in rows {
        match r.kind.as_str() {
            "e" => {
                entries.insert(r.id.unwrap(), r.read);
            }
            "c" => {
                copies.insert((r.source_type.unwrap(), r.external_id.unwrap()), r.read);
            }
            _ => {}
        }
    }
    WorkReadState::Ready {
        entries,
        copies,
        revision,
        through,
    }
}

pub async fn ready_library_progress(
    pool: &Database,
    user_id: &str,
) -> Result<HashMap<String, (i64, i64, Option<String>)>> {
    let rows = crate::db_types::query_tuple::<(String, i64, i64, Option<String>)>(
        "WITH library AS ( \
             SELECT DISTINCT COALESCE(ss.work_id, ul.series_id) AS work_id \
               FROM user_library ul LEFT JOIN source_series ss \
                 ON ss.source_type = 'suwayomi' AND ss.source_key = ul.series_id \
              WHERE ul.user_id = $1 \
         ), works AS ( \
             SELECT l.work_id, p.through_entry_id, pe.sort_key AS through_sort, p.updated_at \
               FROM library l JOIN work_chapter_state s ON s.work_id = l.work_id AND s.ready \
               LEFT JOIN work_progress p ON p.work_id = l.work_id AND p.user_id = $1 \
               LEFT JOIN work_chapter pe ON pe.id = p.through_entry_id \
         ), copies AS ( \
             SELECT ss.work_id, l.work_chapter_id AS entry_id, cp.updated_at \
               FROM works w JOIN source_series ss ON ss.work_id = w.work_id AND ss.source_type = 'mangadex' \
               JOIN chapter c ON c.source_series_id = ss.id \
               LEFT JOIN work_chapter_link l ON l.chapter_id = c.id \
               JOIN canonical_progress cp ON cp.chapter_id = c.external_id AND cp.work_id = w.work_id \
                AND cp.user_id = $1 AND cp.read \
             UNION ALL \
             SELECT ss.work_id, l.work_chapter_id, sp.updated_at \
               FROM works w JOIN source_series ss ON ss.work_id = w.work_id AND ss.source_type = 'suwayomi' \
               JOIN chapter c ON c.source_series_id = ss.id \
               LEFT JOIN work_chapter_link l ON l.chapter_id = c.id \
               JOIN suwayomi_progress sp ON sp.chapter_id = c.external_id AND sp.series_id = ss.source_key \
                AND sp.user_id = $1 AND sp.read \
         ), counts AS ( \
             SELECT w.work_id, COUNT(e.id) FILTER (WHERE COALESCE(m.read, \
                      EXISTS(SELECT 1 FROM copies c WHERE c.entry_id = e.id) OR \
                      (w.through_sort IS NOT NULL AND e.sort_key <= w.through_sort AND NOT e.special))) AS read_count, \
                    COUNT(e.id) AS total \
               FROM works w LEFT JOIN work_chapter e ON e.work_id = w.work_id AND NOT e.hidden \
                AND EXISTS(SELECT 1 FROM work_chapter_link l WHERE l.work_chapter_id = e.id) \
               LEFT JOIN work_chapter_mark m ON m.work_chapter_id = e.id AND m.user_id = $1 \
              GROUP BY w.work_id \
         ) SELECT n.work_id, n.read_count, n.total, \
                  GREATEST((SELECT MAX(c.updated_at) FROM copies c WHERE c.work_id = w.work_id), \
                           CASE WHEN w.through_entry_id IS NOT NULL THEN w.updated_at END) \
             FROM counts n JOIN works w ON w.work_id = n.work_id",
    )
    .bind(crate::db_types::uuid(user_id)?)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(w, r, t, at)| (w, (r, t, at)))
        .collect())
}

// Both deletion paths use the same atomic statement. A conflicting false mark wins,
// including one committed while this statement was waiting on its unique index.
const KEEP_EXPLICIT_SQL: &str = "WITH disappearing AS ( \
    SELECT c.id, l.work_chapter_id AS entry_id FROM chapter c \
      JOIN source_series ss ON ss.id = c.source_series_id \
      JOIN work_chapter_link l ON l.chapter_id = c.id \
     WHERE ($1::text[] IS NOT NULL AND c.id = ANY($1)) \
        OR ($2::text IS NOT NULL AND ss.source_type = $2) \
), copies AS ( \
    SELECT c.id, c.external_id, ss.source_type, ss.source_key, ss.work_id, l.work_chapter_id AS entry_id \
      FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
      JOIN work_chapter_link l ON l.chapter_id = c.id \
     WHERE l.work_chapter_id IN (SELECT entry_id FROM disappearing) \
), reads AS ( \
    SELECT c.id, c.entry_id, cp.user_id FROM copies c JOIN canonical_progress cp \
      ON c.source_type = 'mangadex' AND cp.chapter_id = c.external_id \
     AND cp.work_id = c.work_id AND cp.read \
    UNION \
    SELECT c.id, c.entry_id, sp.user_id FROM copies c JOIN suwayomi_progress sp \
      ON c.source_type = 'suwayomi' AND sp.chapter_id = c.external_id \
     AND sp.series_id = c.source_key AND sp.read \
) INSERT INTO work_chapter_mark (user_id, work_chapter_id, read, created_at) \
  SELECT DISTINCT r.user_id, e.id, true, now() FROM reads r \
    JOIN disappearing d ON d.id = r.id JOIN work_chapter e ON e.id = r.entry_id \
    LEFT JOIN work_progress p ON p.user_id = r.user_id AND p.work_id = e.work_id \
    LEFT JOIN work_chapter pe ON pe.id = p.through_entry_id \
   WHERE NOT EXISTS(SELECT 1 FROM work_chapter_mark m \
                     WHERE m.user_id = r.user_id AND m.work_chapter_id = e.id) \
     AND NOT (pe.sort_key IS NOT NULL AND e.sort_key <= pe.sort_key AND NOT e.special) \
     AND NOT EXISTS(SELECT 1 FROM reads other WHERE other.user_id = r.user_id \
                     AND other.entry_id = e.id AND NOT EXISTS \
                         (SELECT 1 FROM disappearing gone WHERE gone.id = other.id)) \
  ON CONFLICT DO NOTHING";

pub async fn keep_explicit_reads(conn: &mut PgConnection, chapter_ids: &[String]) -> Result<u64> {
    Ok(sqlx::query(KEEP_EXPLICIT_SQL)
        .bind(chapter_ids)
        .bind(None::<&str>)
        .execute(conn)
        .await?
        .rows_affected())
}

/// Whole store variant for the engine reset (`source_type` = 'suwayomi').
pub async fn keep_explicit_reads_of_store(
    conn: &mut PgConnection,
    source_type: &str,
) -> Result<u64> {
    Ok(sqlx::query(KEEP_EXPLICIT_SQL)
        .bind(None::<&[String]>)
        .bind(source_type)
        .execute(conn)
        .await?
        .rows_affected())
}

pub async fn affected_users(conn: &mut PgConnection, works: &[String]) -> Result<Vec<String>> {
    Ok(crate::db_types::query_scalar::<String>(
        "SELECT user_id FROM work_progress WHERE work_id = ANY($1) \
         UNION SELECT m.user_id FROM work_chapter_mark m \
           JOIN work_chapter e ON e.id = m.work_chapter_id WHERE e.work_id = ANY($1) \
         UNION SELECT user_id FROM canonical_progress WHERE work_id = ANY($1) \
         UNION SELECT sp.user_id FROM suwayomi_progress sp JOIN source_series ss \
           ON ss.source_type = 'suwayomi' AND ss.source_key = sp.series_id WHERE ss.work_id = ANY($1) \
         UNION SELECT u.user_id FROM progress_untick u JOIN chapter c ON c.external_id = u.chapter_id \
           JOIN source_series ss ON ss.id = c.source_series_id WHERE ss.work_id = ANY($1) \
         ORDER BY user_id",
    ).bind(works).fetch_all(conn).await?)
}

pub async fn convert_unticks_on(conn: &mut PgConnection, work_id: &str) -> Result<u64> {
    convert_unticks(conn, Some(work_id), None).await
}

/// Before a store's copies and untick rows go (a new engine generation): on works
/// that are not ready, the unticks of linked copies become unread marks, so the
/// pointer cannot cover them once the drain makes the work ready.
pub async fn keep_unticks_of_store(conn: &mut PgConnection, source_type: &str) -> Result<u64> {
    convert_unticks(conn, None, Some(source_type)).await
}

async fn convert_unticks(
    conn: &mut PgConnection,
    work_id: Option<&str>,
    source_type: Option<&str>,
) -> Result<u64> {
    let converted: i64 = sqlx::query_scalar(
        "WITH matched AS ( \
             SELECT DISTINCT u.user_id, u.chapter_id, l.work_chapter_id, u.created_at FROM progress_untick u \
               JOIN chapter c ON c.external_id = u.chapter_id \
               JOIN source_series ss ON ss.id = c.source_series_id \
                AND ($1::text IS NULL OR ss.work_id = $1) AND ($2::text IS NULL OR ss.source_type = $2) \
               /* A store-wide pass skips ready works: their unticks are already marks or stale. */ \
               AND ($1::text IS NOT NULL OR NOT EXISTS ( \
                   SELECT 1 FROM work_chapter_state s WHERE s.work_id = ss.work_id AND s.ready)) \
                AND ss.source_type = CASE WHEN u.chapter_id ~ '^[0-9]+$' THEN 'suwayomi' ELSE 'mangadex' END \
               JOIN work_chapter_link l ON l.chapter_id = c.id \
         ), marks AS ( \
             INSERT INTO work_chapter_mark (user_id, work_chapter_id, read, created_at) \
             SELECT user_id, work_chapter_id, false, max(created_at) FROM matched \
              GROUP BY user_id, work_chapter_id \
             /* The later of the untick and an existing mark wins: a merge or split that \
                kept a read after the untick keeps it. */ \
             ON CONFLICT (user_id, work_chapter_id) DO UPDATE SET read = false, created_at = excluded.created_at \
              WHERE work_chapter_mark.created_at <= excluded.created_at RETURNING 1 \
         ), removed AS ( \
             DELETE FROM progress_untick u USING matched m \
              WHERE u.user_id = m.user_id AND u.chapter_id = m.chapter_id RETURNING 1 \
         ) SELECT COUNT(*) FROM removed",
    ).bind(work_id).bind(source_type).fetch_one(conn).await?;
    Ok(converted as u64)
}

/// Include unlinked copies: the drain discovers users before linking those copies.
pub async fn untick_users(conn: &mut PgConnection, work_id: &str) -> Result<Vec<String>> {
    Ok(crate::db_types::query_scalar::<String>(
        "SELECT DISTINCT u.user_id FROM progress_untick u \
           JOIN chapter c ON c.external_id = u.chapter_id \
           JOIN source_series ss ON ss.id = c.source_series_id AND ss.work_id = $1 \
            AND ss.source_type = CASE WHEN u.chapter_id ~ '^[0-9]+$' THEN 'suwayomi' ELSE 'mangadex' END \
          ORDER BY u.user_id",
    ).bind(work_id).fetch_all(conn).await?)
}

#[derive(sqlx::FromRow)]
struct Entry {
    id: String,
    work_id: String,
    sort_key: String,
    special: bool,
    ready: bool,
    keys: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct CopyProgress {
    user_id: Uuid,
    work_id: String,
    chapter_id: String,
    entry_id: Option<String>,
    chapter_key: Option<String>,
    read: bool,
    fold: bool,
}

type UserWork = (Uuid, String);
type UserEntry = (Uuid, String);
type Pointer = (Option<String>, Option<String>, i64);

/// A transaction-local snapshot. Callers hold user locks before taking work locks.
pub struct Before {
    users: Vec<Uuid>,
    entries: HashMap<String, Entry>,
    pointers: HashMap<UserWork, Pointer>,
    marks: HashMap<UserEntry, bool>,
    explicit: HashSet<UserEntry>,
    own_true: HashSet<(Uuid, String)>,
    had_state: HashSet<UserWork>,
    effective: HashMap<UserEntry, bool>,
}

pub async fn keep_state_before(
    conn: &mut PgConnection,
    users: &[String],
    works: &[String],
) -> Result<Before> {
    let mut users = users
        .iter()
        .map(|u| crate::db_types::uuid(u))
        .collect::<Result<Vec<_>>>()?;
    users.sort_unstable();
    users.dedup();
    // Five bulk queries, independent of the number of readers or entries.
    let entries: Vec<Entry> = sqlx::query_as(
        "SELECT e.id, e.work_id, e.sort_key::text, e.special, COALESCE(s.ready, false) AS ready, \
                ARRAY(SELECT c.chapter_key FROM work_chapter_link l JOIN chapter c ON c.id = l.chapter_id \
                       WHERE l.work_chapter_id = e.id AND c.chapter_key IS NOT NULL) AS keys \
           FROM work_chapter e LEFT JOIN work_chapter_state s ON s.work_id = e.work_id \
          WHERE e.work_id = ANY($1)",
    ).bind(works).fetch_all(&mut *conn).await?;
    let progress: Vec<(Uuid, String, Option<String>, Option<String>, i64)> = sqlx::query_as(
        "SELECT p.user_id, p.work_id, p.through_entry_id, e.sort_key::text, p.revision \
           FROM work_progress p LEFT JOIN work_chapter e ON e.id = p.through_entry_id \
          WHERE p.user_id = ANY($1) AND p.work_id = ANY($2)",
    )
    .bind(&users)
    .bind(works)
    .fetch_all(&mut *conn)
    .await?;
    let marks: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT m.user_id, m.work_chapter_id, m.read FROM work_chapter_mark m \
           JOIN work_chapter e ON e.id = m.work_chapter_id \
          WHERE m.user_id = ANY($1) AND e.work_id = ANY($2)",
    )
    .bind(&users)
    .bind(works)
    .fetch_all(&mut *conn)
    .await?;
    let copies: Vec<CopyProgress> = sqlx::query_as(
        "SELECT cp.user_id, ss.work_id, c.id AS chapter_id, l.work_chapter_id AS entry_id, \
                c.chapter_key, cp.read, COALESCE(c.lang = 'en', false) AS fold \
           FROM canonical_progress cp JOIN source_series ss \
             ON ss.work_id = cp.work_id AND ss.source_type = 'mangadex' \
           JOIN chapter c ON c.source_series_id = ss.id AND c.external_id = cp.chapter_id \
           LEFT JOIN work_chapter_link l ON l.chapter_id = c.id \
          WHERE cp.user_id = ANY($1) AND cp.work_id = ANY($2) \
         UNION ALL \
         SELECT sp.user_id, ss.work_id, c.id, l.work_chapter_id, c.chapter_key, sp.read, true \
           FROM suwayomi_progress sp JOIN source_series ss \
             ON ss.source_type = 'suwayomi' AND ss.source_key = sp.series_id \
           JOIN chapter c ON c.source_series_id = ss.id AND c.external_id = sp.chapter_id \
           LEFT JOIN work_chapter_link l ON l.chapter_id = c.id \
          WHERE sp.user_id = ANY($1) AND ss.work_id = ANY($2)",
    )
    .bind(&users)
    .bind(works)
    .fetch_all(&mut *conn)
    .await?;
    // A page row can outlive its mirrored copy. It still needs a revision fence
    // when its work changes identity, even though it reads no entry in this snapshot.
    let holders: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT user_id, work_id FROM canonical_progress WHERE user_id = ANY($1) AND work_id = ANY($2) \
         UNION SELECT sp.user_id, ss.work_id FROM suwayomi_progress sp JOIN source_series ss \
           ON ss.source_type = 'suwayomi' AND ss.source_key = sp.series_id \
          WHERE sp.user_id = ANY($1) AND ss.work_id = ANY($2)",
    ).bind(&users).bind(works).fetch_all(&mut *conn).await?;
    let mut out = Before {
        users,
        entries: entries.into_iter().map(|e| (e.id.clone(), e)).collect(),
        pointers: HashMap::new(),
        marks: HashMap::new(),
        explicit: HashSet::new(),
        own_true: HashSet::new(),
        had_state: holders.into_iter().collect(),
        effective: HashMap::new(),
    };
    for (user, work, through, sort, revision) in progress {
        out.had_state.insert((user, work.clone()));
        out.pointers.insert((user, work), (through, sort, revision));
    }
    for (user, entry, read) in marks {
        if let Some(e) = out.entries.get(&entry) {
            out.had_state.insert((user, e.work_id.clone()));
        }
        out.marks.insert((user, entry), read);
    }
    let mut keys: HashSet<(Uuid, String, String)> = HashSet::new();
    for copy in copies {
        out.had_state.insert((copy.user_id, copy.work_id.clone()));
        if copy.read {
            out.own_true.insert((copy.user_id, copy.chapter_id));
            if let Some(entry) = copy.entry_id {
                out.explicit.insert((copy.user_id, entry));
            }
            if copy.fold {
                if let Some(key) = copy.chapter_key {
                    keys.insert((copy.user_id, copy.work_id, key));
                }
            }
        }
    }
    for user in &out.users {
        for entry in out.entries.values() {
            let key = (*user, entry.id.clone());
            let read = if entry.ready {
                let sort = out
                    .pointers
                    .get(&(*user, entry.work_id.clone()))
                    .and_then(|p| p.1.as_deref());
                out.read_with_pointer(*user, entry, sort)
            } else {
                out.explicit.contains(&key)
                    || entry
                        .keys
                        .iter()
                        .any(|k| keys.contains(&(*user, entry.work_id.clone(), k.clone())))
            };
            out.effective.insert(key, read);
        }
    }
    Ok(out)
}

impl Before {
    fn pointer(&self, user: Uuid, work: &str) -> Option<&str> {
        self.pointers
            .get(&(user, work.to_owned()))
            .and_then(|p| p.0.as_deref())
    }

    fn pointer_sort(&self, user: Uuid, work: &str) -> Option<&str> {
        self.pointers
            .get(&(user, work.to_owned()))
            .and_then(|p| p.1.as_deref())
    }

    fn read_with_pointer(&self, user: Uuid, entry: &Entry, sort: Option<&str>) -> bool {
        let key = (user, entry.id.clone());
        self.marks.get(&key).copied().unwrap_or_else(|| {
            self.explicit.contains(&key)
                || (!entry.special
                    && sort
                        .is_some_and(|p| compare_numeric(&entry.sort_key, p) != Ordering::Greater))
        })
    }
}

pub enum PointerPlan<'a> {
    Merge {
        survivor: &'a str,
        loser: &'a str,
    },
    Split {
        source: &'a str,
        dest: &'a str,
        dest_is_new: bool,
    },
    Edit {
        work: &'a str,
    },
}

/// Preserve every old entry's truth value even when a pointer gains new entries below it.
pub async fn keep_state_after(
    conn: &mut PgConnection,
    before: Before,
    moves: &[EntryMove],
    copy_moves: &[(String, String)],
    plan: PointerPlan<'_>,
) -> Result<()> {
    let works: Vec<String> = match &plan {
        PointerPlan::Merge { survivor, .. } => vec![(*survivor).to_owned()],
        PointerPlan::Split { source, dest, .. } => vec![(*source).to_owned(), (*dest).to_owned()],
        PointerPlan::Edit { work } => vec![(*work).to_owned()],
    };
    let users: Vec<String> = before.users.iter().map(ToString::to_string).collect();
    let current = keep_state_before(conn, &users, &works).await?;
    let targets = entry_targets(&before, &current, moves);
    let mut desired: HashMap<UserEntry, bool> = HashMap::new();
    for ((user, old), read) in &before.effective {
        if let Some(ids) = targets.get(old) {
            for id in ids {
                *desired.entry((*user, id.clone())).or_default() |= read;
            }
        }
    }
    for (copy, target) in copy_moves {
        if current.entries.contains_key(target) {
            for user in &before.users {
                if before.own_true.contains(&(*user, copy.clone())) {
                    desired.insert((*user, target.clone()), true);
                }
            }
        }
    }
    let mut mark_users = Vec::new();
    let mut mark_entries = Vec::new();
    let mut mark_reads = Vec::new();
    let mut progress_users = Vec::new();
    let mut progress_works = Vec::new();
    let mut progress_through: Vec<Option<String>> = Vec::new();
    let mut progress_origin: Vec<Option<String>> = Vec::new();
    let mut read_users = Vec::new();
    let mut read_entries = Vec::new();
    for user in &before.users {
        for work in &works {
            let through = planned_pointer(*user, work, &plan, &before, &current, &targets);
            let sort = through
                .as_ref()
                .and_then(|id| current.entries.get(id))
                .map(|e| e.sort_key.as_str());
            let inherited = match &plan {
                PointerPlan::Merge { loser, .. } => {
                    before.had_state.contains(&(*user, (*loser).to_owned()))
                }
                _ => false,
            };
            let mut had_state = inherited
                || before.had_state.contains(&(*user, work.clone()))
                || current.had_state.contains(&(*user, work.clone()))
                || through.is_some();
            for entry in current.entries.values().filter(|e| e.work_id == *work) {
                let Some(want) = desired.get(&(*user, entry.id.clone())) else {
                    continue;
                };
                had_state |= *want;
                if *want {
                    read_users.push(*user);
                    read_entries.push(entry.id.clone());
                }
                // New split destinations use the key fold until the drain. Store
                // compensation for their eventual entry rule in the meantime.
                if current.read_with_pointer(*user, entry, sort) != *want {
                    had_state = true;
                    mark_users.push(*user);
                    mark_entries.push(entry.id.clone());
                    mark_reads.push(*want);
                }
            }
            if had_state {
                progress_users.push(*user);
                progress_works.push(work.clone());
                progress_through.push(through);
                // The row whose last-read time a new row inherits.
                progress_origin.push(match &plan {
                    PointerPlan::Merge { loser, .. } => Some((*loser).to_owned()),
                    PointerPlan::Split { source, .. } if work.as_str() != *source => {
                        Some((*source).to_owned())
                    }
                    _ => None,
                });
            }
        }
    }
    if !mark_users.is_empty() {
        sqlx::query(
            "INSERT INTO work_chapter_mark (user_id, work_chapter_id, read, created_at) \
             SELECT u.user_id, u.entry_id, u.read, now() \
               FROM UNNEST($1::uuid[], $2::text[], $3::bool[]) AS u(user_id, entry_id, read) \
             ON CONFLICT (user_id, work_chapter_id) DO UPDATE SET read = EXCLUDED.read, created_at = EXCLUDED.created_at",
        )
        .bind(&mark_users)
        .bind(&mark_entries)
        .bind(&mark_reads)
        .execute(&mut *conn)
        .await?;
    }
    if !read_users.is_empty() {
        // A kept read must not be undone later by a pending legacy untick of one of
        // the entry's copies (the drain turns unticks into unread marks).
        sqlx::query(
            "DELETE FROM progress_untick u \
              USING UNNEST($1::uuid[], $2::text[]) AS d(user_id, entry_id), \
                    work_chapter_link l, chapter c, source_series ss \
              WHERE l.work_chapter_id = d.entry_id AND c.id = l.chapter_id \
                AND ss.id = c.source_series_id AND u.user_id = d.user_id \
                AND u.chapter_id = c.external_id \
                AND ss.source_type = CASE WHEN u.chapter_id ~ '^[0-9]+$' THEN 'suwayomi' ELSE 'mangadex' END",
        )
        .bind(&read_users)
        .bind(&read_entries)
        .execute(&mut *conn)
        .await?;
    }
    if !progress_users.is_empty() {
        // Only the revision moves. `updated_at` is the reader's last read, which orders
        // Continue reading: keep the row's own; a new row takes its origin's (the loser's
        // or the split source's), and a merge keeps the later of the two. A row with
        // neither has no pointer, so its time is never shown.
        sqlx::query(
            "INSERT INTO work_progress (user_id, work_id, through_entry_id, revision, updated_at) \
             SELECT u.user_id, u.work_id, u.through, 1, COALESCE(CASE WHEN $5 \
                      THEN GREATEST(own.updated_at, origin.updated_at) \
                      ELSE COALESCE(own.updated_at, origin.updated_at) END, now()) \
               FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::text[]) \
                    AS u(user_id, work_id, through, origin) \
               LEFT JOIN work_progress own ON own.user_id = u.user_id AND own.work_id = u.work_id \
               LEFT JOIN work_progress origin ON origin.user_id = u.user_id AND origin.work_id = u.origin \
             ON CONFLICT (user_id, work_id) DO UPDATE SET through_entry_id = EXCLUDED.through_entry_id, \
                 revision = work_progress.revision + 1, updated_at = EXCLUDED.updated_at",
        )
        .bind(&progress_users)
        .bind(&progress_works)
        .bind(&progress_through)
        .bind(&progress_origin)
        .bind(matches!(plan, PointerPlan::Merge { .. }))
        .execute(&mut *conn)
        .await?;
    }
    if let PointerPlan::Merge { loser, .. } = plan {
        sqlx::query("DELETE FROM work_progress WHERE work_id = $1 AND user_id = ANY($2)")
            .bind(loser)
            .bind(&before.users)
            .execute(conn)
            .await?;
    }
    Ok(())
}

fn entry_targets(
    before: &Before,
    current: &Before,
    moves: &[EntryMove],
) -> HashMap<String, Vec<String>> {
    let mut targets: HashMap<String, Vec<String>> = HashMap::new();
    for id in before.entries.keys() {
        if current.entries.contains_key(id) {
            targets.entry(id.clone()).or_default().push(id.clone());
        }
    }
    for m in moves {
        if current.entries.contains_key(&m.to) {
            targets
                .entry(m.from.clone())
                .or_default()
                .push(m.to.clone());
        }
    }
    for ids in targets.values_mut() {
        // Keep the caller's move order for pointer remapping; identity comes first.
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));
    }
    targets
}

fn planned_pointer(
    user: Uuid,
    work: &str,
    plan: &PointerPlan<'_>,
    before: &Before,
    current: &Before,
    targets: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let stays = |id: &str| current.entries.get(id).is_some_and(|e| e.work_id == work);
    let old = |w: &str| {
        before
            .pointer(user, w)
            .filter(|id| stays(id))
            .map(str::to_owned)
    };
    let mapped = |w: &str| {
        before
            .pointer(user, w)
            .and_then(|id| targets.get(id))
            .and_then(|ids| ids.iter().find(|id| stays(id)))
            .cloned()
    };
    let latest = |w: &str| {
        before.pointer_sort(user, w).and_then(|sort| {
            current
                .entries
                .values()
                .filter(|e| {
                    e.work_id == work && compare_numeric(&e.sort_key, sort) != Ordering::Greater
                })
                .max_by(|a, b| {
                    compare_numeric(&a.sort_key, &b.sort_key).then_with(|| a.id.cmp(&b.id))
                })
                .map(|e| e.id.clone())
        })
    };
    match plan {
        PointerPlan::Merge { survivor, loser } => old(survivor).or_else(|| mapped(loser)),
        PointerPlan::Split {
            source,
            dest,
            dest_is_new,
        } => {
            if work == *source {
                old(source).or_else(|| latest(source))
            } else if *dest_is_new {
                latest(source)
            } else {
                old(dest)
            }
        }
        PointerPlan::Edit { work } => old(work).or_else(|| mapped(work)),
    }
}

/// PostgreSQL NUMERIC sorts must stay exact even for tightly spaced special entries.
/// Reading its decimal text avoids converting those boundaries to rounded floats.
fn compare_numeric(a: &str, b: &str) -> Ordering {
    let rank = |s| match s {
        "-Infinity" => 0,
        "Infinity" => 2,
        "NaN" => 3,
        _ => 1,
    };
    let order = rank(a).cmp(&rank(b));
    if order != Ordering::Equal || rank(a) != 1 {
        return order;
    }
    let parts = |s: &str| {
        let negative = s.starts_with('-');
        let unsigned = s.trim_start_matches(['-', '+']);
        let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let integer = integer.trim_start_matches('0').to_owned();
        let fraction = fraction.trim_end_matches('0').to_owned();
        (
            negative && (!integer.is_empty() || !fraction.is_empty()),
            integer,
            fraction,
        )
    };
    let (an, ai, af) = parts(a);
    let (bn, bi, bf) = parts(b);
    match an.cmp(&bn) {
        Ordering::Greater => Ordering::Less,
        Ordering::Less => Ordering::Greater,
        Ordering::Equal => {
            let order = ai
                .len()
                .cmp(&bi.len())
                .then_with(|| ai.cmp(&bi))
                .then_with(|| af.cmp(&bf));
            if an {
                order.reverse()
            } else {
                order
            }
        }
    }
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
