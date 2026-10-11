//! Content block rules through the API: who may read and write them, and that every
//! surface which lists or opens a series obeys them. The rule logic itself is tested in
//! `block.rs`; this file is about the resolvers.

use super::sources_tests::{err, fixture, mock_engine, ok, run};
use super::*;
use serde_json::Value;

/// One MangaDex-anchored work with a dated English chapter, so it is in Browse and in
/// both updates feeds. Returns its work id.
async fn seed_work(pool: &Database, key: &str, title: &str, rating: &str, tags: &[&str]) -> String {
    let work_id = catalog::upsert_work_from_mangadex(
        pool,
        key,
        &catalog::WorkInput {
            primary_title: Some(title.to_string()),
            original_language: Some("ja".into()),
            status: Some("ongoing".into()),
            content_rating: Some(rating.to_string()),
            is_nsfw: matches!(rating, "erotica" | "pornographic"),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let source_series = catalog::find_source_series_id(pool, "mangadex", "mangadex", key)
        .await
        .unwrap()
        .unwrap();
    catalog::upsert_chapter(
        pool,
        &source_series,
        &catalog::ChapterInput {
            external_id: format!("{key}-c1"),
            number: Some("1".into()),
            lang: Some("en".into()),
            published_at: Some("2026-07-01T00:00:00.000000+00:00".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    work_id
}

/// The three works every test here reads: one nothing should ever hide, one tagged
/// Gore and rated suggestive, one tagged Gore and Action and rated safe.
struct Works {
    plain: String,
    gore: String,
    gore_action: String,
}

async fn seed(pool: &Database) -> Works {
    let works = Works {
        plain: seed_work(pool, "md-plain", "Plain Chronicle", "safe", &["Action"]).await,
        gore: seed_work(pool, "md-gore", "Gore Chronicle", "suggestive", &["Gore"]).await,
        gore_action: seed_work(
            pool,
            "md-both",
            "Blade Chronicle",
            "safe",
            &["Gore", "Action"],
        )
        .await,
    };
    catalog::refresh_feed_updates(pool).await.unwrap();
    works
}

/// The titles a list query returns, sorted. `path` is the list's field, and `items` its
/// rows when it has that field.
fn titles(data: &Value, path: &str) -> Vec<String> {
    let list = &data[path];
    let rows = list.get("items").unwrap_or(list).as_array().unwrap();
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| r["title"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

const ALL: [&str; 3] = ["Blade Chronicle", "Gore Chronicle", "Plain Chronicle"];

/// Every reader list that pages in SQL, with its total where it has one.
const LISTS: [(&str, &str); 6] = [
    (
        "search",
        r#"{ search(query: "") { total items { title } } }"#,
    ),
    (
        "search",
        r#"{ search(query: "", filter: {}) { total items { title } } }"#,
    ),
    ("updatesFeed", "{ updatesFeed { total items { title } } }"),
    ("canonicalUpdates", "{ canonicalUpdates { title } }"),
    // The two text branches, on the word every seeded title shares.
    (
        "search",
        r#"{ search(query: "Chronicle") { total items { title } } }"#,
    ),
    (
        "search",
        r#"{ search(query: "Chronicle", filter: {}) { total items { title } } }"#,
    ),
];

async fn listed(s: &ApiSchema, query: &str, path: &str, token: Option<&str>) -> Vec<String> {
    let data = ok(run(s, query, token).await);
    let got = titles(&data, path);
    // A total, where there is one, counts exactly the rows: these lists fit one page.
    if let Some(total) = data[path].get("total").and_then(Value::as_i64) {
        assert_eq!(total as usize, got.len(), "{query}: total");
    }
    got
}

/// Every title on every `discovery` rail.
async fn discovery_titles(s: &ApiSchema, token: Option<&str>) -> Vec<String> {
    let rails = ok(run(s, "{ discovery { items { title } } }", token).await);
    rails["discovery"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|rail| rail["items"].as_array().unwrap())
        .map(|item| item["title"].as_str().unwrap().to_string())
        .collect()
}

async fn set_site(s: &ApiSchema, rules: &str) -> Value {
    ok(run(
        s,
        &format!("mutation {{ setSiteBlockRules(rules: {rules}) {{ tags ratings }} }}"),
        Some("admintok"),
    )
    .await)["setSiteBlockRules"]
        .clone()
}

async fn set_mine(s: &ApiSchema, rules: &str, token: &str) -> Value {
    ok(run(
        s,
        &format!("mutation {{ setMyBlockRules(rules: {rules}) {{ tags ratings }} }}"),
        Some(token),
    )
    .await)["setMyBlockRules"]
        .clone()
}

#[tokio::test]
async fn block_rules_are_scoped_validated_and_stored_whole() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;

    // The site's rules are the admin's to read and write. A user's are their own.
    const SITE_Q: &str = "{ siteBlockRules { tags ratings } }";
    const MINE_Q: &str = "{ myBlockRules { tags ratings } }";
    const SITE_SET: &str =
        r#"mutation { setSiteBlockRules(rules: [{ tags: ["gore"] }]) { tags } }"#;
    const MINE_SET: &str = r#"mutation { setMyBlockRules(rules: [{ tags: ["gore"] }]) { tags } }"#;
    for q in [SITE_Q, SITE_SET] {
        assert_eq!(err(&run(&s, q, None).await), "Not authenticated", "{q}");
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required",
            "{q}"
        );
    }
    for q in [MINE_Q, MINE_SET] {
        assert_eq!(err(&run(&s, q, None).await), "Not authenticated", "{q}");
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM content_block_rule")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "a refused write stores nothing");

    // Stored canonical: tags sorted, ratings mildest first, a repeated rule once.
    let rules = set_mine(
        &s,
        r#"[
            { tags: ["isekai", "harem"], ratings: [PORNOGRAPHIC, EROTICA] },
            { ratings: [PORNOGRAPHIC] },
            { tags: ["harem", "isekai"], ratings: [EROTICA, PORNOGRAPHIC] }
        ]"#,
        "bobtok",
    )
    .await;
    assert_eq!(
        rules,
        serde_json::json!([
            { "tags": ["harem", "isekai"], "ratings": ["EROTICA", "PORNOGRAPHIC"] },
            { "tags": [], "ratings": ["PORNOGRAPHIC"] },
        ])
    );
    assert_eq!(
        ok(run(&s, MINE_Q, Some("bobtok")).await)["myBlockRules"],
        rules
    );
    // One user's rules are not another's, and not the site's.
    assert_eq!(
        ok(run(&s, MINE_Q, Some("admintok")).await)["myBlockRules"],
        serde_json::json!([])
    );
    assert_eq!(
        ok(run(&s, SITE_Q, Some("admintok")).await)["siteBlockRules"],
        serde_json::json!([])
    );

    // A rule that is refused leaves the stored list as it was.
    for (bad, message) in [
        ("[{}]", "A rule needs a tag or a content rating."),
        (
            "[{ tags: [], ratings: [] }]",
            "A rule needs a tag or a content rating.",
        ),
        (r#"[{ tags: ["Gore"] }]"#, "\"Gore\" is not a tag."),
        (
            r#"[{ tags: ["x' OR 1=1 --"] }]"#,
            "\"x' OR 1=1 --\" is not a tag.",
        ),
        (
            "[{ ratings: [SAFE, SUGGESTIVE, EROTICA, PORNOGRAPHIC] }]",
            "A rule with every content rating and no tag would block everything.",
        ),
        (
            r#"[{ tags: ["a","b","c","d","e","f","g","h","i"] }]"#,
            "A rule can name at most 8 tags.",
        ),
    ] {
        let q = format!("mutation {{ setMyBlockRules(rules: {bad}) {{ tags }} }}");
        assert_eq!(err(&run(&s, &q, Some("bobtok")).await), message, "{bad}");
    }
    let many = (0..=crate::block::MAX_RULES)
        .map(|n| format!(r#"{{ tags: ["t{n}"] }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let q = format!("mutation {{ setMyBlockRules(rules: [{many}]) {{ tags }} }}");
    assert_eq!(err(&run(&s, &q, Some("bobtok")).await), "At most 50 rules.");
    assert_eq!(
        ok(run(&s, MINE_Q, Some("bobtok")).await)["myBlockRules"],
        rules
    );

    // An empty list clears the scope.
    assert_eq!(set_mine(&s, "[]", "bobtok").await, serde_json::json!([]));
    assert_eq!(
        ok(run(&s, MINE_Q, Some("bobtok")).await)["myBlockRules"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn a_site_rule_hides_a_work_from_every_list_and_closes_its_doors() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let works = seed(&st.pool).await;
    // The reader has the Gore work in their library from before the rule.
    sqlx::query(
        "INSERT INTO user_library (user_id, series_id, created_at) \
         VALUES ('ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c', $1, '2026-01-01T00:00:00Z'), ('ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c', $2, '2026-01-02T00:00:00Z')",
    )
    .bind(&works.gore)
    .bind(&works.plain)
    .execute(&st.pool)
    .await
    .unwrap();

    // `discovery` reads the engine cache first; with one series in it, it does not go
    // to the (mock) engine for a live browse.
    seed_engine_series(&st.pool, 901, "Engine Plain", &["Action"]).await;

    // No rule: every list has all three, for a visitor and for a reader.
    for (path, query) in LISTS {
        for token in [None, Some("bobtok")] {
            assert_eq!(listed(&s, query, path, token).await, ALL, "{query}");
        }
    }
    let shown = discovery_titles(&s, None).await;
    assert!(shown.contains(&"Gore Chronicle".to_string()), "{shown:?}");

    assert_eq!(
        set_site(&s, r#"[{ tags: ["gore"] }]"#).await,
        serde_json::json!([{ "tags": ["gore"], "ratings": [] }])
    );

    // Both Gore works are gone from every list, for everyone, with honest totals.
    for (path, query) in LISTS {
        for token in [None, Some("bobtok"), Some("admintok")] {
            assert_eq!(
                listed(&s, query, path, token).await,
                ["Plain Chronicle"],
                "{query} as {token:?}"
            );
        }
    }
    // A title search for the blocked work finds nothing, on both search paths.
    for query in [
        r#"{ search(query: "Gore") { total items { title } } }"#,
        r#"{ search(query: "Gore", filter: {}) { total items { title } } }"#,
    ] {
        assert!(
            listed(&s, query, "search", None).await.is_empty(),
            "{query}"
        );
    }
    // The discovery rails drop it per request.
    let shown = discovery_titles(&s, None).await;
    assert!(shown.contains(&"Plain Chronicle".to_string()), "{shown:?}");
    for hidden in ["Gore Chronicle", "Blade Chronicle"] {
        assert!(!shown.contains(&hidden.to_string()), "{shown:?}");
    }
    // A library entry is hidden too: the site does not serve the work.
    let library = ok(run(&s, "{ library { title } }", Some("bobtok")).await);
    assert_eq!(titles(&library, "library"), ["Plain Chronicle"]);

    // The console's own views still see it: an admin has to be able to fix the tags.
    for (path, query) in [
        (
            "search",
            r#"{ search(query: "", includeNsfw: true) { total items { title } } }"#,
        ),
        (
            "canonicalUpdates",
            "{ canonicalUpdates(includeNsfw: true) { title } }",
        ),
    ] {
        assert_eq!(
            listed(&s, query, path, Some("admintok")).await,
            ALL,
            "{query}"
        );
        // The same argument from a reader changes nothing.
        assert_eq!(
            listed(&s, query, path, Some("bobtok")).await,
            ["Plain Chronicle"],
            "{query}"
        );
    }

    // The doors. A blocked work answers like one that does not exist.
    let gore = &works.gore;
    for q in [
        format!(r#"{{ canonicalSeries(workId: "{gore}") {{ id }} }}"#),
        format!(r#"{{ canonicalChapters(workId: "{gore}") {{ id }} }}"#),
        format!(r#"{{ aggregatedChapters(workId: "{gore}") {{ number }} }}"#),
        format!(r#"{{ seriesPage(workId: "{gore}") {{ workId }} }}"#),
    ] {
        for token in [None, Some("bobtok"), Some("admintok")] {
            assert_eq!(err(&run(&s, &q, token).await), "No such work", "{q}");
        }
    }
    // The second Gore work is behind the same rule.
    let q = format!(
        r#"{{ canonicalSeries(workId: "{}") {{ id }} }}"#,
        works.gore_action
    );
    assert_eq!(err(&run(&s, &q, None).await), "No such work");
    let q = format!(r#"{{ workSources(workId: "{gore}") {{ sourceType }} }}"#);
    assert_eq!(
        ok(run(&s, &q, None).await)["workSources"],
        serde_json::json!([])
    );
    let q = format!(
        r#"{{ workSourcesBatch(workIds: ["{gore}", "{}"]) {{ workId sources {{ sourceType }} }} }}"#,
        works.plain
    );
    let batch = ok(run(&s, &q, None).await);
    assert_eq!(
        batch["workSourcesBatch"][0]["sources"],
        serde_json::json!([])
    );
    assert_eq!(
        batch["workSourcesBatch"][1]["sources"][0]["sourceType"],
        "mangadex"
    );
    // Its chapter pages are refused before any request to MangaDex is made.
    st.mangadex.set_enabled(true);
    let q = r#"{ canonicalPages(chapterId: "md-gore-c1") { index } }"#;
    assert_eq!(err(&run(&s, q, None).await), "No such chapter");
    // So is a chapter the mirror does not hold: while a rule exists, a chapter with no
    // known work is not fetched, or a blocked work's newest chapters would be.
    let q = r#"{ canonicalPages(chapterId: "md-not-mirrored") { index } }"#;
    assert_eq!(err(&run(&s, q, None).await), "No such chapter");
    st.mangadex.set_enabled(false);

    // What hangs off the series page is closed with it: its reviews and its thread.
    for q in [
        format!(r#"{{ reviews(seriesId: "{gore}") {{ items {{ id }} }} }}"#),
        format!(r#"{{ myReview(seriesId: "{gore}") {{ id }} }}"#),
        format!(r#"{{ comments(targetType: "series", targetId: "{gore}") {{ items {{ id }} }} }}"#),
    ] {
        assert_eq!(
            err(&run(&s, &q, Some("bobtok")).await),
            "No such work",
            "{q}"
        );
    }
    // The library mutations echo the series back, so they are doors too. Nothing is
    // written: the entry from before the rule is still there, unchanged.
    for q in [
        format!(r#"mutation {{ mark(seriesId: "{gore}", marked: false) {{ title }} }}"#),
        format!(
            r#"mutation {{ setLibraryStatus(seriesId: "{gore}", status: "plan") {{ title }} }}"#
        ),
        format!(r#"mutation {{ setFavorite(seriesId: "{gore}", favorite: true) {{ title }} }}"#),
    ] {
        assert_eq!(
            err(&run(&s, &q, Some("bobtok")).await),
            "No such work",
            "{q}"
        );
    }
    let kept: (Option<String>, i64) = crate::db_types::query_tuple(
        "SELECT status, is_favorite FROM user_library WHERE user_id = 'ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c' AND series_id = $1",
    )
    .bind(gore)
    .fetch_one(&st.pool)
    .await
    .unwrap();
    assert_eq!(kept, (None, 0));
    // The same calls on a work that is not blocked go through.
    let q = format!(
        r#"mutation {{ setFavorite(seriesId: "{}", favorite: true) {{ title }} }}"#,
        works.plain
    );
    assert_eq!(
        ok(run(&s, &q, Some("bobtok")).await)["setFavorite"]["title"],
        "Plain Chronicle"
    );
    let q = format!(
        r#"{{ reviews(seriesId: "{}") {{ items {{ id }} }} }}"#,
        works.plain
    );
    ok(run(&s, &q, None).await);

    // The work that is not blocked still opens.
    let q = format!(
        r#"{{ canonicalSeries(workId: "{}") {{ title }} }}"#,
        works.plain
    );
    assert_eq!(
        ok(run(&s, &q, None).await)["canonicalSeries"]["title"],
        "Plain Chronicle"
    );

    // Removing the rules brings everything back at once: nothing is memoized.
    assert_eq!(set_site(&s, "[]").await, serde_json::json!([]));
    for (path, query) in LISTS {
        assert_eq!(listed(&s, query, path, None).await, ALL, "{query}");
    }
    let q = format!(r#"{{ canonicalSeries(workId: "{gore}") {{ title }} }}"#);
    assert_eq!(
        ok(run(&s, &q, None).await)["canonicalSeries"]["title"],
        "Gore Chronicle"
    );
}

#[tokio::test]
async fn a_users_own_rule_hides_from_their_lists_and_from_nothing_else() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let works = seed(&st.pool).await;
    sqlx::query(
        "INSERT INTO user_library (user_id, series_id, created_at) \
         VALUES ('ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c', $1, '2026-01-01T00:00:00Z')",
    )
    .bind(&works.gore)
    .execute(&st.pool)
    .await
    .unwrap();

    // A tag and a rating together: Gore, but only the suggestive one.
    set_mine(
        &s,
        r#"[{ tags: ["gore"], ratings: [SUGGESTIVE] }]"#,
        "bobtok",
    )
    .await;
    for (path, query) in LISTS {
        assert_eq!(
            listed(&s, query, path, Some("bobtok")).await,
            ["Blade Chronicle", "Plain Chronicle"],
            "{query}"
        );
        // Nobody else is affected.
        for token in [None, Some("admintok")] {
            assert_eq!(listed(&s, query, path, token).await, ALL, "{query}");
        }
    }

    // Two tags in one rule: a work needs both.
    set_mine(&s, r#"[{ tags: ["gore", "action"] }]"#, "bobtok").await;
    for (path, query) in LISTS {
        assert_eq!(
            listed(&s, query, path, Some("bobtok")).await,
            ["Gore Chronicle", "Plain Chronicle"],
            "{query}"
        );
    }

    // A rating alone. A user's rules add to the site's.
    set_mine(&s, "[{ ratings: [SUGGESTIVE] }]", "bobtok").await;
    set_site(&s, r#"[{ tags: ["action"], ratings: [SAFE] }]"#).await;
    for (path, query) in LISTS {
        assert!(
            listed(&s, query, path, Some("bobtok")).await.is_empty(),
            "{query}"
        );
        assert_eq!(
            listed(&s, query, path, None).await,
            ["Gore Chronicle"],
            "{query}"
        );
    }
    set_site(&s, "[]").await;

    // Their own rule does not lock them out of a series they open by its link, and it
    // does not take a series out of their library.
    let gore = &works.gore;
    for q in [
        format!(r#"{{ canonicalSeries(workId: "{gore}") {{ id }} }}"#),
        format!(r#"{{ canonicalChapters(workId: "{gore}") {{ id }} }}"#),
        format!(r#"{{ seriesPage(workId: "{gore}") {{ workId }} }}"#),
    ] {
        ok(run(&s, &q, Some("bobtok")).await);
    }
    let q = format!(r#"{{ workSources(workId: "{gore}") {{ sourceType }} }}"#);
    assert_eq!(
        ok(run(&s, &q, Some("bobtok")).await)["workSources"][0]["sourceType"],
        "mangadex"
    );
    let library = ok(run(&s, "{ library { title } }", Some("bobtok")).await);
    assert_eq!(titles(&library, "library"), ["Gore Chronicle"]);
}

/// Browse reads a COPY of the rating (`browse_catalogue.content_rating`). An admin edit
/// that changes the rating, or the NSFW flag that lifts it, has to reach that copy at
/// once: a rule on a rating must take the work off every list in the same moment its
/// page stops opening, not at the next catalogue refresh.
#[tokio::test]
async fn an_admin_edit_that_changes_the_rating_reaches_a_rule_at_once() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let works = seed(&st.pool).await;
    // A reader who sees adult works, so only a rule can hide one.
    sqlx::query(
        r#"UPDATE users SET show_nsfw = TRUE WHERE id = 'ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c'"#,
    )
    .execute(&st.pool)
    .await
    .unwrap();
    set_site(&s, "[{ ratings: [EROTICA] }]").await;
    for (path, query) in LISTS {
        assert_eq!(
            listed(&s, query, path, Some("bobtok")).await,
            ALL,
            "{query}"
        );
    }

    // A new rating.
    let q = format!(
        r#"mutation {{ updateSeriesMetadata(input: {{ seriesId: "{}", contentRating: EROTICA }}) {{ id }} }}"#,
        works.plain
    );
    ok(run(&s, &q, Some("admintok")).await);
    for (path, query) in LISTS {
        assert_eq!(
            listed(&s, query, path, Some("bobtok")).await,
            ["Blade Chronicle", "Gore Chronicle"],
            "{query}"
        );
    }

    // The NSFW flag alone: a suggestive work an admin marks adult counts as erotica.
    let q = format!(
        r#"mutation {{ updateSeriesMetadata(input: {{ seriesId: "{}", isNsfw: true }}) {{ id }} }}"#,
        works.gore
    );
    ok(run(&s, &q, Some("admintok")).await);
    for (path, query) in LISTS {
        assert_eq!(
            listed(&s, query, path, Some("bobtok")).await,
            ["Blade Chronicle"],
            "{query}"
        );
    }
    for work in [&works.plain, &works.gore] {
        let q = format!(r#"{{ canonicalSeries(workId: "{work}") {{ id }} }}"#);
        assert_eq!(err(&run(&s, &q, Some("bobtok")).await), "No such work");
    }
}

/// A series that only an engine source carries: a numeric id, mapped to a work.
async fn seed_engine_series(pool: &Database, id: i64, title: &str, tags: &[&str]) {
    let work_id = format!("w_engine{id}");
    sqlx::query(
        "INSERT INTO work (id, primary_title, created_at, updated_at) \
         VALUES ($1, $2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(&work_id)
    .bind(title)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ($1, $2, 'suwayomi', '77', $3, '2026-01-01T00:00:00Z')",
    )
    .bind(format!("ss{id}"))
    .bind(&work_id)
    .bind(id.to_string())
    .execute(pool)
    .await
    .unwrap();
    for tag in tags {
        sqlx::query(
            "INSERT INTO work_tag (work_id, tag, tag_key, source) VALUES ($1, $2, $3, 'suwayomi')",
        )
        .bind(&work_id)
        .bind(tag)
        .bind(crate::tags::slug(tag))
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        r#"INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, in_library, latest_chapter_at, updated_at) VALUES ($1, $2, 'ONGOING', '77', 5, TRUE, '1750000000000', '2026-07-15T00:00:00+00:00')"#,
    )
    .bind(id)
    .bind(title)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO series_scan_state \
           (series_id, avg_interval_hours, known_chapter_count, last_new_chapter_at, updated_at, next_scan_at) \
         VALUES ($1, 0, 5, '2026-07-15T00:00:00+00:00', '2026-07-15T00:00:00+00:00', '1970-01-01T00:00:00+00:00')",
    )
    .bind(id.to_string())
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn rules_reach_a_series_that_only_an_engine_source_carries() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed_engine_series(&st.pool, 901, "Engine Plain", &["Action"]).await;
    seed_engine_series(&st.pool, 902, "Engine Gore", &["Gore"]).await;

    const UPDATES_Q: &str = "{ updates { total items { title } } }";
    let both = ["Engine Gore", "Engine Plain"];
    assert_eq!(listed(&s, UPDATES_Q, "updates", None).await, both);

    // A reader's own rule: their list, and only theirs.
    set_mine(&s, r#"[{ tags: ["gore"] }]"#, "bobtok").await;
    assert_eq!(
        listed(&s, UPDATES_Q, "updates", Some("bobtok")).await,
        ["Engine Plain"]
    );
    assert_eq!(listed(&s, UPDATES_Q, "updates", None).await, both);
    let shown = discovery_titles(&s, Some("bobtok")).await;
    assert!(shown.contains(&"Engine Plain".to_string()), "{shown:?}");
    assert!(!shown.contains(&"Engine Gore".to_string()), "{shown:?}");
    // A visitor's rails still have it.
    let shown = discovery_titles(&s, None).await;
    assert!(shown.contains(&"Engine Gore".to_string()), "{shown:?}");
    // Their own rule leaves the door open.
    let q = r#"{ chapters(seriesId: "902") { id } }"#;
    assert_ne!(err(&run(&s, q, Some("bobtok")).await), "No such series");

    // The site's rule: everyone's lists, and the doors.
    set_mine(&s, "[]", "bobtok").await;
    set_site(&s, r#"[{ tags: ["gore"] }]"#).await;
    for token in [None, Some("bobtok")] {
        assert_eq!(
            listed(&s, UPDATES_Q, "updates", token).await,
            ["Engine Plain"]
        );
        for q in [
            r#"{ series(id: "902") { id } }"#,
            r#"{ chapters(seriesId: "902") { id } }"#,
            // The resolvers parse the id, so these spell the same series. The gate
            // must read them the same way.
            r#"{ series(id: "0902") { id } }"#,
            r#"{ chapters(seriesId: "0902") { id } }"#,
            r#"{ series(id: "+902") { id } }"#,
            r#"{ chapters(seriesId: "+902") { id } }"#,
            r#"{ reviews(seriesId: "902") { items { id } } }"#,
            r#"{ comments(targetType: "series", targetId: "902") { items { id } } }"#,
        ] {
            assert_eq!(err(&run(&s, q, token).await), "No such series", "{q}");
        }
    }
    let q = r#"mutation { mark(seriesId: "902", marked: true) { title } }"#;
    assert_eq!(err(&run(&s, q, Some("bobtok")).await), "No such series");
    // The page image route asks the same question without a GraphQL request.
    assert!(site_blocks_engine_series(&st.pool, 902).await.unwrap());
    assert!(!site_blocks_engine_series(&st.pool, 901).await.unwrap());
    // A series with no work yet has nothing a rule can match.
    assert!(!site_blocks_engine_series(&st.pool, 999).await.unwrap());
    // The other series is not refused by the rule.
    let q = r#"{ chapters(seriesId: "901") { id } }"#;
    assert_ne!(err(&run(&s, q, None).await), "No such series");

    set_site(&s, "[]").await;
    assert!(!site_blocks_engine_series(&st.pool, 902).await.unwrap());
}

/// A series is judged from the moment it is added. Its tags are derived from its
/// source's genres at enrol, not at the next catalogue refresh (hours later), so a rule
/// that names one of them closes the series at once.
#[tokio::test]
async fn a_rule_reaches_a_series_the_moment_it_is_added() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    // The source is on record, so the enrol does not have to ask the engine about it.
    sqlx::query(
        r#"INSERT INTO source_extension (source_id, pkg_name, repo_url, is_nsfw, updated_at) VALUES ('77', 'pkg', 'repo', FALSE, '2026-01-01T00:00:00Z')"#,
    )
    .execute(&st.pool)
    .await
    .unwrap();
    set_site(&s, r#"[{ tags: ["gore"] }]"#).await;

    let manga = |id: i64, title: &str, genre: &str| crate::suwayomi::SuwayomiManga {
        id,
        title: title.into(),
        url: None,
        thumbnail_url: None,
        author: None,
        artist: None,
        description: None,
        genre: vec![genre.to_string()],
        status: "ONGOING".into(),
        in_library: false,
        in_library_at: None,
        last_fetched_at: None,
        latest_chapter_at: None,
        source_id: "77".into(),
        source: None,
        chapters: None,
    };
    let gore = add_source_series_checked(&st, &manga(950, "Fresh Gore Tale", "Gore"), None, false)
        .await
        .unwrap();
    let plain =
        add_source_series_checked(&st, &manga(951, "Fresh Plain Tale", "Action"), None, false)
            .await
            .unwrap();

    assert!(site_blocks_engine_series(&st.pool, 950).await.unwrap());
    assert!(!site_blocks_engine_series(&st.pool, 951).await.unwrap());
    for q in [
        r#"{ series(id: "950") { id } }"#,
        r#"{ chapters(seriesId: "950") { id } }"#,
    ] {
        assert_eq!(err(&run(&s, q, None).await), "No such series", "{q}");
    }
    // The rule read the tags the enrol derived, not a later refresh.
    for (work, tag) in [(&gore.work_id, "gore"), (&plain.work_id, "action")] {
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT tag_key FROM work_tag WHERE work_id = $1 AND source = 'suwayomi'",
        )
        .bind(work)
        .fetch_all(&st.pool)
        .await
        .unwrap();
        assert_eq!(keys, [tag]);
    }
}

#[tokio::test]
async fn a_source_ai_tag_is_admin_written_and_public_to_read() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed_engine_series(&st.pool, 901, "Engine Plain", &["Action"]).await;
    // `scanlatorSites` lists the sources Browse knows, so the series must be in it.
    sqlx::query(
        r#"INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, en_chapter_count, created_at) VALUES ('w_engine901', '901', 'Engine Plain', FALSE, 5, '2026-01-01T00:00:00Z')"#,
    )
    .execute(&st.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO source_extension (source_id, pkg_name, repo_url, lang, updated_at) \
         VALUES ('77', 'ext.en.example', 'https://example.org/repo', 'en', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();

    const SITES_Q: &str = "{ scanlatorSites { sourceId name lang seriesCount ai } }";
    const TAGS_Q: &str = "{ sourceAiTags { sourceId ai } }";
    const SET_Q: &str = r#"mutation { setSourceAi(sourceId: "77", ai: PARTIAL) }"#;

    // The list and the write are the console's; a reader gets neither.
    for q in [SITES_Q, SET_Q] {
        assert_eq!(err(&run(&s, q, None).await), "Not authenticated", "{q}");
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required",
            "{q}"
        );
    }

    // The engine does not answer here, so the site is named by its package.
    let sites = ok(run(&s, SITES_Q, Some("admintok")).await);
    assert_eq!(
        sites["scanlatorSites"],
        serde_json::json!([
            { "sourceId": "77", "name": "example", "lang": "en", "seriesCount": 1, "ai": null }
        ])
    );
    assert_eq!(
        ok(run(&s, TAGS_Q, None).await)["sourceAiTags"],
        serde_json::json!([])
    );

    assert_eq!(
        ok(run(&s, SET_Q, Some("admintok")).await)["setSourceAi"],
        true
    );
    // The same tag again changes nothing.
    assert_eq!(
        ok(run(&s, SET_Q, Some("admintok")).await)["setSourceAi"],
        false
    );
    // Anyone can read the tags: a reader app marks chapters with them.
    assert_eq!(
        ok(run(&s, TAGS_Q, None).await)["sourceAiTags"],
        serde_json::json!([{ "sourceId": "77", "ai": "PARTIAL" }])
    );
    let sites = ok(run(&s, SITES_Q, Some("admintok")).await);
    assert_eq!(sites["scanlatorSites"][0]["ai"], "PARTIAL");

    // A tag on a source that has no series left is still listed, so it can be removed.
    let q = r#"mutation { setSourceAi(sourceId: "88", ai: AI) }"#;
    assert_eq!(ok(run(&s, q, Some("admintok")).await)["setSourceAi"], true);
    let sites = ok(run(&s, SITES_Q, Some("admintok")).await);
    assert_eq!(
        sites["scanlatorSites"][1],
        serde_json::json!({ "sourceId": "88", "name": "88", "lang": null, "seriesCount": 0, "ai": "AI" })
    );
    let q = r#"mutation { setSourceAi(sourceId: "88", ai: null) }"#;
    assert_eq!(ok(run(&s, q, Some("admintok")).await)["setSourceAi"], true);
    assert_eq!(
        ok(run(&s, SITES_Q, Some("admintok")).await)["scanlatorSites"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // MangaDex credits its groups, so it is not a site; nor is free text a source id.
    for bad in ["mangadex", "", "not a source"] {
        let q = format!(r#"mutation {{ setSourceAi(sourceId: "{bad}", ai: AI) }}"#);
        assert_eq!(
            err(&run(&s, &q, Some("admintok")).await),
            "Choose a source.",
            "{bad}"
        );
    }
}

/// The seven operations the apps send for these two features, read from the client
/// package as they are written there, run against this server's schema. A field, an
/// argument or a type that one side renames fails here, not in a browser.
#[tokio::test]
async fn the_clients_operations_for_these_features_run_on_this_schema() {
    const OPERATIONS: &str = include_str!("../../../../packages/api/src/operations.ts");
    fn document(name: &str) -> &'static str {
        let head = format!("export const {name} = /* GraphQL */ `");
        let body = OPERATIONS
            .split(&head)
            .nth(1)
            .unwrap_or_else(|| panic!("{name} is not in operations.ts"));
        body.split('`').next().unwrap()
    }

    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed_engine_series(&st.pool, 901, "Engine Plain", &["Action"]).await;
    let rules = serde_json::json!({ "rules": [{ "tags": ["gore"], "ratings": ["EROTICA"] }] });
    let stored = serde_json::json!([{ "tags": ["gore"], "ratings": ["EROTICA"] }]);
    let none = serde_json::json!({});
    // (operation, variables, who sends it, the field it answers with)
    let calls = [
        (
            "SET_SITE_BLOCK_RULES",
            &rules,
            "admintok",
            "setSiteBlockRules",
        ),
        ("SITE_BLOCK_RULES", &none, "admintok", "siteBlockRules"),
        ("SET_MY_BLOCK_RULES", &rules, "bobtok", "setMyBlockRules"),
        ("MY_BLOCK_RULES", &none, "bobtok", "myBlockRules"),
    ];
    for (name, variables, token, field) in calls {
        let resp = s
            .execute(
                async_graphql::Request::new(document(name))
                    .variables(async_graphql::Variables::from_json(variables.clone()))
                    .data(RequestAuth(Some(token.to_string())))
                    .data(ClientIp(Some("1.1.1.1".into())))
                    .data(RequestUserCache::default())
                    .data(RequestLibraryCache::default()),
            )
            .await;
        assert_eq!(ok(resp)[field], stored, "{name}");
    }

    let send = |name: &'static str, variables: serde_json::Value| {
        let s = &s;
        async move {
            ok(s.execute(
                async_graphql::Request::new(document(name))
                    .variables(async_graphql::Variables::from_json(variables))
                    .data(RequestAuth(Some("admintok".to_string())))
                    .data(ClientIp(Some("1.1.1.1".into())))
                    .data(RequestUserCache::default())
                    .data(RequestLibraryCache::default()),
            )
            .await)
        }
    };
    let tagged = send(
        "SET_SOURCE_AI",
        serde_json::json!({ "sourceId": "77", "ai": "PARTIAL" }),
    )
    .await;
    assert_eq!(tagged["setSourceAi"], true);
    assert_eq!(
        send("SOURCE_AI_TAGS", none.clone()).await["sourceAiTags"],
        serde_json::json!([{ "sourceId": "77", "ai": "PARTIAL" }])
    );
    let sites = send("SCANLATOR_SITES", none.clone()).await;
    let site = &sites["scanlatorSites"][0];
    assert_eq!(site["sourceId"], "77");
    assert_eq!(site["ai"], "PARTIAL");
    // Not in Browse yet (no catalogue refresh ran here), so it is listed by its tag.
    assert_eq!(site["seriesCount"], 0);
    assert_eq!(site["name"], "77");
    assert!(site["lang"].is_null());
    // Removing the tag takes a null, as the client sends it.
    let cleared = send(
        "SET_SOURCE_AI",
        serde_json::json!({ "sourceId": "77", "ai": null }),
    )
    .await;
    assert_eq!(cleared["setSourceAi"], true);
}
