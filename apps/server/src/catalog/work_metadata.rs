//! Work metadata trust and provenance (M9 W2).
//!
//! Every source mapping records what it last said about its work, one
//! `work_metadata_contribution` row per value, grouped into six field groups. The
//! projector here owns the work's effective metadata: for each group it picks the first
//! mapping in rank order whose source is not barred for that group
//! (`source_metadata_bar`) and writes that mapping's values onto the existing `work`
//! columns and child tables, so every reader keeps reading what it reads today.
//!
//! Rank: MangaDex mappings first, then `source_series.created_at`, then `id`. Only
//! `mangadex` and `suwayomi` mappings count. A chosen mapping with no rows for the group
//! has not been observed yet: the work keeps its current values. When every source is
//! barred the work keeps its current values too (no blanking); the series editor reports
//! it ([`work_sources`]).
//!
//! Admin overrides (`title_override`, `description_override`) and field pins stay above
//! the projector: overrides are separate columns, and a pinned field's projected value
//! goes to `work_field_pin.source_value`, so unpinning restores it.
//!
//! The cover group is recorded here but enforced by the cover code (phase C).

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::Connection;

use crate::db::Database;

/// One group of work metadata a source can be barred from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FieldGroup {
    /// `primary_title`, `primary_lang` and the work's aliases.
    Title,
    /// `description` and `work_description`.
    Description,
    /// `author`, `artist` and `work_credit`.
    Credits,
    /// `work_tag` (MangaDex tier and Suwayomi tier).
    Tags,
    /// The work cover (phase C).
    Cover,
    /// `status`.
    Status,
}

impl FieldGroup {
    pub const ALL: [FieldGroup; 6] = [
        FieldGroup::Title,
        FieldGroup::Description,
        FieldGroup::Credits,
        FieldGroup::Tags,
        FieldGroup::Cover,
        FieldGroup::Status,
    ];

    /// The stored `field_group` value.
    pub fn as_str(self) -> &'static str {
        match self {
            FieldGroup::Title => "title",
            FieldGroup::Description => "description",
            FieldGroup::Credits => "credits",
            FieldGroup::Tags => "tags",
            FieldGroup::Cover => "cover",
            FieldGroup::Status => "status",
        }
    }

    pub fn parse(value: &str) -> Option<FieldGroup> {
        FieldGroup::ALL.into_iter().find(|g| g.as_str() == value)
    }
}

/// How sure we are that a source gave the work its current value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// A live writer saw the source give this value (or, in the backfill, the stored
    /// MangaDex scalars, which every MangaDex refresh overwrites).
    Observed,
    /// Backfill: the source's cached value equals the work's current value.
    Inferred,
    /// Backfill: the source's cached value differs from the work's current value.
    Unknown,
}

impl Attribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Attribution::Observed => "observed",
            Attribution::Inferred => "inferred",
            Attribution::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Attribution> {
        match value {
            "observed" => Some(Attribution::Observed),
            "inferred" => Some(Attribution::Inferred),
            "unknown" => Some(Attribution::Unknown),
            _ => None,
        }
    }
}

/// One contributed value. `slot` is `primary`/`alias` (title), `primary`/`localized`
/// (description), `author`/`artist` (credits), `tag`, `status` or `cover`; the empty
/// slot with an empty value marks a source that was observed and gave nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub slot: String,
    pub lang: String,
    pub value: String,
    pub ord: i32,
    pub detail: Option<serde_json::Value>,
}

/// Everything one source said about one work at one observation. A group missing from
/// the map was not observed and keeps its stored rows; a group present with no rows is
/// recorded as observed-and-empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observation {
    pub groups: BTreeMap<FieldGroup, Vec<Row>>,
}

impl Observation {
    /// What a MangaDex record says (`catalog::WorkInput` built by `mangadex::to_work_input`).
    /// Title: `primary` (primary_title, primary_lang) and one `alias` per input alias.
    /// Description: `primary` (input.description) and one `localized` per
    /// input.descriptions entry. Credits: the scalar author/artist first (ord 0), then
    /// input.credits in order, deduplicated. Tags: input.tags in order with
    /// `{"id", "group"}` detail from input.tag_details. Status: input.status. Cover:
    /// input.cover_file_name. Blank values are skipped.
    pub fn from_mangadex(input: &super::WorkInput) -> Observation {
        let mut observation = Self::empty();
        observation.push(
            FieldGroup::Title,
            "primary",
            input.primary_lang.as_deref(),
            input.primary_title.as_deref(),
            0,
            None,
        );
        for (ord, alias) in input.aliases.iter().enumerate() {
            observation.push(
                FieldGroup::Title,
                "alias",
                alias.lang.as_deref(),
                Some(&alias.raw),
                ord as i32,
                None,
            );
        }
        observation.push(
            FieldGroup::Description,
            "primary",
            None,
            input.description.as_deref(),
            0,
            None,
        );
        for (ord, (lang, value)) in input.descriptions.iter().enumerate() {
            observation.push(
                FieldGroup::Description,
                "localized",
                Some(lang),
                Some(value),
                ord as i32,
                None,
            );
        }
        for (role, value) in [("author", &input.author), ("artist", &input.artist)] {
            observation.push(FieldGroup::Credits, role, None, value.as_deref(), 0, None);
        }
        for (ord, (role, value)) in input.credits.iter().enumerate() {
            if matches!(role.as_str(), "author" | "artist") {
                observation.push(
                    FieldGroup::Credits,
                    role,
                    None,
                    Some(value),
                    ord as i32 + 1,
                    None,
                );
            }
        }
        for (ord, name) in input.tags.iter().enumerate() {
            let detail = input
                .tag_details
                .iter()
                .rev()
                .find(|detail| detail.name == *name)
                .map(|detail| serde_json::json!({"id": detail.id, "group": detail.group}));
            observation.push(
                FieldGroup::Tags,
                "tag",
                None,
                Some(name),
                ord as i32,
                detail,
            );
        }
        observation.push(
            FieldGroup::Status,
            "status",
            None,
            input.status.as_deref(),
            0,
            None,
        );
        observation.push(
            FieldGroup::Cover,
            "cover",
            None,
            input.cover_file_name.as_deref(),
            0,
            None,
        );
        observation.deduplicate();
        observation
    }

    /// What a Suwayomi manga says: title (`primary` and `alias`, no language),
    /// description (`primary`), credits (`author`, `artist`), tags (`tag`, in order),
    /// status (raw, as the mint writes it), cover (`thumbnail_url`). Blank values are
    /// skipped.
    pub fn from_suwayomi(m: &crate::suwayomi::SuwayomiManga) -> Observation {
        Self::from_suwayomi_values(
            &m.title,
            m.author.as_deref(),
            m.artist.as_deref(),
            m.description.as_deref(),
            &m.genre,
            &m.status,
            m.thumbnail_url.as_deref(),
        )
    }

    fn from_suwayomi_values(
        title: &str,
        author: Option<&str>,
        artist: Option<&str>,
        description: Option<&str>,
        genre: &[String],
        status: &str,
        thumbnail_url: Option<&str>,
    ) -> Observation {
        let mut observation = Self::empty();
        for slot in ["primary", "alias"] {
            observation.push(FieldGroup::Title, slot, None, Some(title), 0, None);
        }
        observation.push(
            FieldGroup::Description,
            "primary",
            None,
            description,
            0,
            None,
        );
        observation.push(FieldGroup::Credits, "author", None, author, 0, None);
        observation.push(FieldGroup::Credits, "artist", None, artist, 0, None);
        for (ord, name) in genre.iter().enumerate() {
            observation.push(FieldGroup::Tags, "tag", None, Some(name), ord as i32, None);
        }
        observation.push(FieldGroup::Status, "status", None, Some(status), 0, None);
        observation.push(FieldGroup::Cover, "cover", None, thumbnail_url, 0, None);
        observation.deduplicate();
        observation
    }

    fn empty() -> Self {
        Self {
            groups: FieldGroup::ALL
                .into_iter()
                .map(|group| (group, Vec::new()))
                .collect(),
        }
    }

    fn push(
        &mut self,
        group: FieldGroup,
        slot: &str,
        lang: Option<&str>,
        value: Option<&str>,
        ord: i32,
        detail: Option<serde_json::Value>,
    ) {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            self.groups.entry(group).or_default().push(Row {
                slot: slot.into(),
                lang: lang.unwrap_or("").into(),
                value: value.into(),
                ord,
                detail,
            });
        }
    }

    fn deduplicate(&mut self) {
        for rows in self.groups.values_mut() {
            rows.sort_by_key(|row| row.ord);
            let mut seen = HashSet::new();
            rows.retain(|row| seen.insert((row.slot.clone(), row.lang.clone(), row.value.clone())));
        }
    }
}

/// SQL boolean: the source named by the SQL text expression `source` is barred for the
/// group named by the SQL text expression `group`.
pub(crate) fn barred_sql(source: &str, group: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM source_metadata_bar b \
                  WHERE b.source_id = {source} AND b.field_group = {group})"
    )
}

/// SQL text expression for the source id of a `source_series` row aliased `alias`:
/// `mangadex` for MangaDex mappings, the Suwayomi source id otherwise.
pub(crate) fn mapping_source_sql(alias: &str) -> String {
    format!("COALESCE({alias}.source_id, {alias}.source_type)")
}

/// SQL text expression, over a `work` row aliased `w`, for the author or artist the
/// matcher may use (`role` is `author` or `artist`, the `work` column and the credit
/// slot): a pinned value always counts. If any supported mapping is barred for credits,
/// the stored value counts only when an allowed mapping contributed that exact value.
/// With no barred mapping this is the stored value, including unobserved legacy values.
pub(crate) fn match_credit_sql(role: &str) -> String {
    debug_assert!(role == "author" || role == "artist");
    let barred = barred_sql(&mapping_source_sql("ms"), "'credits'");
    format!(
        "CASE WHEN EXISTS (SELECT 1 FROM work_field_pin mp \
                            WHERE mp.work_id = w.id AND mp.field = '{role}') \
              THEN w.{role} \
              WHEN NOT EXISTS (SELECT 1 FROM source_series ms WHERE ms.work_id = w.id \
                                 AND ms.source_type IN ('mangadex', 'suwayomi') AND {barred}) \
                OR EXISTS (SELECT 1 FROM work_metadata_contribution mc \
                             JOIN source_series ms ON ms.id = mc.source_series_id \
                            WHERE ms.work_id = w.id AND ms.source_type IN ('mangadex', 'suwayomi') \
                              AND mc.field_group = 'credits' \
                              AND mc.slot = '{role}' AND mc.value = w.{role} AND NOT {barred}) \
              THEN w.{role} END"
    )
}

/// The groups `source_id` is barred from.
pub async fn barred_groups_on(
    conn: &mut sqlx::PgConnection,
    source_id: &str,
) -> Result<HashSet<FieldGroup>> {
    let groups: Vec<String> =
        sqlx::query_scalar("SELECT field_group FROM source_metadata_bar WHERE source_id = $1")
            .bind(source_id)
            .fetch_all(conn)
            .await?;
    Ok(groups
        .iter()
        .filter_map(|group| FieldGroup::parse(group))
        .collect())
}

/// Replace what mapping `source_series_id` says for every group in `observation` with
/// `observation`'s rows, as a diff: rows no longer said are deleted, new rows inserted,
/// changed `ord`/`detail`/`attribution` updated, `observed_at` set on inserted rows
/// only. Groups absent from `observation` are untouched. An unchanged observation writes
/// nothing. Returns whether anything changed.
///
/// The caller holds the lock of the mapping's work (`FOR NO KEY UPDATE`).
pub async fn record_on(
    conn: &mut sqlx::PgConnection,
    source_series_id: &str,
    observation: &Observation,
    attribution: Attribution,
) -> Result<bool> {
    let wanted = WantedRows::new(observation);
    if wanted.groups.is_empty() {
        return Ok(false);
    }
    let deleted = sqlx::query(&format!(
        "WITH wanted AS ({WANTED_SQL}) \
         DELETE FROM work_metadata_contribution c WHERE c.source_series_id = $1 \
           AND c.field_group = ANY($9::text[]) \
           AND NOT EXISTS (SELECT 1 FROM wanted w WHERE \
             (w.field_group, w.slot, w.lang, w.value_key) = (c.field_group, c.slot, c.lang, c.value_key))"
    ))
    .bind(source_series_id)
    .bind(&wanted.fields)
    .bind(&wanted.slots)
    .bind(&wanted.langs)
    .bind(&wanted.values)
    .bind(&wanted.ords)
    .bind(&wanted.details)
    .bind(attribution.as_str())
    .bind(&wanted.groups)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let inserted = sqlx::query(&format!(
        "WITH wanted AS ({WANTED_SQL}) \
         INSERT INTO work_metadata_contribution \
           (source_series_id, field_group, slot, lang, value, ord, detail, attribution, observed_at) \
         SELECT $1, w.field_group, w.slot, w.lang, w.value, w.ord, w.detail, $8, now() FROM wanted w \
         WHERE NOT EXISTS (SELECT 1 FROM work_metadata_contribution c WHERE c.source_series_id = $1 \
           AND (c.field_group, c.slot, c.lang, c.value_key) = (w.field_group, w.slot, w.lang, w.value_key) \
           AND (c.value, c.ord, c.detail, c.attribution) IS NOT DISTINCT FROM (w.value, w.ord, w.detail, $8::text)) \
         ON CONFLICT (source_series_id, field_group, slot, lang, value_key) DO UPDATE \
           SET value = excluded.value, ord = excluded.ord, detail = excluded.detail, attribution = excluded.attribution \
         WHERE (work_metadata_contribution.value, work_metadata_contribution.ord, work_metadata_contribution.detail, work_metadata_contribution.attribution) \
           IS DISTINCT FROM (excluded.value, excluded.ord, excluded.detail, excluded.attribution)"
    ))
    .bind(source_series_id).bind(&wanted.fields).bind(&wanted.slots).bind(&wanted.langs)
    .bind(&wanted.values).bind(&wanted.ords).bind(&wanted.details).bind(attribution.as_str())
    .execute(conn).await?.rows_affected();
    Ok(deleted + inserted > 0)
}

/// Read only: whether [`record_on`] with these arguments would change anything.
pub async fn differs_on(
    conn: &mut sqlx::PgConnection,
    source_series_id: &str,
    observation: &Observation,
    attribution: Attribution,
) -> Result<bool> {
    let wanted = WantedRows::new(observation);
    if wanted.groups.is_empty() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(&format!(
        "WITH wanted AS ({WANTED_SQL}), stored AS (SELECT * FROM work_metadata_contribution \
            WHERE source_series_id = $1 AND field_group = ANY($9::text[])) \
         SELECT EXISTS (SELECT 1 FROM wanted w FULL JOIN stored c \
           USING (field_group, slot, lang, value_key) \
           WHERE w.field_group IS NULL OR c.field_group IS NULL \
             OR (c.value, c.ord, c.detail, c.attribution) IS DISTINCT FROM (w.value, w.ord, w.detail, $8::text))"
    ))
    .bind(source_series_id)
    .bind(&wanted.fields)
    .bind(&wanted.slots)
    .bind(&wanted.langs)
    .bind(&wanted.values)
    .bind(&wanted.ords)
    .bind(&wanted.details)
    .bind(attribution.as_str())
    .bind(&wanted.groups)
    .fetch_one(conn)
    .await?)
}

const WANTED_SQL: &str =
    "SELECT field_group, slot, lang, value, md5(value) AS value_key, ord, detail::jsonb AS detail, $8::text AS attribution \
    FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::int[], $7::text[]) \
      AS u(field_group, slot, lang, value, ord, detail)";

#[derive(Default)]
struct WantedRows {
    groups: Vec<&'static str>,
    fields: Vec<&'static str>,
    slots: Vec<String>,
    langs: Vec<String>,
    values: Vec<String>,
    ords: Vec<i32>,
    details: Vec<Option<String>>,
}

impl WantedRows {
    fn new(observation: &Observation) -> Self {
        let mut observation = observation.clone();
        observation.deduplicate();
        let mut wanted = Self::default();
        for (group, mut rows) in observation.groups {
            wanted.groups.push(group.as_str());
            if rows.is_empty() {
                rows.push(Row {
                    slot: String::new(),
                    lang: String::new(),
                    value: String::new(),
                    ord: 0,
                    detail: None,
                });
            }
            for row in rows {
                wanted.fields.push(group.as_str());
                wanted.slots.push(row.slot);
                wanted.langs.push(row.lang);
                wanted.values.push(row.value);
                wanted.ords.push(row.ord);
                wanted
                    .details
                    .push(row.detail.map(|detail| detail.to_string()));
            }
        }
        wanted
    }
}

#[derive(sqlx::FromRow)]
struct Mapping {
    id: String,
    source_type: String,
    source_id: String,
}

#[derive(sqlx::FromRow)]
struct Contribution {
    source_series_id: String,
    field_group: String,
    slot: String,
    lang: String,
    value: String,
    ord: i32,
    detail: Option<serde_json::Value>,
    attribution: String,
}

struct Sources {
    mappings: Vec<Mapping>,
    contributions: Vec<Contribution>,
    bars: HashSet<(String, String)>,
}

impl Sources {
    async fn load(conn: &mut sqlx::PgConnection, work_id: &str) -> Result<Self> {
        let mappings: Vec<Mapping> = sqlx::query_as(
            "SELECT id, source_type, COALESCE(source_id, source_type) AS source_id FROM source_series \
             WHERE work_id = $1 AND source_type IN ('mangadex', 'suwayomi') \
             ORDER BY (source_type = 'mangadex') DESC, created_at, id"
        ).bind(work_id).fetch_all(&mut *conn).await?;
        let contributions = sqlx::query_as(
            "SELECT c.* FROM work_metadata_contribution c JOIN source_series ss ON ss.id = c.source_series_id \
             WHERE ss.work_id = $1 AND ss.source_type IN ('mangadex', 'suwayomi') \
             ORDER BY c.ord, c.slot, c.lang, c.value"
        ).bind(work_id).fetch_all(&mut *conn).await?;
        let ids: Vec<&str> = mappings
            .iter()
            .map(|mapping| mapping.source_id.as_str())
            .collect();
        let bars: Vec<(String, String)> = sqlx::query_as(
            "SELECT source_id, field_group FROM source_metadata_bar WHERE source_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(conn)
        .await?;
        Ok(Self {
            mappings,
            contributions,
            bars: bars.into_iter().collect(),
        })
    }

    fn barred(&self, mapping: &Mapping, group: FieldGroup) -> bool {
        self.bars
            .contains(&(mapping.source_id.clone(), group.as_str().into()))
    }

    fn chosen(&self, group: FieldGroup) -> Option<&Mapping> {
        self.mappings
            .iter()
            .find(|mapping| !self.barred(mapping, group))
    }

    fn rows(&self, mapping: &Mapping, group: FieldGroup) -> Vec<&Contribution> {
        self.contributions
            .iter()
            .filter(|row| row.source_series_id == mapping.id && row.field_group == group.as_str())
            .collect()
    }

    fn partition(&self, group: FieldGroup) -> (Vec<&Contribution>, Vec<&Contribution>) {
        let mut allowed = Vec::new();
        let mut barred = Vec::new();
        for mapping in &self.mappings {
            let target = if self.barred(mapping, group) {
                &mut barred
            } else {
                &mut allowed
            };
            target.extend(self.rows(mapping, group));
        }
        (allowed, barred)
    }
}

const PROJECTED_GROUPS: [FieldGroup; 5] = [
    FieldGroup::Title,
    FieldGroup::Description,
    FieldGroup::Credits,
    FieldGroup::Tags,
    FieldGroup::Status,
];

/// What one projector run changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    pub title: bool,
    pub aliases: bool,
    pub description: bool,
    pub credits: bool,
    pub tags: bool,
    pub status: bool,
}

impl Projection {
    pub fn any(self) -> bool {
        self.title || self.aliases || self.description || self.credits || self.tags || self.status
    }
}

/// Project the work's effective metadata for `groups` (the cover group is ignored here)
/// and re-project its cards in the same transaction: a title or alias change runs the
/// per-work Browse projection with search (which also refreshes the feed title); a
/// status change recomputes `feed_series_updates.status` first. Tag changes refresh
/// both cards' comic types. Writes only what differs.
///
/// The caller holds the work lock (`FOR NO KEY UPDATE`) and owns the transaction. The
/// Suwayomi tag tier is not rebuilt here (it lives in `tags.rs`, which skips barred
/// sources); callers that change bars resync it after commit. An observed chosen
/// MangaDex mapping replaces the MangaDex tag tier, including an observed-empty set.
pub async fn project_on(
    conn: &mut sqlx::PgConnection,
    work_id: &str,
    groups: &[FieldGroup],
) -> Result<Projection> {
    let sources = Sources::load(conn, work_id).await?;
    let mut projection = Projection::default();
    for group in PROJECTED_GROUPS {
        if !groups.contains(&group) {
            continue;
        }
        let chosen = sources.chosen(group);
        let rows = chosen
            .map(|mapping| sources.rows(mapping, group))
            .unwrap_or_default();
        let observed = !rows.is_empty();
        match group {
            FieldGroup::Title => {
                if let Some(primary) = rows
                    .iter()
                    .find(|row| row.slot == "primary" && !row.value.trim().is_empty())
                {
                    projection.title = sqlx::query(
                        "UPDATE work SET primary_title = $2, primary_lang = NULLIF($3, ''), updated_at = now() \
                         WHERE id = $1 AND (primary_title, primary_lang) IS DISTINCT FROM ($2::text, NULLIF($3::text, ''))"
                    ).bind(work_id).bind(&primary.value).bind(&primary.lang).execute(&mut *conn).await?.rows_affected() > 0;
                }
                projection.aliases = project_aliases(conn, work_id, &sources).await?;
            }
            FieldGroup::Description => {
                if observed {
                    let primary = rows
                        .iter()
                        .find(|row| row.slot == "primary")
                        .map(|row| row.value.as_str());
                    projection.description = sqlx::query(
                        "UPDATE work SET description = $2, updated_at = now() WHERE id = $1 AND description IS DISTINCT FROM $2"
                    ).bind(work_id).bind(primary).execute(&mut *conn).await?.rows_affected() > 0;
                    let mut localized = BTreeMap::new();
                    for row in rows
                        .iter()
                        .filter(|row| row.slot == "localized" && !row.lang.is_empty())
                    {
                        localized
                            .entry(row.lang.as_str())
                            .or_insert(row.value.as_str());
                    }
                    let langs: Vec<&str> = localized.keys().copied().collect();
                    let values: Vec<&str> = localized.values().copied().collect();
                    projection.description |= sqlx::query(
                        "INSERT INTO work_description (work_id, lang, description) \
                         SELECT $1, lang, value FROM UNNEST($2::text[], $3::text[]) AS u(lang, value) \
                         WHERE NOT EXISTS (SELECT 1 FROM work_description d WHERE d.work_id = $1 AND d.lang = u.lang AND d.description = u.value) \
                         ON CONFLICT (work_id, lang) DO UPDATE SET description = excluded.description \
                         WHERE work_description.description IS DISTINCT FROM excluded.description"
                    ).bind(work_id).bind(&langs).bind(&values).execute(&mut *conn).await?.rows_affected() > 0;
                }
                projection.description |=
                    delete_barred_children(conn, work_id, &sources, group).await?;
            }
            FieldGroup::Credits => {
                if observed {
                    for role in ["author", "artist"] {
                        let value = rows
                            .iter()
                            .filter(|row| row.slot == role)
                            .min_by_key(|row| row.ord)
                            .map(|row| row.value.as_str());
                        projection.credits |= project_pinnable(conn, work_id, role, value).await?;
                    }
                    if chosen.is_some_and(|mapping| mapping.source_type == "mangadex") {
                        let roles: Vec<&str> = rows
                            .iter()
                            .filter(|row| matches!(row.slot.as_str(), "author" | "artist"))
                            .map(|row| row.slot.as_str())
                            .collect();
                        let names: Vec<&str> = rows
                            .iter()
                            .filter(|row| matches!(row.slot.as_str(), "author" | "artist"))
                            .map(|row| row.value.trim())
                            .collect();
                        projection.credits |= sqlx::query(
                            "INSERT INTO work_credit (work_id, role, name) \
                             SELECT $1, role, name FROM UNNEST($2::text[], $3::text[]) AS u(role, name) \
                             WHERE name <> '' ON CONFLICT DO NOTHING"
                        ).bind(work_id).bind(&roles).bind(&names).execute(&mut *conn).await?.rows_affected() > 0;
                    }
                }
                projection.credits |=
                    delete_barred_children(conn, work_id, &sources, group).await?;
            }
            FieldGroup::Tags => {
                if !sources.mappings.iter().any(|mapping| {
                    mapping.source_type == "mangadex" && !sources.barred(mapping, group)
                }) {
                    projection.tags = sqlx::query(
                        "DELETE FROM work_tag WHERE work_id = $1 AND source = 'mangadex'",
                    )
                    .bind(work_id)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected()
                        > 0;
                } else if observed
                    && chosen.is_some_and(|mapping| mapping.source_type == "mangadex")
                {
                    let tags: Vec<String> = rows
                        .iter()
                        .filter(|row| row.slot == "tag")
                        .map(|row| row.value.clone())
                        .collect();
                    let details = rows
                        .iter()
                        .filter(|row| row.slot == "tag")
                        .filter_map(|row| {
                            let detail = row.detail.as_ref()?;
                            let group = detail.get("group")?.as_str()?;
                            Some(super::SourceTag {
                                name: row.value.clone(),
                                id: detail
                                    .get("id")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned),
                                group: group.into(),
                            })
                        })
                        .collect::<Vec<_>>();
                    projection.tags = restore_tags(conn, work_id, &tags, &details).await?;
                }
            }
            FieldGroup::Status => {
                if observed {
                    let value = rows
                        .iter()
                        .find(|row| row.slot == "status")
                        .map(|row| row.value.as_str());
                    projection.status = project_pinnable(conn, work_id, "status", value).await?;
                }
            }
            FieldGroup::Cover => {}
        }
    }
    if projection.status {
        refresh_feed_status_on(conn, work_id).await?;
    }
    if projection.title || projection.aliases || projection.status {
        super::upsert_browse_work_inner(conn, work_id, projection.title || projection.aliases)
            .await?;
    }
    if projection.tags {
        refresh_comic_types_on(conn, work_id).await?;
    }
    Ok(projection)
}

/// Refresh feed status from the locked work, including after an unpin restored it.
/// Runs on the caller's transaction and writes only changed statuses.
pub(crate) async fn refresh_feed_status_on(
    conn: &mut sqlx::PgConnection,
    work_id: &str,
) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE feed_series_updates f SET status = {} FROM work w WHERE w.id = $1 AND f.work_id = w.id AND f.status IS DISTINCT FROM ({})",
        super::FSU_STATUS_SQL, super::FSU_STATUS_SQL
    )).bind(work_id).execute(conn).await?;
    Ok(())
}

async fn refresh_comic_types_on(conn: &mut sqlx::PgConnection, work_id: &str) -> Result<()> {
    super::fill_comic_types(conn, "browse_catalogue", Some(work_id)).await?;
    super::fill_comic_types(conn, "feed_series_updates", Some(work_id)).await?;
    Ok(())
}

async fn project_aliases(
    conn: &mut sqlx::PgConnection,
    work_id: &str,
    sources: &Sources,
) -> Result<bool> {
    let (allowed, barred) = sources.partition(FieldGroup::Title);
    let title_keys = |rows: &[&Contribution]| -> HashSet<String> {
        rows.iter()
            .filter(|row| matches!(row.slot.as_str(), "primary" | "alias"))
            .map(|row| super::normalize::normalize_title(&row.value))
            .collect()
    };
    let allowed_keys = title_keys(&allowed);
    let barred_keys = title_keys(&barred);
    let mut delete: Vec<&str> = barred_keys
        .difference(&allowed_keys)
        .map(String::as_str)
        .collect();
    delete.sort_unstable();
    let deleted =
        sqlx::query("DELETE FROM work_alias WHERE work_id = $1 AND normalized_title = ANY($2)")
            .bind(work_id)
            .bind(&delete)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0;
    // NULL languages do not conflict in the legacy UNIQUE key. Filter them too, so
    // a repeated projection neither duplicates aliases nor rewrites their tokens.
    let existing: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT normalized_title, lang FROM work_alias WHERE work_id = $1")
            .bind(work_id)
            .fetch_all(&mut *conn)
            .await?;
    let mut existing: HashSet<_> = existing.into_iter().collect();
    let aliases: Vec<super::Alias> = allowed
        .iter()
        .filter(|row| row.slot == "alias")
        .filter_map(|row| {
            let normalized = super::normalize::normalize_title(&row.value);
            let lang = (!row.lang.is_empty()).then(|| row.lang.clone());
            if normalized.is_empty() || !existing.insert((normalized, lang.clone())) {
                None
            } else {
                Some(super::Alias {
                    raw: row.value.clone(),
                    lang,
                })
            }
        })
        .collect();
    if aliases.is_empty() && !deleted {
        return Ok(false);
    }
    // The catalog helpers take Transaction. This is a savepoint inside the caller's
    // transaction; releasing it never commits the caller's unit.
    let mut unit = conn.begin().await?;
    let inserted = super::insert_aliases(&mut unit, work_id, &aliases).await?;
    if deleted {
        super::rebuild_work_alias_tokens(&mut unit, work_id).await?;
    }
    unit.commit().await?;
    Ok(deleted || inserted)
}

async fn delete_barred_children(
    conn: &mut sqlx::PgConnection,
    work_id: &str,
    sources: &Sources,
    group: FieldGroup,
) -> Result<bool> {
    let (allowed, barred) = sources.partition(group);
    let keys = |rows: &[&Contribution]| -> HashSet<(String, String)> {
        rows.iter()
            .filter_map(|row| match group {
                FieldGroup::Description if row.slot == "localized" => {
                    Some((row.lang.clone(), row.value.clone()))
                }
                FieldGroup::Credits if matches!(row.slot.as_str(), "author" | "artist") => {
                    Some((row.slot.clone(), row.value.trim().into()))
                }
                _ => None,
            })
            .collect()
    };
    let allowed = keys(&allowed);
    let barred = keys(&barred);
    let (left, right): (Vec<&str>, Vec<&str>) = barred
        .difference(&allowed)
        .map(|(left, right)| (left.as_str(), right.as_str()))
        .unzip();
    if left.is_empty() {
        return Ok(false);
    }
    let (table, a, b) = match group {
        FieldGroup::Description => ("work_description", "lang", "description"),
        FieldGroup::Credits => ("work_credit", "role", "name"),
        _ => unreachable!("only descriptions and credits have paired child keys"),
    };
    Ok(sqlx::query(&format!(
        "DELETE FROM {table} c USING UNNEST($2::text[], $3::text[]) AS u(a, b) \
         WHERE c.work_id = $1 AND c.{a} = u.a AND c.{b} = u.b"
    ))
    .bind(work_id)
    .bind(&left)
    .bind(&right)
    .execute(conn)
    .await?
    .rows_affected()
        > 0)
}

async fn project_pinnable(
    conn: &mut sqlx::PgConnection,
    work_id: &str,
    field: &str,
    value: Option<&str>,
) -> Result<bool> {
    debug_assert!(matches!(field, "author" | "artist" | "status"));
    Ok(sqlx::query(&format!(
        "WITH pin AS (UPDATE work_field_pin SET source_value = $2 \
           WHERE work_id = $1 AND field = '{field}' AND source_value IS DISTINCT FROM $2) \
         UPDATE work SET {field} = $2, updated_at = now() WHERE id = $1 AND {field} IS DISTINCT FROM $2 \
           AND NOT EXISTS (SELECT 1 FROM work_field_pin WHERE work_id = $1 AND field = '{field}')"
    )).bind(work_id).bind(value).execute(conn).await?.rows_affected() > 0)
}

async fn restore_tags(
    conn: &mut sqlx::PgConnection,
    work_id: &str,
    tags: &[String],
    details: &[super::SourceTag],
) -> Result<bool> {
    let mut unit = conn.begin().await?;
    const TAGS: &str = "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.tag), '[]'::jsonb) \
                        FROM work_tag t WHERE work_id = $1 AND source = 'mangadex'";
    let before: serde_json::Value = sqlx::query_scalar(TAGS)
        .bind(work_id)
        .fetch_one(&mut *unit)
        .await?;
    super::replace_source_tags(&mut unit, work_id, tags, details).await?;
    let after: serde_json::Value = sqlx::query_scalar(TAGS)
        .bind(work_id)
        .fetch_one(&mut *unit)
        .await?;
    unit.commit().await?;
    Ok(before != after)
}

/// Read the current cache observation while the caller holds the Suwayomi identity
/// key shared with cache publication. An absent row lets the caller use its capture.
pub(crate) async fn cached_suwayomi_observation_on(
    conn: &mut sqlx::PgConnection,
    source_key: &str,
) -> Result<Option<Observation>> {
    #[derive(sqlx::FromRow)]
    struct CachedMetadata {
        title: String,
        author: Option<String>,
        artist: Option<String>,
        description: Option<String>,
        genre: Option<serde_json::Value>,
        status: String,
        thumbnail_url: Option<String>,
    }
    let cached: Option<CachedMetadata> = sqlx::query_as(
        "SELECT title, author, artist, description, genre, status, thumbnail_url \
         FROM suwayomi_series WHERE id = public.sqlite_integer_pin($1)",
    )
    .bind(source_key)
    .fetch_optional(conn)
    .await?;
    Ok(cached.map(|cached| {
        let genre = cached
            .genre
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Observation::from_suwayomi_values(
            &cached.title,
            cached.author.as_deref(),
            cached.artist.as_deref(),
            cached.description.as_deref(),
            &genre,
            &cached.status,
            cached.thumbnail_url.as_deref(),
        )
    }))
}

/// After `series_cache::put_series` committed `m`: record the current cache row for its
/// `suwayomi` mapping and project the mapping's work. Skips quietly when the manga has
/// no mapping or nothing changed (no lock, no write). Own transaction, whole-unit retry,
/// engine generation held shared and publication keys taken before works like `put_series`.
pub async fn observe_suwayomi(pool: &Database, m: &crate::suwayomi::SuwayomiManga) -> Result<()> {
    const MAPPING: &str =
        "SELECT id, work_id FROM source_series WHERE source_type = 'suwayomi' AND source_key = $1";
    let observation = Observation::from_suwayomi(m);
    let key = m.id.to_string();
    {
        let mut conn = pool.acquire().await?;
        let mapping: Option<(String, String)> = sqlx::query_as(MAPPING)
            .bind(&key)
            .fetch_optional(&mut *conn)
            .await?;
        let Some((mapping, _)) = mapping else {
            return Ok(());
        };
        if !differs_on(&mut conn, &mapping, &observation, Attribution::Observed).await? {
            return Ok(());
        }
    }
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        crate::engine_generation::shared(
            &mut tx,
            crate::engine_keys::token(m.engine_generation.as_deref())?,
        )
        .await?;
        crate::locks::advisory(&mut tx, &crate::series_cache::series_publication_keys(&key))
            .await?;
        let mapping: Option<(String, String)> = sqlx::query_as(MAPPING)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((mapping, work)) = mapping else {
            return Ok(());
        };
        let locked = crate::locks::lock_works(&mut tx, &[&work]).await?;
        let current: Option<(String, String)> = sqlx::query_as(MAPPING)
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
        if !locked.missing.is_empty() || current.as_ref() != Some(&(mapping.clone(), work.clone()))
        {
            return Err(crate::locks::RestartTransaction.into());
        }
        let Some(observation) = cached_suwayomi_observation_on(&mut tx, &key).await? else {
            return Ok(());
        };
        if record_on(&mut tx, &mapping, &observation, Attribution::Observed).await? {
            project_on(&mut tx, &work, &PROJECTED_GROUPS).await?;
        }
        crate::engine_generation::commit_ready(pool)?;
        tx.commit().await?;
        Ok(())
    })
    .await
}

/// Where a work's group gets its value, for the admin series editor.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSource {
    pub group: FieldGroup,
    pub state: SourceState,
    /// The chosen mapping (for `Source` and `KeptNotObserved`).
    pub source_series_id: Option<String>,
    /// `mangadex` or the Suwayomi source id of the chosen mapping.
    pub source_id: Option<String>,
    /// The least certain attribution among the chosen mapping's rows (`Source` only).
    pub attribution: Option<Attribution>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceState {
    /// The value comes from `source_id`.
    Source,
    /// Every linked source is barred for the group: the current value is kept.
    KeptAllBarred,
    /// The chosen source has not been observed yet: the current value is kept.
    KeptNotObserved,
    /// The work has no `mangadex` or `suwayomi` mapping.
    NoSource,
}

/// Per group, where `work_id` gets its value. Read only.
pub async fn work_sources(pool: &Database, work_id: &str) -> Result<Vec<GroupSource>> {
    let mut conn = pool.acquire().await?;
    let sources = Sources::load(&mut conn, work_id).await?;
    Ok(FieldGroup::ALL
        .into_iter()
        .map(|group| {
            let mut source = GroupSource {
                group,
                state: SourceState::NoSource,
                source_series_id: None,
                source_id: None,
                attribution: None,
            };
            if sources.mappings.is_empty() {
                return source;
            }
            let Some(chosen) = sources.chosen(group) else {
                source.state = SourceState::KeptAllBarred;
                return source;
            };
            source.source_series_id = Some(chosen.id.clone());
            source.source_id = Some(chosen.source_id.clone());
            let rows = sources.rows(chosen, group);
            if rows.is_empty() {
                source.state = SourceState::KeptNotObserved;
            } else {
                source.state = SourceState::Source;
                source.attribution = Some(if rows.iter().any(|row| row.attribution == "unknown") {
                    Attribution::Unknown
                } else if rows.iter().any(|row| row.attribution == "inferred") {
                    Attribution::Inferred
                } else {
                    Attribution::Observed
                });
            }
            source
        })
        .collect())
}

/// Every bar, ordered by source id then group.
pub async fn list_bars(pool: &Database) -> Result<Vec<(String, FieldGroup, DateTime<Utc>)>> {
    let bars: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT source_id, field_group, created_at FROM source_metadata_bar ORDER BY source_id, field_group"
    ).fetch_all(pool).await?;
    Ok(bars
        .into_iter()
        .filter_map(|(source, group, created)| {
            FieldGroup::parse(&group).map(|group| (source, group, created))
        })
        .collect())
}

/// Whether `id` can name a source in a bar: `mangadex`, or 1-32 ASCII letters, digits
/// or `-` (a Suwayomi source id).
pub fn valid_source_id(id: &str) -> bool {
    id == "mangadex"
        || (!id.is_empty()
            && id.len() <= 32
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// A bar's pending re-projection lives in `instance_setting` under this prefix plus
/// `{source_id}:{group}`, so a run cut off by a dropped request, an error or a restart
/// is finished by [`resume_pending_bars`].
const PENDING_PREFIX: &str = "metadata_bar_reproject:";
const BAR_CHUNK: usize = 100;
const PENDING_BOOT_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
const PENDING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// One toggle's re-projection: `run` names the toggle (a newer toggle of the same
/// source and group replaces it), `after` is the last work id whose chunk finished.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingBar {
    run: String,
    after: String,
}

fn pending_key(source_id: &str, group: FieldGroup) -> String {
    format!("{PENDING_PREFIX}{source_id}:{}", group.as_str())
}

/// The runs this process is executing, so the periodic resume does not run a toggle's
/// chunks a second time beside the task the mutation started.
static RUNNING_BARS: std::sync::LazyLock<std::sync::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

struct RunningBar(String);

impl RunningBar {
    fn claim(run: &str) -> Option<Self> {
        let mut running = RUNNING_BARS.lock().unwrap_or_else(|e| e.into_inner());
        running.insert(run.to_owned()).then(|| Self(run.to_owned()))
    }
}

impl Drop for RunningBar {
    fn drop(&mut self) {
        RUNNING_BARS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Bar (`barred = true`) or allow `source_id` for `group`, then re-project every work
/// with a mapping of that source: [`record_bar`] followed by [`reproject_bar`].
/// Returns how many works were re-projected, plus source covers removed after a
/// cover bar (a work may count in both).
pub async fn set_bar(
    pool: &Database,
    source_id: &str,
    group: FieldGroup,
    barred: bool,
) -> Result<u64> {
    let (run, _) = record_bar(pool, source_id, group, barred).await?;
    reproject_bar(pool, source_id, group, &run).await
}

/// Store the bar state and a fresh pending re-projection for `source_id` and `group`
/// in one transaction under the exclusive `SourcePolicy(source_id)` advisory key.
/// Returns the run for [`reproject_bar`] and how many works have a mapping of the
/// source. Storing the same state again is harmless and restarts the re-projection.
pub async fn record_bar(
    pool: &Database,
    source_id: &str,
    group: FieldGroup,
    barred: bool,
) -> Result<(String, u64)> {
    anyhow::ensure!(valid_source_id(source_id), "invalid source id");
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin_with("BEGIN").await?;
        crate::locks::advisory(
            &mut tx,
            &[crate::locks::AdvisoryKey::new(
                crate::locks::KeyClass::SourcePolicy,
                &[source_id],
            )],
        )
        .await?;
        if barred {
            sqlx::query("INSERT INTO source_metadata_bar (source_id, field_group, created_at) VALUES ($1, $2, now()) ON CONFLICT DO NOTHING")
                .bind(source_id).bind(group.as_str()).execute(&mut *tx).await?;
        } else {
            sqlx::query(
                "DELETE FROM source_metadata_bar WHERE source_id = $1 AND field_group = $2",
            )
            .bind(source_id)
            .bind(group.as_str())
            .execute(&mut *tx)
            .await?;
        }
        let run = uuid::Uuid::new_v4().simple().to_string();
        let pending = serde_json::to_string(&PendingBar {
            run: run.clone(),
            after: String::new(),
        })?;
        sqlx::query(
            "INSERT INTO instance_setting (key, value, updated_at) VALUES ($1, $2, now()) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(pending_key(source_id, group))
        .bind(&pending)
        .execute(&mut *tx)
        .await?;
        let works: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT work_id) FROM source_series WHERE COALESCE(source_id, source_type) = $1",
        )
        .bind(source_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((run, works as u64))
    })
    .await
}

/// Re-project the works of `source_id` for `group` for the pending `run`, from its
/// cursor on, in chunks of 100 works by id. Each chunk is one retried transaction:
/// exclusive `SourcePolicy(source_id)` advisory key, the pending row re-read (a newer
/// toggle or a finished run stops this one), then the chunk's works locked in sorted
/// order (rediscovered under the lock; a changed set restarts) and [`project_on`] for
/// `group` only. For the tags group, each committed chunk's works that have a Suwayomi
/// mapping or Suwayomi-tier tags then get `tags::sync_work_suwayomi_tags(pool, work,
/// None, &[])` and their materialized comic types refreshed under the work lock. For
/// the cover group no projector runs: each chunk instead refreshes its works' stored
/// feed and Browse thumbnail fallbacks on the same transaction, writing only changed
/// rows. The cursor moves after a chunk's follow-up work, so an interrupted chunk runs
/// again; every step is idempotent. A cover bar then removes the source's published
/// covers, and the pending row is deleted last. Returns how many works were
/// re-projected, plus source covers removed (a work may count in both); 0 when this
/// process already runs `run`.
pub async fn reproject_bar(
    pool: &Database,
    source_id: &str,
    group: FieldGroup,
    run: &str,
) -> Result<u64> {
    let Some(_running) = RunningBar::claim(run) else {
        return Ok(0);
    };
    let key = pending_key(source_id, group);
    let mut projected = 0;
    loop {
        let Some((works, pending)) =
            crate::db::retry_transaction(|| set_bar_chunk(pool, source_id, group, &key, run))
                .await?
        else {
            return Ok(projected);
        };
        projected += works.len() as u64;
        if group == FieldGroup::Tags && !works.is_empty() {
            // Only the Suwayomi tier is rebuilt here; works without it have nothing to
            // resync, and project_on already refreshed their comic types.
            let tiered: Vec<String> = sqlx::query_scalar(
                "SELECT work_id FROM source_series WHERE work_id = ANY($1) AND source_type = 'suwayomi' \
                 UNION SELECT work_id FROM work_tag WHERE work_id = ANY($1) AND source = 'suwayomi' \
                 ORDER BY 1",
            )
            .bind(&works)
            .fetch_all(pool)
            .await?;
            for work in &tiered {
                if let Err(error) =
                    crate::tags::sync_work_suwayomi_tags(pool, work, None, &[]).await
                {
                    tracing::warn!(work_id = %work, %error, "metadata bar tag resync failed");
                }
                crate::db::retry_transaction(|| async {
                    let mut tx = pool.begin().await?;
                    let locked = crate::locks::lock_works(&mut tx, &[work]).await?;
                    if locked.missing.is_empty() {
                        refresh_comic_types_on(&mut tx, work).await?;
                    }
                    tx.commit().await?;
                    Ok(())
                })
                .await?;
            }
        }
        let Some(last) = works.last() else {
            break;
        };
        let next = serde_json::to_string(&PendingBar {
            run: run.to_owned(),
            after: last.clone(),
        })?;
        // Compare-and-set: a newer toggle's row is left alone (the next chunk stops).
        sqlx::query(
            "UPDATE instance_setting SET value = $3, updated_at = now() WHERE key = $1 AND value = $2",
        )
        .bind(&key)
        .bind(&pending)
        .bind(&next)
        .execute(pool)
        .await?;
        if works.len() < BAR_CHUNK {
            break;
        }
    }
    let barred: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM source_metadata_bar WHERE source_id = $1 AND field_group = $2)",
    )
    .bind(source_id)
    .bind(group.as_str())
    .fetch_one(pool)
    .await?;
    if group == FieldGroup::Cover && barred {
        let mut after = String::new();
        loop {
            let works: Vec<String> = sqlx::query_scalar(
                "SELECT id FROM work WHERE cover_source = $1 AND id > $2 ORDER BY id LIMIT 100",
            )
            .bind(source_id)
            .bind(&after)
            .fetch_all(pool)
            .await?;
            for work in &works {
                projected +=
                    crate::cover::remove_barred_work_cover(pool, work, source_id).await? as u64;
            }
            let Some(last) = works.last() else {
                break;
            };
            after = last.clone();
            if works.len() < 100 {
                break;
            }
        }
    }
    sqlx::query("DELETE FROM instance_setting WHERE key = $1 AND value::jsonb ->> 'run' = $2")
        .bind(&key)
        .bind(run)
        .execute(pool)
        .await?;
    Ok(projected)
}

/// Finish every pending bar re-projection (a dropped admin request, an error or a
/// restart left it). Returns how many works were re-projected.
pub async fn resume_pending_bars(pool: &Database) -> Result<u64> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, value FROM instance_setting WHERE starts_with(key, $1) ORDER BY key",
    )
    .bind(PENDING_PREFIX)
    .fetch_all(pool)
    .await?;
    let mut projected = 0;
    for (key, value) in rows {
        let parsed = key[PENDING_PREFIX.len()..]
            .rsplit_once(':')
            .and_then(|(source, group)| Some((source, FieldGroup::parse(group)?)))
            .filter(|(source, _)| valid_source_id(source));
        let (Some((source_id, group)), Ok(pending)) =
            (parsed, serde_json::from_str::<PendingBar>(&value))
        else {
            tracing::warn!(%key, "skipping a malformed pending metadata bar re-projection");
            continue;
        };
        projected += reproject_bar(pool, source_id, group, &pending.run).await?;
    }
    Ok(projected)
}

/// Resume pending bar re-projections shortly after start and then every ten minutes,
/// calling `done` (the read caches' clear) after a pass that re-projected anything.
pub fn spawn_pending_bars(
    pool: Database,
    shutdown: tokio::sync::watch::Receiver<bool>,
    done: fn(),
) {
    let pool = pool.background();
    tokio::spawn(crate::task::supervise(
        "metadata-bar-resume",
        PENDING_INTERVAL,
        shutdown.clone(),
        move || pending_bars_loop(pool.clone(), shutdown.clone(), done),
    ));
}

async fn pending_bars_loop(
    pool: Database,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    done: fn(),
) {
    let mut delay = PENDING_BOOT_DELAY;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        if *shutdown.borrow() {
            return;
        }
        delay = PENDING_INTERVAL;
        let result = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            result = resume_pending_bars(&pool) => result,
        };
        match result {
            Ok(0) => {}
            Ok(works) => {
                done();
                tracing::info!(works, "pending metadata bar re-projection finished");
            }
            Err(error) => {
                let error = crate::db::observed_database_error(error);
                tracing::warn!(error = %error, "pending metadata bar re-projection failed; will retry");
            }
        }
    }
}

async fn set_bar_chunk(
    pool: &Database,
    source_id: &str,
    group: FieldGroup,
    key: &str,
    run: &str,
) -> Result<Option<(Vec<String>, String)>> {
    const WORKS: &str =
        "SELECT DISTINCT work_id FROM source_series WHERE COALESCE(source_id, source_type) = $1 \
        AND work_id > $2 ORDER BY work_id LIMIT 100";
    let mut tx = pool.begin_with("BEGIN").await?;
    crate::locks::advisory(
        &mut tx,
        &[crate::locks::AdvisoryKey::new(
            crate::locks::KeyClass::SourcePolicy,
            &[source_id],
        )],
    )
    .await?;
    let stored: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = $1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    let pending = stored
        .as_deref()
        .and_then(|value| serde_json::from_str::<PendingBar>(value).ok());
    let (Some(stored), Some(pending)) = (stored, pending) else {
        tx.rollback().await?;
        return Ok(None);
    };
    if pending.run != run {
        tx.rollback().await?;
        return Ok(None);
    }
    let works: Vec<String> = sqlx::query_scalar(WORKS)
        .bind(source_id)
        .bind(&pending.after)
        .fetch_all(&mut *tx)
        .await?;
    let locked = crate::locks::lock_works(&mut tx, &works).await?;
    let current: Vec<String> = sqlx::query_scalar(WORKS)
        .bind(source_id)
        .bind(&pending.after)
        .fetch_all(&mut *tx)
        .await?;
    if !locked.missing.is_empty() || works != current {
        return Err(crate::locks::RestartTransaction.into());
    }
    if group == FieldGroup::Cover {
        // The projector does nothing for cover. What a cover bar changes is the stored
        // Suwayomi thumbnail fallback: the feed row first, with the rebuild's rule (the
        // newest-releasing allowed mapping, NULL without one), because Browse copies it.
        let allowed = super::suwayomi_cover_thumbnails_select("AND ss.work_id = $1", true);
        let feed = format!(
            "UPDATE feed_series_updates f SET suwayomi_thumbnail = c.thumbnail_url \
               FROM (SELECT (SELECT a.thumbnail_url FROM ({allowed}) a) AS thumbnail_url) c \
              WHERE f.work_id = $1 AND f.suwayomi_thumbnail IS DISTINCT FROM c.thumbnail_url"
        );
        for work in &works {
            sqlx::query(&feed).bind(work).execute(&mut *tx).await?;
            super::upsert_browse_work_inner(&mut tx, work, false).await?;
        }
    } else {
        for work in &works {
            project_on(&mut tx, work, &[group]).await?;
        }
    }
    tx.commit().await?;
    Ok(Some((works, stored)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        phase4_tests::{pool, source, work},
        Alias, SourceTag, WorkInput,
    };

    fn row(slot: &str, lang: &str, value: &str, ord: i32) -> Row {
        Row {
            slot: slot.into(),
            lang: lang.into(),
            value: value.into(),
            ord,
            detail: None,
        }
    }

    fn single_group(group: FieldGroup, rows: Vec<Row>) -> Observation {
        Observation {
            groups: [(group, rows)].into_iter().collect(),
        }
    }

    fn manga() -> crate::suwayomi::SuwayomiManga {
        serde_json::from_value(serde_json::json!({
            "id": 7, "title": "Suwayomi title", "thumbnailUrl": "cover.jpg",
            "author": "Writer", "artist": "Artist", "description": "Description",
            "genre": ["Action", " ", "Action", "Adventure"], "status": "ON_HIATUS",
            "inLibrary": false, "inLibraryAt": null, "lastFetchedAt": null,
            "sourceId": "source", "source": null, "chapters": null
        }))
        .unwrap()
    }

    async fn mapping(
        pool: &Database,
        work: &str,
        kind: &str,
        source: &str,
        key: &str,
        created: &str,
    ) -> String {
        let id = format!("ss_{}", uuid::Uuid::new_v4().simple());
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz)")
            .bind(&id).bind(work).bind(kind).bind(source).bind(key).bind(created).execute(pool).await.unwrap();
        id
    }

    async fn record(
        pool: &Database,
        work: &str,
        mapping: &str,
        observation: &Observation,
        attribution: Attribution,
    ) -> bool {
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[work]).await.unwrap();
        let changed = record_on(&mut tx, mapping, observation, attribution)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        changed
    }

    async fn project(pool: &Database, work: &str, groups: &[FieldGroup]) -> Projection {
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[work]).await.unwrap();
        let changed = project_on(&mut tx, work, groups).await.unwrap();
        tx.commit().await.unwrap();
        changed
    }

    async fn title(pool: &Database, work: &str) -> String {
        sqlx::query_scalar("SELECT primary_title FROM work WHERE id = $1")
            .bind(work)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn aliases(pool: &Database, work: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT raw_title FROM work_alias WHERE work_id = $1 ORDER BY raw_title")
            .bind(work)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[test]
    fn mangadex_rows_include_all_groups_and_keep_first_duplicate() {
        let input = WorkInput {
            primary_title: Some("Primary".into()),
            primary_lang: Some("en".into()),
            aliases: vec![
                Alias {
                    raw: "Alias".into(),
                    lang: Some("ja-ro".into()),
                },
                Alias {
                    raw: "Alias".into(),
                    lang: Some("ja-ro".into()),
                },
                Alias {
                    raw: " ".into(),
                    lang: None,
                },
            ],
            description: Some("Description".into()),
            descriptions: vec![("fr".into(), "Texte".into())],
            author: Some("Writer".into()),
            artist: Some(" ".into()),
            credits: vec![
                ("author".into(), "Writer".into()),
                ("author".into(), "Other".into()),
                ("artist".into(), "Artist".into()),
                ("editor".into(), "Ignored".into()),
            ],
            tags: vec!["Action".into(), "Action".into(), "Adventure".into()],
            tag_details: vec![SourceTag {
                id: Some("tag-id".into()),
                name: "Action".into(),
                group: "genre".into(),
            }],
            status: Some("ONGOING".into()),
            cover_file_name: Some("cover.jpg".into()),
            ..Default::default()
        };
        let observation = Observation::from_mangadex(&input);
        assert_eq!(observation.groups.len(), 6);
        assert_eq!(
            observation.groups[&FieldGroup::Title],
            vec![
                row("primary", "en", "Primary", 0),
                row("alias", "ja-ro", "Alias", 0)
            ]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Credits],
            vec![
                row("author", "", "Writer", 0),
                row("author", "", "Other", 2),
                row("artist", "", "Artist", 3)
            ]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Description],
            vec![
                row("primary", "", "Description", 0),
                row("localized", "fr", "Texte", 0)
            ]
        );
        assert_eq!(observation.groups[&FieldGroup::Tags].len(), 2);
        assert_eq!(
            observation.groups[&FieldGroup::Tags][0].detail,
            Some(serde_json::json!({"id": "tag-id", "group": "genre"}))
        );
        assert_eq!(observation.groups[&FieldGroup::Tags][1].ord, 2);
        assert_eq!(
            observation.groups[&FieldGroup::Status],
            vec![row("status", "", "ONGOING", 0)]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Cover],
            vec![row("cover", "", "cover.jpg", 0)]
        );
        assert!(Observation::from_mangadex(&WorkInput::default())
            .groups
            .values()
            .all(Vec::is_empty));
    }

    #[test]
    fn suwayomi_rows_keep_raw_status_and_observed_empty_groups() {
        let mut m = manga();
        let observation = Observation::from_suwayomi(&m);
        assert_eq!(
            observation.groups[&FieldGroup::Title],
            vec![
                row("primary", "", &m.title, 0),
                row("alias", "", &m.title, 0)
            ]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Tags],
            vec![row("tag", "", "Action", 0), row("tag", "", "Adventure", 3)]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Status],
            vec![row("status", "", "ON_HIATUS", 0)]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Description],
            vec![row("primary", "", "Description", 0)]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Credits],
            vec![
                row("author", "", "Writer", 0),
                row("artist", "", "Artist", 0)
            ]
        );
        assert_eq!(
            observation.groups[&FieldGroup::Cover],
            vec![row("cover", "", "cover.jpg", 0)]
        );
        m.author = None;
        m.artist = Some(" ".into());
        m.genre.clear();
        let observation = Observation::from_suwayomi(&m);
        assert!(observation.groups[&FieldGroup::Credits].is_empty());
        assert!(observation.groups[&FieldGroup::Tags].is_empty());
    }

    async fn versions(
        pool: &Database,
        mapping: &str,
    ) -> Vec<(
        String,
        String,
        i32,
        Option<serde_json::Value>,
        String,
        DateTime<Utc>,
        String,
    )> {
        sqlx::query_as("SELECT slot, value, ord, detail, attribution, observed_at, xmin::text FROM work_metadata_contribution WHERE source_series_id = $1 ORDER BY field_group, slot, lang, value")
            .bind(mapping).fetch_all(pool).await.unwrap()
    }

    #[tokio::test]
    async fn record_diff_preserves_unchanged_rows_and_marks_empty_groups() {
        let pool = pool().await;
        let work = work(&pool, "Original").await;
        let mapping = source(&pool, &work, "1").await;
        let mut observation = single_group(
            FieldGroup::Title,
            vec![
                row("primary", "en", "First", 0),
                row("alias", "en", "Alias", 1),
            ],
        );
        observation
            .groups
            .insert(FieldGroup::Description, Vec::new());
        assert!(differs_on(
            &mut pool.acquire().await.unwrap(),
            &mapping,
            &observation,
            Attribution::Observed
        )
        .await
        .unwrap());
        assert!(record(&pool, &work, &mapping, &observation, Attribution::Observed).await);
        let before = versions(&pool, &mapping).await;
        assert_eq!(before.len(), 3);
        assert!(before
            .iter()
            .any(|row| row.0.is_empty() && row.1.is_empty()));
        assert!(!differs_on(
            &mut pool.acquire().await.unwrap(),
            &mapping,
            &observation,
            Attribution::Observed
        )
        .await
        .unwrap());
        assert!(!record(&pool, &work, &mapping, &observation, Attribution::Observed).await);
        assert_eq!(
            versions(&pool, &mapping).await,
            before,
            "unchanged rows keep their xmin"
        );
        observation.groups.get_mut(&FieldGroup::Title).unwrap()[0].value = "Second".into();
        assert!(record(&pool, &work, &mapping, &observation, Attribution::Observed).await);
        let after = versions(&pool, &mapping).await;
        assert_eq!(
            before
                .iter()
                .filter(|row| row.0 != "primary")
                .collect::<Vec<_>>(),
            after
                .iter()
                .filter(|row| row.0 != "primary")
                .collect::<Vec<_>>()
        );
        assert!(!after.iter().any(|row| row.1 == "First"));
        assert_ne!(
            before.iter().find(|row| row.0 == "primary").unwrap().6,
            after.iter().find(|row| row.0 == "primary").unwrap().6
        );
        let mut alias = row("alias", "en", "Alias", 9);
        alias.detail = Some(serde_json::json!({"changed": true}));
        let changed = single_group(FieldGroup::Title, vec![alias]);
        assert!(record(&pool, &work, &mapping, &changed, Attribution::Inferred).await);
        let final_rows = versions(&pool, &mapping).await;
        let alias = final_rows.iter().find(|row| row.0 == "alias").unwrap();
        assert_eq!(
            (alias.2, &alias.3, alias.4.as_str()),
            (9, &Some(serde_json::json!({"changed": true})), "inferred")
        );
        assert_eq!(
            alias.5,
            before.iter().find(|row| row.0 == "alias").unwrap().5,
            "updates preserve observed_at"
        );
        assert_eq!(
            final_rows.iter().find(|row| row.0.is_empty()),
            before.iter().find(|row| row.0.is_empty()),
            "absent groups are untouched"
        );
    }

    #[tokio::test]
    async fn long_description_records_and_rerecords_without_rewriting() {
        use rand_core::RngCore;

        let pool = pool().await;
        let work = work(&pool, "Long description").await;
        let mapping = source(&pool, &work, "1").await;
        let mut random = vec![0u8; 20_000];
        rand_core::OsRng.fill_bytes(&mut random);
        let description: String = random
            .into_iter()
            .map(|byte| char::from(b'!' + byte % 94))
            .collect();
        let observation = single_group(
            FieldGroup::Description,
            vec![row("primary", "", &description, 0)],
        );
        assert!(record(&pool, &work, &mapping, &observation, Attribution::Observed).await);
        let before = versions(&pool, &mapping).await;
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].1, description);
        let valid_key: bool = sqlx::query_scalar(
            "SELECT value_key = md5(value) AND length(value) = 20000 \
             FROM work_metadata_contribution WHERE source_series_id = $1",
        )
        .bind(&mapping)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(valid_key);
        assert!(!differs_on(
            &mut pool.acquire().await.unwrap(),
            &mapping,
            &observation,
            Attribution::Observed,
        )
        .await
        .unwrap());
        assert!(!record(&pool, &work, &mapping, &observation, Attribution::Observed).await);
        assert_eq!(versions(&pool, &mapping).await, before);
    }

    #[tokio::test]
    async fn title_trust_rank_alias_union_and_cards_follow_bar_and_unbar() {
        let pool = pool().await;
        let work = work(&pool, "Admin alias").await;
        let sw = source(&pool, &work, "1").await;
        let md = mapping(
            &pool,
            &work,
            "mangadex",
            "mangadex",
            "md",
            "2030-01-01T00:00:00Z",
        )
        .await;
        let md_observation = single_group(
            FieldGroup::Title,
            vec![
                row("primary", "en", "MangaDex title", 0),
                row("alias", "en", "MangaDex exclusive", 1),
                row("alias", "en", "Shared name", 2),
            ],
        );
        let sw_observation = single_group(
            FieldGroup::Title,
            vec![
                row("primary", "", "Suwayomi title", 0),
                row("alias", "", "Suwayomi alias", 0),
                row("alias", "", "Shared name!", 1),
            ],
        );
        record(&pool, &work, &md, &md_observation, Attribution::Unknown).await;
        record(&pool, &work, &sw, &sw_observation, Attribution::Observed).await;
        sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, released_at) VALUES ($1, $1, 'Old card', now())").bind(&work).execute(&pool).await.unwrap();
        assert!(project(&pool, &work, &[FieldGroup::Title]).await.title);
        assert_eq!(
            title(&pool, &work).await,
            "MangaDex title",
            "MangaDex beats an older Suwayomi mapping even with unknown attribution"
        );
        let before = aliases(&pool, &work).await;
        assert!(before.contains(&"Suwayomi alias".into()));
        assert!(!project(&pool, &work, &[FieldGroup::Title]).await.any());
        assert_eq!(aliases(&pool, &work).await, before);
        assert_eq!(
            set_bar(&pool, "mangadex", FieldGroup::Title, true)
                .await
                .unwrap(),
            1
        );
        assert_eq!(title(&pool, &work).await, "Suwayomi title");
        let names = aliases(&pool, &work).await;
        assert!(!names.contains(&"MangaDex exclusive".into()));
        assert!(names.contains(&"Admin alias".into()));
        assert!(
            names.contains(&"Shared name".into()),
            "an allowed normalized title protects the alias"
        );
        let token: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work_alias_token WHERE work_id = $1 AND token = 'exclusive')").bind(&work).fetch_one(&pool).await.unwrap();
        assert!(!token);
        let cards: (String, String) = sqlx::query_as("SELECT f.title, b.title FROM feed_series_updates f JOIN browse_catalogue b USING(work_id) WHERE f.work_id = $1").bind(&work).fetch_one(&pool).await.unwrap();
        assert_eq!(cards, ("Suwayomi title".into(), "Suwayomi title".into()));
        set_bar(&pool, "mangadex", FieldGroup::Title, false)
            .await
            .unwrap();
        assert_eq!(title(&pool, &work).await, "MangaDex title");
        assert!(aliases(&pool, &work)
            .await
            .contains(&"MangaDex exclusive".into()));
        set_bar(&pool, "source", FieldGroup::Title, true)
            .await
            .unwrap();
        set_bar(&pool, "mangadex", FieldGroup::Title, true)
            .await
            .unwrap();
        assert_eq!(
            title(&pool, &work).await,
            "MangaDex title",
            "all barred keeps the scalar"
        );
    }

    #[tokio::test]
    async fn a_cut_off_bar_reprojection_resumes_from_its_cursor_and_a_newer_toggle_wins() {
        let pool = pool().await;
        let mut works = Vec::new();
        for (n, key) in [(1, "md-1"), (2, "md-2")] {
            let work = work(&pool, &format!("Legacy {n}")).await;
            let sw = source(&pool, &work, key).await;
            let md = mapping(
                &pool,
                &work,
                "mangadex",
                "mangadex",
                key,
                "2030-01-01T00:00:00Z",
            )
            .await;
            let md_title = format!("MangaDex {n}");
            let sw_title = format!("Suwayomi {n}");
            for (mapping, value) in [(&md, &md_title), (&sw, &sw_title)] {
                let observation =
                    single_group(FieldGroup::Title, vec![row("primary", "", value, 0)]);
                record(&pool, &work, mapping, &observation, Attribution::Observed).await;
            }
            project(&pool, &work, &[FieldGroup::Title]).await;
            works.push(work);
        }
        works.sort();
        let titles = |pool: Database, works: Vec<String>| async move {
            let mut out = Vec::new();
            for work in &works {
                out.push(title(&pool, work).await);
            }
            out
        };
        let pending = || async {
            sqlx::query_scalar::<_, String>(
                "SELECT value FROM instance_setting WHERE key = 'metadata_bar_reproject:mangadex:title'",
            )
            .fetch_optional(&pool)
            .await
            .unwrap()
        };

        // The request stored the bar and was cut off before any chunk ran.
        let (stale, linked) = record_bar(&pool, "mangadex", FieldGroup::Title, true)
            .await
            .unwrap();
        assert_eq!(linked, 2);
        let before = titles(pool.clone(), works.clone()).await;
        assert!(
            before.iter().all(|t| t.starts_with("MangaDex")),
            "{before:?}"
        );
        assert!(pending().await.is_some());
        // Re-running the same toggle restarts it; the older run then stops untouched.
        let (run, _) = record_bar(&pool, "mangadex", FieldGroup::Title, true)
            .await
            .unwrap();
        assert_eq!(
            reproject_bar(&pool, "mangadex", FieldGroup::Title, &stale)
                .await
                .unwrap(),
            0
        );
        assert_eq!(titles(pool.clone(), works.clone()).await, before);
        // A chunk that finished the first work before the cut.
        let cursor = serde_json::to_string(&PendingBar {
            run,
            after: works[0].clone(),
        })
        .unwrap();
        sqlx::query("UPDATE instance_setting SET value = $1 WHERE key = 'metadata_bar_reproject:mangadex:title'")
            .bind(&cursor).execute(&pool).await.unwrap();

        assert_eq!(resume_pending_bars(&pool).await.unwrap(), 1);
        let after = titles(pool.clone(), works.clone()).await;
        assert!(
            after[0].starts_with("MangaDex"),
            "the cursor skips done works: {after:?}"
        );
        assert!(after[1].starts_with("Suwayomi"), "{after:?}");
        assert!(pending().await.is_none());
        assert_eq!(resume_pending_bars(&pool).await.unwrap(), 0);
        // The full toggle again is idempotent and reaches every work.
        assert_eq!(
            set_bar(&pool, "mangadex", FieldGroup::Title, true)
                .await
                .unwrap(),
            2
        );
        let after = titles(pool.clone(), works.clone()).await;
        assert!(after.iter().all(|t| t.starts_with("Suwayomi")), "{after:?}");
        assert!(pending().await.is_none());
    }

    #[tokio::test]
    async fn unobserved_preferred_mapping_keeps_legacy_values() {
        let pool = pool().await;
        let work = work(&pool, "Legacy title").await;
        source(&pool, &work, "1").await;
        let later = mapping(
            &pool,
            &work,
            "suwayomi",
            "later",
            "2",
            "2030-01-01T00:00:00Z",
        )
        .await;
        let observed = Observation::from_mangadex(&WorkInput {
            primary_title: Some("New title".into()),
            author: Some("New author".into()),
            description: Some("New description".into()),
            status: Some("COMPLETED".into()),
            ..Default::default()
        });
        record(&pool, &work, &later, &observed, Attribution::Observed).await;
        sqlx::query("UPDATE work SET description = 'Legacy description', author = 'Legacy author', status = 'ONGOING' WHERE id = $1").bind(&work).execute(&pool).await.unwrap();
        project(&pool, &work, &PROJECTED_GROUPS).await;
        let values: (String, String, String, String) = sqlx::query_as(
            "SELECT primary_title, description, author, status FROM work WHERE id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            values,
            (
                "Legacy title".into(),
                "Legacy description".into(),
                "Legacy author".into(),
                "ONGOING".into()
            )
        );
    }

    #[tokio::test]
    async fn descriptions_and_credits_clean_only_barred_contributions() {
        let pool = pool().await;
        let work = work(&pool, "Children").await;
        let sw = source(&pool, &work, "1").await;
        let md = mapping(
            &pool,
            &work,
            "mangadex",
            "mangadex",
            "md",
            "2020-01-01T00:00:00Z",
        )
        .await;
        let md_input = WorkInput {
            description: Some("MangaDex description".into()),
            descriptions: vec![
                ("fr".into(), "MangaDex French".into()),
                ("de".into(), "Shared German".into()),
            ],
            author: Some("MangaDex author".into()),
            artist: Some("Shared artist".into()),
            credits: vec![("author".into(), "Second author".into())],
            ..Default::default()
        };
        let sw_observation = Observation {
            groups: [
                (
                    FieldGroup::Description,
                    vec![
                        row("localized", "de", "Shared German", 0),
                        row("localized", "es", "Suwayomi Spanish", 1),
                    ],
                ),
                (
                    FieldGroup::Credits,
                    vec![
                        row("author", "", "Suwayomi author", 0),
                        row("author", "", "Later author", 2),
                        row("artist", "", "Shared artist", 0),
                    ],
                ),
            ]
            .into_iter()
            .collect(),
        };
        record(
            &pool,
            &work,
            &md,
            &Observation::from_mangadex(&md_input),
            Attribution::Observed,
        )
        .await;
        record(&pool, &work, &sw, &sw_observation, Attribution::Observed).await;
        sqlx::query("INSERT INTO work_description (work_id, lang, description) VALUES ($1, 'it', 'Admin Italian')").bind(&work).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO work_credit (work_id, role, name) VALUES ($1, 'author', 'Admin author')",
        )
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
        project(
            &pool,
            &work,
            &[FieldGroup::Description, FieldGroup::Credits],
        )
        .await;
        set_bar(&pool, "mangadex", FieldGroup::Description, true)
            .await
            .unwrap();
        set_bar(&pool, "mangadex", FieldGroup::Credits, true)
            .await
            .unwrap();
        let scalars: (Option<String>, String, String) =
            sqlx::query_as("SELECT description, author, artist FROM work WHERE id = $1")
                .bind(&work)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            scalars,
            (None, "Suwayomi author".into(), "Shared artist".into()),
            "an observed group without a primary description clears it, and lowest credit ord wins"
        );
        let descriptions: Vec<(String, String)> = sqlx::query_as(
            "SELECT lang, description FROM work_description WHERE work_id = $1 ORDER BY lang",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            descriptions,
            vec![
                ("de".into(), "Shared German".into()),
                ("es".into(), "Suwayomi Spanish".into()),
                ("it".into(), "Admin Italian".into())
            ]
        );
        let credits: Vec<(String, String)> = sqlx::query_as(
            "SELECT role, name FROM work_credit WHERE work_id = $1 ORDER BY role, name",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            credits,
            vec![
                ("artist".into(), "Shared artist".into()),
                ("author".into(), "Admin author".into())
            ],
            "Suwayomi changes scalars without populating the MangaDex credit table"
        );
        set_bar(&pool, "mangadex", FieldGroup::Description, false)
            .await
            .unwrap();
        set_bar(&pool, "mangadex", FieldGroup::Credits, false)
            .await
            .unwrap();
        let restored: (String, String) =
            sqlx::query_as("SELECT description, author FROM work WHERE id = $1")
                .bind(&work)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            restored,
            ("MangaDex description".into(), "MangaDex author".into())
        );
        let credits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_credit WHERE work_id = $1")
                .bind(&work)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(credits, 4);
        set_bar(&pool, "source", FieldGroup::Credits, true)
            .await
            .unwrap();
        set_bar(&pool, "mangadex", FieldGroup::Credits, true)
            .await
            .unwrap();
        let kept: String = sqlx::query_scalar("SELECT author FROM work WHERE id = $1")
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, "MangaDex author");
        record(
            &pool,
            &work,
            &sw,
            &single_group(FieldGroup::Credits, Vec::new()),
            Attribution::Observed,
        )
        .await;
        set_bar(&pool, "source", FieldGroup::Credits, false)
            .await
            .unwrap();
        let empty: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT author, artist FROM work WHERE id = $1")
                .bind(&work)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            empty,
            (None, None),
            "observed empty credits clear both scalars"
        );
    }

    #[tokio::test]
    async fn observe_suwayomi_skips_unchanged_without_waiting_for_work_lock() {
        let pool = pool().await;
        let work = work(&pool, "Cached").await;
        let mapping = source(&pool, &work, "7").await;
        let mut m = manga();
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        observe_suwayomi(&pool, &m).await.unwrap();
        assert_eq!(title(&pool, &work).await, m.title);
        let before = versions(&pool, &mapping).await;
        let xmin: String = sqlx::query_scalar("SELECT xmin::text FROM work WHERE id = $1")
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[&work]).await.unwrap();
        // The read-only fast path does not even need to parse the generation token.
        m.engine_generation = Some("invalid generation".into());
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            observe_suwayomi(&pool, &m),
        )
        .await
        .expect("unchanged observation waited on the work lock")
        .unwrap();
        assert_eq!(versions(&pool, &mapping).await, before);
        let after: String = sqlx::query_scalar("SELECT xmin::text FROM work WHERE id = $1")
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, xmin);
        tx.rollback().await.unwrap();
        m.id = 8;
        observe_suwayomi(&pool, &m).await.unwrap();
    }

    #[tokio::test]
    async fn on_helpers_leave_every_projection_in_the_callers_transaction() {
        let pool = pool().await;
        let work = work(&pool, "Original").await;
        let mapping = mapping(
            &pool,
            &work,
            "mangadex",
            "mangadex",
            "md",
            "2020-01-01T00:00:00Z",
        )
        .await;
        sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, status, released_at) VALUES ($1, $1, 'Original', 'UNKNOWN', now())").bind(&work).execute(&pool).await.unwrap();
        let observation = Observation::from_mangadex(&WorkInput {
            primary_title: Some("Projected".into()),
            aliases: vec![Alias {
                raw: "Projected alias".into(),
                lang: None,
            }],
            tags: vec!["Action".into()],
            status: Some("COMPLETED".into()),
            ..Default::default()
        });
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[&work]).await.unwrap();
        record_on(&mut tx, &mapping, &observation, Attribution::Observed)
            .await
            .unwrap();
        let projected = project_on(&mut tx, &work, &PROJECTED_GROUPS).await.unwrap();
        assert!(projected.title && projected.aliases && projected.tags && projected.status);
        assert_eq!(title(&pool, &work).await, "Original");
        assert_eq!(aliases(&pool, &work).await, vec!["Original"]);
        assert!(versions(&pool, &mapping).await.is_empty());
        tx.rollback().await.unwrap();
        assert_eq!(title(&pool, &work).await, "Original");
        let state: (i64, String, String) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM work_tag WHERE work_id = $1), title, status FROM feed_series_updates WHERE work_id = $1").bind(&work).fetch_one(&pool).await.unwrap();
        assert_eq!(state, (0, "Original".into(), "UNKNOWN".into()));
        assert!(versions(&pool, &mapping).await.is_empty());
    }

    #[tokio::test]
    async fn pins_keep_columns_and_status_refreshes_both_cards() {
        let pool = pool().await;
        let work = work(&pool, "Original").await;
        let mapping = source(&pool, &work, "1").await;
        sqlx::query("UPDATE work SET author = 'Pinned author', artist = 'Old artist', title_override = 'Admin title', description_override = 'Admin description' WHERE id = $1").bind(&work).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at) VALUES ($1, 'author', 'Pinned author', 'Old source', now())").bind(&work).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, status, released_at) VALUES ($1, $1, 'Original', 'ONGOING', now())").bind(&work).execute(&pool).await.unwrap();
        let observed = Observation::from_mangadex(&WorkInput {
            primary_title: Some("Source title".into()),
            description: Some("Source description".into()),
            author: Some("Source author".into()),
            status: Some("ON_HIATUS".into()),
            ..Default::default()
        });
        record(&pool, &work, &mapping, &observed, Attribution::Observed).await;
        let changes = project(&pool, &work, &PROJECTED_GROUPS).await;
        assert!(changes.title && changes.status && changes.credits);
        let values: (String, String, Option<String>, String, String, String) = sqlx::query_as("SELECT w.author, p.source_value, w.artist, w.title_override, w.description_override, w.status FROM work w JOIN work_field_pin p ON p.work_id = w.id WHERE w.id = $1").bind(&work).fetch_one(&pool).await.unwrap();
        assert_eq!(
            values,
            (
                "Pinned author".into(),
                "Source author".into(),
                None,
                "Admin title".into(),
                "Admin description".into(),
                "ON_HIATUS".into()
            )
        );
        let cards: (String, String, String, String) = sqlx::query_as("SELECT f.status, b.status, f.title, b.title FROM feed_series_updates f JOIN browse_catalogue b USING(work_id) WHERE f.work_id = $1").bind(&work).fetch_one(&pool).await.unwrap();
        assert_eq!(
            cards,
            (
                "HIATUS".into(),
                "HIATUS".into(),
                "Admin title".into(),
                "Admin title".into()
            )
        );
        assert!(!project(&pool, &work, &PROJECTED_GROUPS).await.any());
        sqlx::query("INSERT INTO work_field_pin (work_id, field, value, source_value, pinned_at) VALUES ($1, 'status', 'ON_HIATUS', 'ON_HIATUS', now())").bind(&work).execute(&pool).await.unwrap();
        record(
            &pool,
            &work,
            &mapping,
            &single_group(FieldGroup::Status, vec![row("status", "", "COMPLETED", 0)]),
            Attribution::Observed,
        )
        .await;
        assert!(!project(&pool, &work, &[FieldGroup::Status]).await.status);
        let pinned: (String, String, String) = sqlx::query_as("SELECT w.status, p.source_value, f.status FROM work w JOIN work_field_pin p ON p.work_id = w.id AND p.field = 'status' JOIN feed_series_updates f ON f.work_id = w.id WHERE w.id = $1").bind(&work).fetch_one(&pool).await.unwrap();
        assert_eq!(
            pinned,
            ("ON_HIATUS".into(), "COMPLETED".into(), "HIATUS".into())
        );
    }

    #[tokio::test]
    async fn suwayomi_tag_bar_refreshes_browse_and_feed_comic_types() {
        let pool = pool().await;
        let work = work(&pool, "Format test").await;
        let mut m = manga();
        m.title = "Format test".into();
        m.genre = vec!["Manhwa".into()];
        crate::series_cache::put_series(&pool, &m).await.unwrap();
        source(&pool, &work, "7").await;
        sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, released_at) VALUES ($1, '7', 'Format test', now())")
            .bind(&work).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[&work]).await.unwrap();
        crate::catalog::upsert_browse_work_inner(&mut tx, &work, false)
            .await
            .unwrap();
        refresh_comic_types_on(&mut tx, &work).await.unwrap();
        tx.commit().await.unwrap();
        for (barred, expected) in [(false, "MANHWA"), (true, "MANGA"), (false, "MANHWA")] {
            set_bar(&pool, "source", FieldGroup::Tags, barred)
                .await
                .unwrap();
            let types: (String, String) = sqlx::query_as(
                "SELECT b.comic_type, f.comic_type FROM browse_catalogue b \
                 JOIN feed_series_updates f USING (work_id) WHERE b.work_id = $1",
            )
            .bind(&work)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(types, (expected.into(), expected.into()));
        }
    }

    #[tokio::test]
    async fn tag_projection_refreshes_browse_and_feed_comic_types() {
        let pool = pool().await;
        let work = work(&pool, "Format test").await;
        let mapping = mapping(&pool, &work, "mangadex", "mangadex", "md", "2020-01-01").await;
        sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, released_at) VALUES ($1, $1, 'Format test', now())")
            .bind(&work).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[&work]).await.unwrap();
        crate::catalog::upsert_browse_work_inner(&mut tx, &work, false)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let observation = single_group(FieldGroup::Tags, vec![row("tag", "", "Manhwa", 0)]);
        record(&pool, &work, &mapping, &observation, Attribution::Observed).await;
        assert!(project(&pool, &work, &[FieldGroup::Tags]).await.tags);
        let types: (String, String) = sqlx::query_as(
            "SELECT b.comic_type, f.comic_type FROM browse_catalogue b \
             JOIN feed_series_updates f USING (work_id) WHERE b.work_id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(types, ("MANHWA".into(), "MANHWA".into()));
        sqlx::query("INSERT INTO source_metadata_bar (source_id, field_group, created_at) VALUES ('mangadex', 'tags', now())")
            .execute(&pool).await.unwrap();
        assert!(project(&pool, &work, &[FieldGroup::Tags]).await.tags);
        let types: (String, String) = sqlx::query_as(
            "SELECT b.comic_type, f.comic_type FROM browse_catalogue b \
             JOIN feed_series_updates f USING (work_id) WHERE b.work_id = $1",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(types, ("MANGA".into(), "MANGA".into()));
    }

    #[tokio::test]
    async fn mangadex_tags_restore_detail_and_keep_admin_rows() {
        let pool = pool().await;
        let work = work(&pool, "Tags").await;
        let mapping = mapping(
            &pool,
            &work,
            "mangadex",
            "mangadex",
            "md",
            "2020-01-01T00:00:00Z",
        )
        .await;
        let input = WorkInput {
            tags: vec!["Action".into(), "Adventure".into()],
            tag_details: vec![SourceTag {
                id: Some("action-id".into()),
                name: "Action".into(),
                group: "theme".into(),
            }],
            ..Default::default()
        };
        record(
            &pool,
            &work,
            &mapping,
            &Observation::from_mangadex(&input),
            Attribution::Observed,
        )
        .await;
        sqlx::query("INSERT INTO work_tag (work_id, tag, source) VALUES ($1, 'Admin', 'admin')")
            .bind(&work)
            .execute(&pool)
            .await
            .unwrap();
        assert!(project(&pool, &work, &[FieldGroup::Tags]).await.tags);
        let before: Vec<(String, i64, Option<String>, Option<String>)> = sqlx::query_as("SELECT tag, ord, tag_id, tag_group FROM work_tag WHERE work_id = $1 AND source = 'mangadex' ORDER BY ord").bind(&work).fetch_all(&pool).await.unwrap();
        assert_eq!(
            before,
            vec![
                (
                    "Action".into(),
                    0,
                    Some("action-id".into()),
                    Some("theme".into())
                ),
                ("Adventure".into(), 1, None, None)
            ]
        );
        set_bar(&pool, "mangadex", FieldGroup::Tags, true)
            .await
            .unwrap();
        let tags: Vec<String> = sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id = $1")
            .bind(&work)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(tags, vec!["Admin"]);
        set_bar(&pool, "mangadex", FieldGroup::Tags, false)
            .await
            .unwrap();
        let restored: Vec<(String, i64, Option<String>, Option<String>)> = sqlx::query_as("SELECT tag, ord, tag_id, tag_group FROM work_tag WHERE work_id = $1 AND source = 'mangadex' ORDER BY ord").bind(&work).fetch_all(&pool).await.unwrap();
        assert_eq!(restored, before);
    }

    #[tokio::test]
    async fn source_states_cover_no_source_not_observed_barred_and_attribution() {
        let pool = pool().await;
        let work = work(&pool, "States").await;
        assert!(work_sources(&pool, &work)
            .await
            .unwrap()
            .iter()
            .all(|source| source.state == SourceState::NoSource));
        let mapping = source(&pool, &work, "1").await;
        let states = work_sources(&pool, &work).await.unwrap();
        assert!(states
            .iter()
            .all(|source| source.state == SourceState::KeptNotObserved
                && source.source_series_id.as_deref() == Some(mapping.as_str())
                && source.source_id.as_deref() == Some("source")
                && source.attribution.is_none()));
        record(
            &pool,
            &work,
            &mapping,
            &single_group(
                FieldGroup::Title,
                vec![row("primary", "", "Title", 0), row("alias", "", "Alias", 1)],
            ),
            Attribution::Inferred,
        )
        .await;
        record(
            &pool,
            &work,
            &mapping,
            &single_group(FieldGroup::Description, Vec::new()),
            Attribution::Unknown,
        )
        .await;
        let states = work_sources(&pool, &work).await.unwrap();
        assert_eq!(states[0].attribution, Some(Attribution::Inferred));
        assert_eq!(
            states[1].state,
            SourceState::Source,
            "marker rows are observed"
        );
        assert_eq!(states[1].attribution, Some(Attribution::Unknown));
        sqlx::query("UPDATE work_metadata_contribution SET attribution = 'unknown' WHERE source_series_id = $1 AND slot = 'alias'").bind(&mapping).execute(&pool).await.unwrap();
        assert_eq!(
            work_sources(&pool, &work).await.unwrap()[0].attribution,
            Some(Attribution::Unknown),
            "mixed attribution reports the least certain row"
        );
        for (primary, alias, expected) in [
            ("observed", "inferred", Attribution::Inferred),
            ("inferred", "observed", Attribution::Inferred),
            ("unknown", "observed", Attribution::Unknown),
            ("observed", "observed", Attribution::Observed),
        ] {
            sqlx::query("UPDATE work_metadata_contribution SET attribution = CASE slot WHEN 'primary' THEN $2 ELSE $3 END WHERE source_series_id = $1 AND field_group = 'title'")
                .bind(&mapping).bind(primary).bind(alias).execute(&pool).await.unwrap();
            assert_eq!(
                work_sources(&pool, &work).await.unwrap()[0].attribution,
                Some(expected)
            );
        }
        sqlx::query("INSERT INTO work_metadata_contribution (source_series_id, field_group, slot, value, attribution, observed_at) VALUES ($1, 'title', 'alias', 'Uncertain alias', 'unknown', now())")
            .bind(&mapping).execute(&pool).await.unwrap();
        assert_eq!(
            work_sources(&pool, &work).await.unwrap()[0].attribution,
            Some(Attribution::Unknown),
            "one unknown row outweighs several observed rows"
        );
        set_bar(&pool, "source", FieldGroup::Title, true)
            .await
            .unwrap();
        let state = work_sources(&pool, &work).await.unwrap().remove(0);
        assert_eq!(state.state, SourceState::KeptAllBarred);
        assert!(state.source_id.is_none() && state.attribution.is_none());
        assert_eq!(
            barred_groups_on(&mut pool.acquire().await.unwrap(), "source")
                .await
                .unwrap(),
            HashSet::from([FieldGroup::Title])
        );
        let bars = list_bars(&pool).await.unwrap();
        assert_eq!(
            (bars[0].0.as_str(), bars[0].1),
            ("source", FieldGroup::Title)
        );
    }

    #[tokio::test]
    async fn set_bar_projects_multiple_chunks_and_persists_without_works() {
        let pool = pool().await;
        assert_eq!(
            set_bar(&pool, "empty", FieldGroup::Title, true)
                .await
                .unwrap(),
            0
        );
        assert_eq!(list_bars(&pool).await.unwrap().len(), 1);
        assert_eq!(
            set_bar(&pool, "empty", FieldGroup::Title, false)
                .await
                .unwrap(),
            0
        );
        assert!(list_bars(&pool).await.unwrap().is_empty());
        // Bulk fixtures keep this a policy-chunk test rather than 150 catalog mints.
        sqlx::query("INSERT INTO work (id, primary_title, created_at, updated_at) SELECT 'w_chunk_' || lpad(i::text, 3, '0'), 'Original', now(), now() FROM generate_series(1, 150) i").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) SELECT 'ss_md_' || i, 'w_chunk_' || lpad(i::text, 3, '0'), 'mangadex', 'mangadex', i::text, now() FROM generate_series(1, 150) i").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) SELECT 'ss_sw_' || i, 'w_chunk_' || lpad(i::text, 3, '0'), 'suwayomi', 'fallback', i::text, now() FROM generate_series(1, 150) i").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO work_metadata_contribution (source_series_id, field_group, slot, lang, value, attribution, observed_at) SELECT id, 'title', 'primary', '', CASE source_type WHEN 'mangadex' THEN 'MangaDex' ELSE 'Fallback' END, 'observed', now() FROM source_series").execute(&pool).await.unwrap();
        assert_eq!(
            set_bar(&pool, "mangadex", FieldGroup::Title, true)
                .await
                .unwrap(),
            150
        );
        let fallback: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE primary_title = 'Fallback'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(fallback, 150);
        assert_eq!(
            set_bar(&pool, "mangadex", FieldGroup::Title, false)
                .await
                .unwrap(),
            150
        );
        let restored: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE primary_title = 'MangaDex'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(restored, 150);
    }
}
