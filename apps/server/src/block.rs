//! Content block rules (migration 0118): the series a server, or one user, does not want
//! listed.
//!
//! A [`BlockRule`] names tags, content ratings, or both. It matches a work that carries
//! ALL of its tags and, when it names ratings, has ONE of them. A viewer's [`BlockSet`] is
//! the site-wide rules plus their own, and a work is blocked when ANY rule matches it.
//!
//! This module owns three things:
//!
//! 1. [`BlockRule::new`] — the one place a rule is validated. Both vocabularies are closed
//!    (`tags::is_plausible_tag_id`, [`RATINGS`]), which is what makes point 3 safe.
//! 2. [`list`] / [`replace`] / [`load`] — the `content_block_rule` table.
//! 3. [`BlockSet::blocked_sql`] / [`blocked_ids`] — the gate. One SQL expression that is
//!    true for a blocked work, for the surfaces that list works from SQL, and one lookup
//!    for the surfaces that hold a list of work ids already.
//!
//! THE RULE VALUES ARE WRITTEN INTO THE SQL, NOT BOUND. Every gated query in the server
//! already has its own bind list, in its own order, and a rule set adds a variable number
//! of values in the middle of it. A [`BlockRule`] can only be built by [`BlockRule::new`],
//! which admits `[a-z0-9-]` tag ids and the four fixed rating words and nothing else, so
//! there is no value here that could close a string literal.
//!
//! WHAT A RATING MEANS HERE is the EFFECTIVE rating (`catalog::effective_content_rating_sql`):
//! a work the NSFW gate treats as adult is never below `erotica`, and a work with no
//! rating is `safe`. It is the rating Browse filters on, so a rule and the Browse filter
//! agree about every work.

use std::collections::HashSet;

use crate::database::Database;
use anyhow::Result;
use chrono::Utc;

/// The four content ratings, mildest first. The words `work.content_rating` stores.
pub const RATINGS: [&str; 4] = ["safe", "suggestive", "erotica", "pornographic"];

/// Most rules one scope (the site, or one user) can hold.
pub const MAX_RULES: usize = 50;

/// Most tags one rule can name. A rule needs every one of them on a work, so a long list
/// matches almost nothing.
pub const MAX_RULE_TAGS: usize = 8;

/// Why a rule was not accepted. The text is shown to the person who wrote the rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleError {
    Empty,
    BadTag(String),
    BadRating(String),
    TooManyTags,
    /// Every rating at once and no tag: the rule would block the whole catalogue.
    Everything,
    TooManyRules,
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuleError::Empty => write!(f, "A rule needs a tag or a content rating."),
            RuleError::BadTag(t) => write!(f, "\"{t}\" is not a tag."),
            RuleError::BadRating(r) => write!(f, "\"{r}\" is not a content rating."),
            RuleError::TooManyTags => {
                write!(f, "A rule can name at most {MAX_RULE_TAGS} tags.")
            }
            RuleError::Everything => write!(
                f,
                "A rule with every content rating and no tag would block everything."
            ),
            RuleError::TooManyRules => write!(f, "At most {MAX_RULES} rules."),
        }
    }
}

impl std::error::Error for RuleError {}

/// The EFFECTIVE content rating of the `work` row aliased `alias`, as SQL. What a rule's
/// ratings are compared with wherever the caller's row is a `work` (see the module
/// header), and what [`BlockSet::blocked_sql`] reads when the caller has no rating.
pub fn work_rating_sql(alias: &str) -> String {
    crate::catalog::effective_content_rating_sql(
        &format!("COALESCE({alias}.content_rating, 'safe')"),
        &format!("COALESCE({alias}.is_nsfw_override, {alias}.is_nsfw)"),
    )
}

/// One validated rule. The fields are private so that [`BlockRule::new`] is the only way
/// to make one: [`BlockSet::blocked_sql`] writes them into SQL as they are.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockRule {
    /// Tag ids, sorted, no repeats. The work must carry all of them.
    tags: Vec<String>,
    /// Rating words, in [`RATINGS`] order, no repeats. Empty: any rating.
    ratings: Vec<&'static str>,
}

impl BlockRule {
    /// Validate and canonicalize one rule. Order and repeats in the input do not matter.
    pub fn new<T, R>(tags: T, ratings: R) -> Result<Self, RuleError>
    where
        T: IntoIterator,
        T::Item: AsRef<str>,
        R: IntoIterator,
        R::Item: AsRef<str>,
    {
        let mut tag_ids: Vec<String> = Vec::new();
        for tag in tags {
            let tag = tag.as_ref().trim();
            if !crate::tags::is_plausible_tag_id(tag) {
                return Err(RuleError::BadTag(tag.to_string()));
            }
            tag_ids.push(tag.to_string());
        }
        tag_ids.sort();
        tag_ids.dedup();
        if tag_ids.len() > MAX_RULE_TAGS {
            return Err(RuleError::TooManyTags);
        }

        let mut wanted: HashSet<&'static str> = HashSet::new();
        for rating in ratings {
            let rating = rating.as_ref().trim();
            let word = RATINGS
                .iter()
                .find(|w| w.eq_ignore_ascii_case(rating))
                .ok_or_else(|| RuleError::BadRating(rating.to_string()))?;
            wanted.insert(word);
        }
        let ratings: Vec<&'static str> = RATINGS
            .iter()
            .copied()
            .filter(|w| wanted.contains(w))
            .collect();

        if tag_ids.is_empty() {
            if ratings.is_empty() {
                return Err(RuleError::Empty);
            }
            if ratings.len() == RATINGS.len() {
                return Err(RuleError::Everything);
            }
        }
        Ok(BlockRule {
            tags: tag_ids,
            ratings,
        })
    }

    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    pub fn ratings(&self) -> &[&'static str] {
        &self.ratings
    }

    /// The SQL condition for this rule. `rating` is an expression for the work's
    /// effective rating; `work_id` an expression for its id.
    fn sql(&self, work_id: &str, rating: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        // The rating first: it reads a column, and SQLite stops at the first false
        // operand of an AND, so a rule that names ratings probes `work_tag` only for
        // the works that have one of them.
        if !self.ratings.is_empty() {
            let words: Vec<String> = self.ratings.iter().map(|w| format!("'{w}'")).collect();
            parts.push(format!("({rating}) IN ({})", words.join(", ")));
        }
        // One covering probe of `idx_work_tag_work_key(work_id, tag_key)` per tag.
        for tag in &self.tags {
            debug_assert!(crate::tags::is_plausible_tag_id(tag));
            parts.push(format!(
                "EXISTS (SELECT 1 FROM work_tag bt WHERE bt.work_id = {work_id} \
                 AND bt.tag_key = '{tag}')"
            ));
        }
        format!("({})", parts.join(" AND "))
    }
}

/// The rules that apply to one viewer: the site's and their own. Sorted and without
/// repeats, so two viewers with the same rules have the same [`BlockSet::key`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockSet(Vec<BlockRule>);

impl BlockSet {
    pub fn new(mut rules: Vec<BlockRule>) -> Self {
        rules.sort();
        rules.dedup();
        BlockSet(rules)
    }

    /// No rule at all: nothing is blocked, and no gate adds SQL.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The set as text, for a memo key. Two sets have one key only when they block the
    /// same works: a rule is `tags/ratings`, rules are joined with `;`, and neither
    /// separator can occur in a tag id or a rating word.
    pub fn key(&self) -> String {
        self.0
            .iter()
            .map(|r| format!("{}/{}", r.tags.join(","), r.ratings.join(",")))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// A SQL condition that is TRUE for a blocked work, or `None` when the set is empty.
    ///
    /// `work_id` is an expression for the work's id in the caller's query (`f.work_id`,
    /// `w.id`). `rating` is an expression for its EFFECTIVE rating when the caller's row
    /// has one (`browse_catalogue.content_rating`); with `None` the work row is read for
    /// it, one primary-key probe per candidate row.
    pub fn blocked_sql(&self, work_id: &str, rating: Option<&str>) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let Some(rating) = rating else {
            // Read the rating from `work`. The rules then name `bw.id`, so the tag
            // probes are correlated to this one row.
            let any = self.any_rule_sql("bw.id", &work_rating_sql("bw"));
            return Some(format!(
                "EXISTS (SELECT 1 FROM work bw WHERE bw.id = {work_id} AND {any})"
            ));
        };
        Some(self.any_rule_sql(work_id, rating))
    }

    fn any_rule_sql(&self, work_id: &str, rating: &str) -> String {
        let rules: Vec<String> = self.0.iter().map(|r| r.sql(work_id, rating)).collect();
        format!("({})", rules.join(" OR "))
    }

    /// ` AND NOT <blocked>` for a WHERE clause, or the empty string. See
    /// [`BlockSet::blocked_sql`] for the two arguments.
    pub fn and_not_blocked(&self, work_id: &str, rating: Option<&str>) -> String {
        match self.blocked_sql(work_id, rating) {
            Some(sql) => format!(" AND NOT {sql}"),
            None => String::new(),
        }
    }
}

/// Whose rules: the site's, or one user's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    Site,
    User(&'a str),
}

fn parse_row(tags: &str, ratings: &str) -> Option<BlockRule> {
    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    };
    // A row this version cannot read (a rating word a later version added) is skipped,
    // not an error: one unreadable rule must not take every list down.
    BlockRule::new(split(tags), split(ratings))
        .inspect_err(
            |e| tracing::warn!(tags, ratings, error = %e, "content_block_rule row skipped"),
        )
        .ok()
}

/// One scope's rules, in the order they were saved.
pub async fn list(pool: &Database, scope: Scope<'_>) -> Result<Vec<BlockRule>> {
    let rows: Vec<(String, String)> =
        match scope {
            Scope::Site => sqlx::query_as(
                "SELECT tags, ratings FROM content_block_rule WHERE user_id IS NULL ORDER BY id",
            )
            .fetch_all(pool)
            .await?,
            Scope::User(user_id) => {
                sqlx::query_as(
                    "SELECT tags, ratings FROM content_block_rule WHERE user_id = ? ORDER BY id",
                )
                .bind(user_id)
                .fetch_all(pool)
                .await?
            }
        };
    Ok(rows
        .iter()
        .filter_map(|(tags, ratings)| parse_row(tags, ratings))
        .collect())
}

/// Replace one scope's rules with `rules` (repeats dropped, order kept). Atomic: a reader
/// sees the old list or the new one.
pub async fn replace(pool: &Database, scope: Scope<'_>, rules: &[BlockRule]) -> Result<()> {
    let mut unique: Vec<&BlockRule> = Vec::new();
    for rule in rules {
        if !unique.contains(&rule) {
            unique.push(rule);
        }
    }
    if unique.len() > MAX_RULES {
        return Err(RuleError::TooManyRules.into());
    }
    let user_id = match scope {
        Scope::Site => None,
        Scope::User(id) => Some(id),
    };
    let now = Utc::now().to_rfc3339();
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        // `IS ?` matches NULL to NULL, so one statement serves both scopes.
        sqlx::query("DELETE FROM content_block_rule WHERE user_id IS ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        for rule in &unique {
            sqlx::query(
                "INSERT INTO content_block_rule (user_id, tags, ratings, created_at) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(rule.tags.join(","))
            .bind(rule.ratings.join(","))
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    })
    .await
}

/// The rules that apply to a viewer: the site's, plus `user_id`'s own when signed in.
///
/// One small read per request and never memoized, like `scanlators::list_tags`: a rule an
/// admin just saved must hold on the next request, on every replica.
pub async fn load(pool: &Database, user_id: Option<&str>) -> Result<BlockSet> {
    let rows: Vec<(String, String)> = match user_id {
        Some(user_id) => {
            sqlx::query_as(
                "SELECT tags, ratings FROM content_block_rule \
                  WHERE user_id IS NULL OR user_id = ?",
            )
            .bind(user_id)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as("SELECT tags, ratings FROM content_block_rule WHERE user_id IS NULL")
                .fetch_all(pool)
                .await?
        }
    };
    Ok(BlockSet::new(
        rows.iter()
            .filter_map(|(tags, ratings)| parse_row(tags, ratings))
            .collect(),
    ))
}

/// Which of `work_ids` the set blocks. For a surface that already holds its list (a
/// memoized feed, a library): it drops these ids instead of rebuilding its query.
///
/// An id that is not a work (a numeric Suwayomi series id) is never in the answer.
pub async fn blocked_ids(
    pool: &Database,
    set: &BlockSet,
    work_ids: &[String],
) -> Result<HashSet<String>> {
    let Some(blocked) = set.blocked_sql("w.id", None) else {
        return Ok(HashSet::new());
    };
    let mut out = HashSet::new();
    // Under SQLite's bind limit, with room to spare.
    for chunk in work_ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let sql = format!("SELECT w.id FROM work w WHERE w.id IN ({marks}) AND {blocked}");
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        out.extend(query.fetch_all(pool).await?);
    }
    Ok(out)
}

/// Which of `series_ids` the set blocks, where an id is what a `Series` carries: a `w_`
/// work id, or the numeric id of a Suwayomi series (blocked when the work it is mapped
/// to is). A Suwayomi series with no work yet has no tags or rating to match, so it is
/// never blocked.
pub async fn blocked_series_ids(
    pool: &Database,
    set: &BlockSet,
    series_ids: &[String],
) -> Result<HashSet<String>> {
    let Some(blocked) = set.blocked_sql("ss.work_id", None) else {
        return Ok(HashSet::new());
    };
    let (works, engine): (Vec<String>, Vec<String>) = series_ids
        .iter()
        .cloned()
        .partition(|id| id.starts_with("w_"));
    let mut out = blocked_ids(pool, set, &works).await?;
    for chunk in engine.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT ss.source_key FROM source_series ss \
              WHERE ss.source_type = 'suwayomi' AND ss.source_key IN ({marks}) AND {blocked}"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        out.extend(query.fetch_all(pool).await?);
    }
    Ok(out)
}

/// ` AND NOT EXISTS (…)` for a query driven from `suwayomi_series`: drops a series whose
/// work the set blocks. `series_id` is an expression for the series id as TEXT. The
/// SQL twin of [`blocked_series_ids`].
pub fn and_series_not_blocked(set: &BlockSet, series_id: &str) -> String {
    match set.blocked_sql("ss.work_id", None) {
        Some(blocked) => format!(
            " AND NOT EXISTS (SELECT 1 FROM source_series ss \
               WHERE ss.source_type = 'suwayomi' AND ss.source_key = {series_id} AND {blocked})"
        ),
        None => String::new(),
    }
}

/// True when the set blocks this one work.
pub async fn is_blocked(pool: &Database, set: &BlockSet, work_id: &str) -> Result<bool> {
    if set.is_empty() {
        return Ok(false);
    }
    Ok(!blocked_ids(pool, set, &[work_id.to_string()])
        .await?
        .is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Database {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    fn rule(tags: &[&str], ratings: &[&str]) -> BlockRule {
        BlockRule::new(tags, ratings).unwrap()
    }

    #[test]
    fn a_rule_is_validated_and_canonical() {
        // Order, case of a rating and repeats do not matter.
        assert_eq!(
            rule(&["isekai", "harem", "isekai"], &["PORNOGRAPHIC", "erotica"]),
            rule(&["harem", "isekai"], &["erotica", "pornographic"]),
        );
        assert_eq!(
            rule(&["harem", "isekai"], &["pornographic", "erotica"]).ratings(),
            ["erotica", "pornographic"]
        );
        let none: [&str; 0] = [];
        assert_eq!(BlockRule::new(none, none), Err(RuleError::Empty));
        assert_eq!(BlockRule::new(none, RATINGS), Err(RuleError::Everything));
        // Every rating is fine once a tag narrows it.
        assert!(BlockRule::new(["gore"], RATINGS).is_ok());
        assert_eq!(
            BlockRule::new(none, ["adult"]),
            Err(RuleError::BadRating("adult".into()))
        );
        // Nothing but `[a-z0-9-]` can be a tag, so nothing can close a SQL literal.
        for bad in ["Gore", "go're", "a b", "x' OR 1=1 --", ""] {
            assert_eq!(
                BlockRule::new([bad], none),
                Err(RuleError::BadTag(bad.to_string())),
                "{bad}"
            );
        }
        let many: Vec<String> = (0..=MAX_RULE_TAGS).map(|n| format!("t{n}")).collect();
        assert_eq!(BlockRule::new(&many, none), Err(RuleError::TooManyTags));
    }

    #[test]
    fn a_set_has_one_key_per_meaning() {
        let a = BlockSet::new(vec![rule(&["gore"], &[]), rule(&[], &["pornographic"])]);
        let b = BlockSet::new(vec![
            rule(&[], &["pornographic"]),
            rule(&["gore"], &[]),
            rule(&["gore"], &[]),
        ]);
        assert_eq!(a.key(), b.key());
        // A tag rule and a rating rule with the same word are different rules.
        let tag = BlockSet::new(vec![rule(&["safe"], &[])]);
        let rating = BlockSet::new(vec![rule(&[], &["safe"])]);
        assert_ne!(tag.key(), rating.key());
        // One two-tag rule is not two one-tag rules.
        let both = BlockSet::new(vec![rule(&["a", "b"], &[])]);
        let either = BlockSet::new(vec![rule(&["a"], &[]), rule(&["b"], &[])]);
        assert_ne!(both.key(), either.key());
        assert!(BlockSet::default().is_empty());
        assert_eq!(BlockSet::default().blocked_sql("w.id", None), None);
        assert_eq!(BlockSet::default().and_not_blocked("w.id", None), "");
    }

    /// Five works: every mix of rating, NSFW flag and tags the rules have to tell apart.
    async fn seed(pool: &Database) {
        // (id, content_rating, is_nsfw, is_nsfw_override, tags)
        type Work = (
            &'static str,
            Option<&'static str>,
            i64,
            Option<i64>,
            &'static [&'static str],
        );
        let works: [Work; 5] = [
            ("w_safe", Some("safe"), 0, None, &["action"]),
            ("w_gore", Some("suggestive"), 0, None, &["gore", "action"]),
            ("w_adult_gore", Some("pornographic"), 1, None, &["gore"]),
            // No rating of its own, but a source flagged it adult: effective `erotica`.
            ("w_flagged", None, 1, None, &["romance"]),
            // An admin pinned a safe work adult: effective `erotica` too.
            ("w_pinned", Some("safe"), 0, Some(1), &["romance", "harem"]),
        ];
        for (id, rating, nsfw, pin, tags) in works {
            sqlx::query(
                "INSERT INTO work (id, primary_title, content_rating, is_nsfw, is_nsfw_override, \
                                   created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(id)
            .bind(rating)
            .bind(nsfw)
            .bind(pin)
            .execute(pool)
            .await
            .unwrap();
            for tag in tags {
                sqlx::query(
                    "INSERT INTO work_tag (work_id, tag, tag_key, source) VALUES (?, ?, ?, 'mangadex')",
                )
                .bind(id)
                .bind(tag)
                .bind(tag)
                .execute(pool)
                .await
                .unwrap();
            }
        }
    }

    async fn blocked(pool: &Database, rules: Vec<BlockRule>) -> Vec<String> {
        let set = BlockSet::new(rules);
        let all: Vec<String> = sqlx::query_scalar("SELECT id FROM work ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
        let hit = blocked_ids(pool, &set, &all).await.unwrap();
        // The WHERE form must agree with the lookup form.
        let sql = format!(
            "SELECT w.id FROM work w WHERE 1 = 1{} ORDER BY w.id",
            set.and_not_blocked("w.id", None)
        );
        let kept: Vec<String> = sqlx::query_scalar(&sql).fetch_all(pool).await.unwrap();
        let expect_kept: Vec<String> = all
            .iter()
            .filter(|id| !hit.contains(*id))
            .cloned()
            .collect();
        assert_eq!(kept, expect_kept);
        let mut hit: Vec<String> = hit.into_iter().collect();
        hit.sort();
        hit
    }

    #[tokio::test]
    async fn rules_block_by_tag_by_rating_and_by_both() {
        let pool = pool().await;
        seed(&pool).await;

        assert!(blocked(&pool, vec![]).await.is_empty());
        // A tag.
        assert_eq!(
            blocked(&pool, vec![rule(&["gore"], &[])]).await,
            ["w_adult_gore", "w_gore"]
        );
        // A rating. It is the EFFECTIVE rating: the flagged and the pinned work are
        // `erotica`, whatever their own column says.
        assert_eq!(
            blocked(&pool, vec![rule(&[], &["erotica"])]).await,
            ["w_flagged", "w_pinned"]
        );
        assert_eq!(blocked(&pool, vec![rule(&[], &["safe"])]).await, ["w_safe"]);
        // A tag and a rating: Gore, but only when adult.
        assert_eq!(
            blocked(&pool, vec![rule(&["gore"], &["erotica", "pornographic"])]).await,
            ["w_adult_gore"]
        );
        // Two tags: a work needs both.
        assert_eq!(
            blocked(&pool, vec![rule(&["romance", "harem"], &[])]).await,
            ["w_pinned"]
        );
        // Two rules: either one blocks.
        assert_eq!(
            blocked(
                &pool,
                vec![rule(&["harem"], &[]), rule(&[], &["pornographic"])]
            )
            .await,
            ["w_adult_gore", "w_pinned"]
        );
        // A tag no work carries blocks nothing.
        assert!(blocked(&pool, vec![rule(&["no-such-tag"], &[])])
            .await
            .is_empty());

        let set = BlockSet::new(vec![rule(&["gore"], &[])]);
        assert!(is_blocked(&pool, &set, "w_gore").await.unwrap());
        assert!(!is_blocked(&pool, &set, "w_safe").await.unwrap());
        // Not a work: never blocked.
        assert!(!is_blocked(&pool, &set, "12345").await.unwrap());
    }

    #[tokio::test]
    async fn the_blocked_sql_can_read_a_rating_column_of_the_callers_row() {
        let pool = pool().await;
        seed(&pool).await;
        // A caller whose row already has the effective rating passes it and saves the
        // probe of `work`. Here the "row" is `work` itself with its raw column.
        let set = BlockSet::new(vec![rule(&["gore"], &["suggestive"])]);
        let sql = format!(
            "SELECT w.id FROM work w WHERE {}",
            set.blocked_sql("w.id", Some("w.content_rating")).unwrap()
        );
        let hit: Vec<String> = sqlx::query_scalar(&sql).fetch_all(&pool).await.unwrap();
        assert_eq!(hit, ["w_gore"]);
    }

    #[tokio::test]
    async fn a_scope_is_replaced_as_a_whole_and_a_viewer_gets_both_scopes() {
        let pool = pool().await;
        for user in ["u1", "u2"] {
            sqlx::query(
                "INSERT INTO users (id, username, email, password_hash, created_at) \
                 VALUES (?, ?, ?, 'x', '2020-01-01T00:00:00Z')",
            )
            .bind(user)
            .bind(user)
            .bind(format!("{user}@example.test"))
            .execute(&pool)
            .await
            .unwrap();
        }
        let site = vec![rule(&[], &["pornographic"])];
        let mine = vec![
            rule(&["gore"], &[]),
            rule(&["gore"], &[]),
            rule(&["harem"], &["erotica"]),
        ];
        replace(&pool, Scope::Site, &site).await.unwrap();
        replace(&pool, Scope::User("u1"), &mine).await.unwrap();

        // Saved order kept, the repeat dropped.
        assert_eq!(list(&pool, Scope::Site).await.unwrap(), site);
        assert_eq!(
            list(&pool, Scope::User("u1")).await.unwrap(),
            vec![rule(&["gore"], &[]), rule(&["harem"], &["erotica"])]
        );
        assert!(list(&pool, Scope::User("u2")).await.unwrap().is_empty());

        // Anonymous and u2: the site rule. u1: all three.
        assert_eq!(
            load(&pool, None).await.unwrap(),
            BlockSet::new(site.clone())
        );
        assert_eq!(
            load(&pool, Some("u2")).await.unwrap(),
            BlockSet::new(site.clone())
        );
        assert_eq!(
            load(&pool, Some("u1")).await.unwrap().key(),
            "/pornographic;gore/;harem/erotica"
        );

        // Replacing one scope leaves the other alone; an empty list clears it.
        replace(&pool, Scope::User("u1"), &[]).await.unwrap();
        assert!(list(&pool, Scope::User("u1")).await.unwrap().is_empty());
        assert_eq!(list(&pool, Scope::Site).await.unwrap(), site);

        let too_many: Vec<BlockRule> = (0..=MAX_RULES)
            .map(|n| BlockRule::new([format!("t{n}")], [""; 0]).unwrap())
            .collect();
        assert!(replace(&pool, Scope::Site, &too_many).await.is_err());
        // The failed replace changed nothing.
        assert_eq!(list(&pool, Scope::Site).await.unwrap(), site);

        // A user's rules go with the account.
        replace(&pool, Scope::User("u2"), &mine).await.unwrap();
        sqlx::query("DELETE FROM users WHERE id = 'u2'")
            .execute(&pool)
            .await
            .unwrap();
        let left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM content_block_rule WHERE user_id = 'u2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0);
    }
}
