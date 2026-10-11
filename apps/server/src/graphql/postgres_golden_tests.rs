//! SQLite compatibility baseline for the PostgreSQL port. See tests/postgres_golden/README.md.
//! Lives with the other resolver tests because this binary crate has no library target.

use super::sources_tests::{fixture_with_users, run};
use super::*;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const USERS: [(&str, &str, i64, &str); 3] = [
    (
        "00000000-0000-4000-8000-000000000900",
        "owner",
        1,
        "golden-admin",
    ),
    (
        "00000000-0000-4000-8000-000000000901",
        "reader",
        0,
        "golden-user",
    ),
    (
        "00000000-0000-4000-8000-000000000902",
        "cjk-reader",
        0,
        "golden-nsfw-user",
    ),
];

#[derive(Deserialize)]
struct Case {
    name: String,
    role: String,
    query: String,
    allow_errors: bool,
}

#[derive(Deserialize)]
struct SearchQuery {
    query: String,
    expected: Vec<String>,
    note: String,
}

fn error_sources(response: &async_graphql::Response) -> Vec<String> {
    response
        .errors
        .iter()
        .filter_map(|error| error.source.as_ref())
        .filter_map(|source| {
            source
                .downcast_ref::<sqlx::Error>()
                .map(|error| error.to_string())
                .or_else(|| {
                    source
                        .downcast_ref::<anyhow::Error>()
                        .map(|error| format!("{error:#}"))
                })
        })
        .collect()
}

fn token(role: &str) -> Option<&'static str> {
    match role {
        "anonymous" => None,
        "user" => Some(USERS[1].3),
        "admin" => Some(USERS[0].3),
        "nsfw_user" => Some(USERS[2].3),
        _ => panic!("unknown fixture role: {role}"),
    }
}

async fn seed(pool: &Database, root: &Path) {
    // Ordinary SQL only. The server owns engine-specific migrations and projections.
    // No generated identifiers, current dates, FTS internals or SQLite file fixture.
    for statement in std::fs::read_to_string(root.join("seed.sql"))
        .unwrap()
        .split(';')
    {
        if !statement.trim().is_empty() {
            sqlx::query(statement)
                .execute(pool)
                .await
                .unwrap_or_else(|e| panic!("golden seed failed: {e}\n{statement}"));
        }
    }
    // Reuse the production alias and chapter-label rules without their stamping writers.
    let aliases: Vec<(String, String)> =
        sqlx::query_as("SELECT id, raw_title FROM work_alias ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    for (id, title) in aliases {
        sqlx::query("UPDATE work_alias SET normalized_title = $1 WHERE id = $2")
            .bind(catalog::normalize::normalize_title(&title))
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
    let chapters: Vec<(String, String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, external_id, number, title FROM chapter ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    for (id, external_id, number, title) in chapters {
        let (key, label) = catalog::spine_key_and_label(&catalog::ChapterInput {
            external_id,
            number,
            title,
            ..Default::default()
        });
        sqlx::query("UPDATE chapter SET chapter_key = $1, label = $2 WHERE id = $3")
            .bind(key)
            .bind(label)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
    // Use the server's ledger writer with an explicit clock. This keeps the seed
    // neutral when integer-millisecond columns become native timestamptz: their
    // encoding belongs to the production writer, not to fixture INSERT literals.
    let works: Vec<String> = sqlx::query_scalar("SELECT id FROM work ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap();
    for work_id in works {
        catalog::ledger::seed_work(pool, &work_id, 1_578_618_123_456)
            .await
            .unwrap();
    }
    catalog::refresh_feed_updates(pool).await.unwrap();
    catalog::refresh_feed_series_updates(pool).await.unwrap();
    catalog::refresh_browse_catalogue(pool).await.unwrap();
    catalog::refresh_work_fts(pool).await.unwrap();
    pool.flush_deferred().await;
}

// Explicit recursive sorting also works if serde_json's preserve_order feature is
// enabled later. Array order and every scalar byte (including dates/IDs) stay intact.
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map.into_iter().map(|(k, v)| (k, canonical(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// Report the first differing JSON path plus a contextual byte/line diff. Compare
/// serialized bytes, not parsed values, so date precision/offsets cannot be normalized away.
fn first_difference(expected: &Value, actual: &Value, path: &str) -> Option<String> {
    if expected == actual {
        return None;
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            let keys: std::collections::BTreeSet<_> = e.keys().chain(a.keys()).collect();
            for key in keys {
                if let Some(diff) = first_difference(&e[key], &a[key], &format!("{path}.{key}")) {
                    return Some(diff);
                }
            }
        }
        (Value::Array(e), Value::Array(a)) if e.len() == a.len() => {
            for (i, (e, a)) in e.iter().zip(a).enumerate() {
                if let Some(diff) = first_difference(e, a, &format!("{path}[{i}]")) {
                    return Some(diff);
                }
            }
        }
        _ => {}
    }
    Some(format!("{path}\n- {expected}\n+ {actual}"))
}

// Entries are added only after an explicit owner reply accepting these exact
// outputs. Hashes bind acceptance to every byte, including IDs and array order.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedDifferences {
    schema_version: u32,
    entries: Vec<AcceptedDifference>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedDifference {
    file: String,
    expected_sha256: String,
    actual_sha256: String,
    reason: String,
    acceptance: OwnerAcceptance,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerAcceptance {
    accepted_by: String,
    accepted_on: String,
    evidence: String,
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn accepted_difference(entry: &AcceptedDifference, difference: &Value) -> bool {
    entry.file == difference["file"].as_str().unwrap()
        && entry.expected_sha256 == difference["expected_sha256"].as_str().unwrap()
        && entry.actual_sha256 == difference["actual_sha256"].as_str().unwrap()
}
fn unaccepted_differences<'a>(
    manifest: &AcceptedDifferences,
    differences: &'a [Value],
) -> Vec<&'a Value> {
    assert_eq!(
        manifest.schema_version, 1,
        "unsupported golden acceptance manifest"
    );
    let mut files = std::collections::BTreeSet::new();
    for entry in &manifest.entries {
        assert!(
            files.insert(&entry.file),
            "duplicate golden acceptance: {}",
            entry.file
        );
        assert!(
            !entry.reason.trim().is_empty(),
            "missing explanation: {}",
            entry.file
        );
        assert_eq!(
            entry.acceptance.accepted_by, "owner",
            "only explicit owner acceptance permits a golden difference"
        );
        assert!(
            chrono::NaiveDate::parse_from_str(&entry.acceptance.accepted_on, "%Y-%m-%d").is_ok(),
            "missing owner acceptance date"
        );
        assert!(
            !entry.acceptance.evidence.trim().is_empty(),
            "missing exact owner acceptance evidence"
        );
        for hash in [&entry.expected_sha256, &entry.actual_sha256] {
            assert!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid golden hash"
            );
        }
        assert!(differences.iter().any(|diff|accepted_difference(entry,diff)),
            "stale or altered accepted output: {}; acceptance must be renewed for its exact old/new hashes",entry.file);
    }
    differences
        .iter()
        .filter(|diff| {
            !manifest
                .entries
                .iter()
                .any(|entry| accepted_difference(entry, diff))
        })
        .collect()
}

fn compare(path: &Path, output: Value, update: bool) -> Option<Value> {
    let output = canonical(output);
    let actual = serde_json::to_string_pretty(&output).unwrap() + "\n";
    assert!(!update, "snapshot recording is forbidden during Phase3");
    let expected = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. See tests/postgres_golden/README.md for recording.",
            path.display()
        )
    });
    if actual != expected {
        let parsed: Value = serde_json::from_str(&expected).unwrap();
        let diff = first_difference(&parsed, &output, "$")
            .unwrap_or_else(|| "JSON formatting differs".into());
        let e: Vec<_> = expected.lines().collect();
        let a: Vec<_> = actual.lines().collect();
        let line = e
            .iter()
            .zip(&a)
            .position(|(e, a)| e != a)
            .unwrap_or(e.len().min(a.len()));
        let before = line.saturating_sub(2);
        let after = (line + 4).min(e.len().max(a.len()));
        let context = (before..after)
            .map(|i| {
                format!(
                    "{}\n- {}\n+ {}",
                    i + 1,
                    e.get(i).unwrap_or(&"<EOF>"),
                    a.get(i).unwrap_or(&"<EOF>")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(
            json!({"file":path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap().to_string_lossy(),
            "expected_sha256":sha256(expected.as_bytes()),"actual_sha256":sha256(actual.as_bytes()),
            "first_difference":diff,"line_context":context,"expected":parsed,"actual":output}),
        );
    }
    None
}

#[test]
fn postgres_golden_acceptance_is_bound_to_exact_output_bytes() {
    let empty = AcceptedDifferences {
        schema_version: 1,
        entries: Vec::new(),
    };
    let difference = json!({"file":"tests/postgres_golden/example.json","expected_sha256":sha256(b"old"),"actual_sha256":sha256(b"new")});
    assert_eq!(
        unaccepted_differences(&empty, std::slice::from_ref(&difference)).len(),
        1
    );
    // Synthetic unit input exercises the guard; the on-disk owner manifest stays empty.
    let manifest:AcceptedDifferences=serde_json::from_value(json!({"schema_version":1,"entries":[{
        "file":difference["file"],"expected_sha256":difference["expected_sha256"],"actual_sha256":difference["actual_sha256"],
        "reason":"Synthetic unit input","acceptance":{"accepted_by":"owner","accepted_on":"2026-10-10","evidence":"Synthetic test only"}}]})).unwrap();
    assert!(unaccepted_differences(&manifest, std::slice::from_ref(&difference)).is_empty());
    let altered = json!({"file":difference["file"],"expected_sha256":difference["expected_sha256"],"actual_sha256":sha256(b"changed ID/order")});
    assert!(!accepted_difference(&manifest.entries[0], &altered));
    let unexpected = json!({"file":"tests/postgres_golden/unlisted.json","expected_sha256":sha256(b"old"),"actual_sha256":sha256(b"new")});
    assert_eq!(
        unaccepted_differences(&manifest, &[difference, unexpected]).len(),
        1
    );
}

#[tokio::test]
async fn postgres_golden_sqlite_baseline() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/postgres_golden");
    let update = std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1");
    assert!(!update, "Phase3 forbids recording snapshots from PostgreSQL output; use the date-only conversion script");
    // Invalid URL scheme: even an accidental cache miss cannot open a socket.
    // Successful cases below assert no resolver errors; read the README's exclusions.
    let (schema, st) = fixture_with_users("offline://golden", &USERS).await;
    st.mangadex.set_enabled(false);
    seed(&st.pool, &root).await;
    let cases: Vec<Case> =
        serde_json::from_str(&std::fs::read_to_string(root.join("graphql_cases.json")).unwrap())
            .unwrap();
    let mut differences = Vec::new();
    let mut blocking_errors = Vec::new();
    for case in &cases {
        let response = run(&schema, &case.query, token(&case.role)).await;
        let unexpected_state = response.errors.is_empty() == case.allow_errors;
        let unexpected_error = response.errors.iter().any(|error| {
            !matches!(
                error.message.as_str(),
                "No such work" | "Not authenticated" | "Admin access required"
            )
        });
        if unexpected_state || unexpected_error {
            blocking_errors.push(json!({"case":case.name,"errors":response.errors,"sources":error_sources(&response)}));
        }
        if let Some(diff) = compare(
            &root.join("graphql").join(format!("{}.json", case.name)),
            serde_json::to_value(response).unwrap(),
            update,
        ) {
            differences.push(diff);
        }
    }
    let queries: Vec<SearchQuery> =
        serde_json::from_str(&std::fs::read_to_string(root.join("search_queries.json")).unwrap())
            .unwrap();
    let mut results = Vec::new();
    for query in &queries {
        // The public resolver uses the PostgreSQL title/alias search. Admin
        // includeNsfw removes the reader's gate so recall includes every fixture work.
        let gql = format!(
            "{{ search(query: {}, includeNsfw: true) {{ total items {{ workId }} }} }}",
            serde_json::to_string(&query.query).unwrap()
        );
        let response = run(&schema, &gql, token("admin")).await;
        if !response.errors.is_empty() {
            blocking_errors.push(json!({"search_query":query.query,"errors":response.errors,"sources":error_sources(&response)}));
        }
        let data = response.data.into_json().unwrap();
        let observed: Vec<_> = data["search"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                item["workId"]
                    .as_str()
                    .expect("search result must resolve to a work")
                    .to_owned()
            })
            .collect();
        results.push(json!({"query": query.query, "observed": observed}));
        // Validate human annotations, without requiring SQLite to meet the future recall gate.
        assert!(!query.note.is_empty());
        for id in &query.expected {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE id = $1")
                .bind(id)
                .fetch_one(&st.pool)
                .await
                .unwrap();
            assert_eq!(
                count, 1,
                "unknown expected work for {:?}: {id}",
                query.query
            );
        }
    }
    if let Some(diff) = compare(&root.join("search_observed.json"), json!(results), update) {
        differences.push(diff);
    }
    if let Ok(path) = std::env::var("KOMIKA_PHASE3_GOLDEN_DIFF_REPORT") {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&differences).unwrap() + "\n",
        )
        .unwrap();
        std::fs::write(
            format!("{path}.blocking-errors.json"),
            serde_json::to_string_pretty(&blocking_errors).unwrap() + "\n",
        )
        .unwrap();
    }
    assert!(
        blocking_errors.is_empty(),
        "{} blocking API error/state defects; collected all cases: {:?}",
        blocking_errors.len(),
        blocking_errors
    );
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/postgres_golden/accepted_differences.json");
    let manifest: AcceptedDifferences =
        serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
    let unaccepted = unaccepted_differences(&manifest, &differences);
    for difference in &differences {
        eprintln!(
            "golden mismatch {}: {}",
            difference["file"], difference["first_difference"]
        );
    }
    assert!(unaccepted.is_empty(), "{} unaccepted golden output/order differences require explanation and explicit owner acceptance; snapshots were preserved",unaccepted.len());
    eprintln!(
        "golden baseline: {} GraphQL cases, {} search queries",
        cases.len(),
        queries.len()
    );
    st.pool.close().await;
}

/// The owner accepted these numeric targets on 2026-10-10 before implementation:
/// 100% required fixture recall, duplicate-free results, exact/prefix first, and
/// warmed public GraphQL p95 <=100 ms on this deterministic local fixture.
#[tokio::test]
async fn postgres_search_recall_and_latency() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/postgres_golden");
    let (schema, st) = fixture_with_users("offline://golden", &USERS).await;
    st.mangadex.set_enabled(false);
    seed(&st.pool, &root).await;
    let queries: Vec<SearchQuery> =
        serde_json::from_str(&std::fs::read_to_string(root.join("search_queries.json")).unwrap())
            .unwrap();
    let mut samples = Vec::new();
    let mut required = 0;
    for round in 0..25 {
        for query in &queries {
            let gql = format!(
                "{{ search(query: {}, includeNsfw: true) {{ total items {{ workId }} }} }}",
                serde_json::to_string(&query.query).unwrap()
            );
            let start = std::time::Instant::now();
            let response = run(&schema, &gql, token("admin")).await;
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            assert!(
                response.errors.is_empty(),
                "{:?}: {:?}",
                query.query,
                response.errors
            );
            let data = response.data.into_json().unwrap();
            let observed: Vec<String> = data["search"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["workId"].as_str().unwrap().to_owned())
                .collect();
            let unique: std::collections::BTreeSet<_> = observed.iter().collect();
            assert_eq!(
                unique.len(),
                observed.len(),
                "duplicate alias result: {:?}",
                query.query
            );
            assert_eq!(
                data["search"]["total"].as_i64().unwrap(),
                observed.len() as i64,
                "total/page mismatch in one-page fixture: {:?}",
                query.query
            );
            for expected in &query.expected {
                assert!(
                    observed.contains(expected),
                    "recall lost: {:?}; missing {expected}; got {observed:?}",
                    query.query
                );
            }
            if query.expected.is_empty() {
                assert!(
                    observed.is_empty(),
                    "negative search {:?}: {observed:?}",
                    query.query
                );
            }
            if query.query == "Solo Leveling" || query.query == "Solo Lev" {
                assert_eq!(
                    observed.first().map(String::as_str),
                    Some("w_00000000000040008000000000000001"),
                    "exact/prefix title must precede the side story"
                );
            }
            if round == 0 {
                required += query.expected.len();
            }
            if round >= 5 {
                samples.push(elapsed);
            }
        }
    }
    samples.sort_by(f64::total_cmp);
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    eprintln!("Phase3 search: 15 works; 42 independent queries; {required}/{required} required results (100% recall); {} warmed public GraphQL samples; p95={p95:.3}ms; target<=100ms",samples.len());
    assert!(
        p95 <= 100.0,
        "search p95 {p95:.3}ms exceeded 100ms local fixture gate"
    );

    // Independent AND recall across separate title/alias rows, including aliases
    // with no title word in common. The expected work ID is specified directly.
    sqlx::query("INSERT INTO work(id,primary_title,created_at,updated_at) VALUES('phase3-cross-row','Azure Beacon','2020-01-01T00:00:00Z','2020-01-01T00:00:00Z')")
        .execute(&st.pool).await.unwrap();
    sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES('phase3-cross-source','phase3-cross-row','mangadex','phase3-cross-key','2020-01-01T00:00:00Z')")
        .execute(&st.pool).await.unwrap();
    for (id, title) in [
        ("phase3-cross-alias-a", "Crimson Lantern"),
        ("phase3-cross-alias-b", "Harbor"),
    ] {
        sqlx::query("INSERT INTO work_alias(id,work_id,raw_title,normalized_title) VALUES($1,'phase3-cross-row',$2,$2)")
            .bind(id).bind(title).execute(&st.pool).await.unwrap();
    }
    for query in [
        "azure crimson",
        "beacon lantern",
        "crimson harbor",
        "azure crimsom",
    ] {
        let (_, ids) = catalog::search_works_fts(&st.pool, query, true, &Default::default(), 1, 30)
            .await
            .unwrap();
        assert!(
            ids.iter().any(|id| id == "phase3-cross-row"),
            "cross-row AND lost {query:?}: {ids:?}"
        );
    }
    // A source-less exact-word shell and a hidden exact-word title must not
    // suppress a visible typo hit before the caller's eligibility gate.
    sqlx::query("INSERT INTO work(id,primary_title,is_nsfw,created_at,updated_at) VALUES('phase3-ghost','Azure Crimsom',FALSE,'2020-01-01T00:00:00Z','2020-01-01T00:00:00Z'),('phase3-hidden','Azure Crimsom',TRUE,'2020-01-01T00:00:00Z','2020-01-01T00:00:00Z')")
        .execute(&st.pool).await.unwrap();
    sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES('phase3-hidden-source','phase3-hidden','mangadex','phase3-hidden-key','2020-01-01T00:00:00Z')")
        .execute(&st.pool).await.unwrap();
    let (_, ids) =
        catalog::search_works_fts(&st.pool, "azure crimsom", false, &Default::default(), 1, 30)
            .await
            .unwrap();
    assert!(
        ids.iter().any(|id| id == "phase3-cross-row"),
        "hidden/shell word hit suppressed visible typo: {ids:?}"
    );
    assert!(
        !ids.iter()
            .any(|id| id == "phase3-hidden" || id == "phase3-ghost"),
        "ineligible search work leaked: {ids:?}"
    );
    sqlx::query("UPDATE work SET is_nsfw=FALSE WHERE id='phase3-hidden'")
        .execute(&st.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO work_tag(work_id,tag,tag_key,ord) VALUES('phase3-hidden','Phase3 hidden','phase3-hidden',0)").execute(&st.pool).await.unwrap();
    let blocks = crate::block::BlockSet::new(vec![crate::block::BlockRule::new(
        ["phase3-hidden"],
        Vec::<&str>::new(),
    )
    .unwrap()]);
    let (_, ids) = catalog::search_works_fts(&st.pool, "azure crimsom", true, &blocks, 1, 30)
        .await
        .unwrap();
    assert!(
        ids.iter().any(|id| id == "phase3-cross-row"),
        "blocked word hit suppressed visible typo: {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id == "phase3-hidden"),
        "blocked word hit leaked"
    );
    catalog::refresh_browse_catalogue(&st.pool).await.unwrap();
    let browse_query = crate::browse::BrowseQuery {
        show_nsfw: true,
        exclude_tags: vec!["phase3-hidden".to_owned()],
        sort: crate::browse::Sort::desc(crate::browse::SortKey::Relevance),
        ..Default::default()
    };
    let (_, rows) = crate::browse::search_text(&st.pool, "azure crimsom", &browse_query)
        .await
        .unwrap();
    assert!(
        rows.iter().any(|row| row.work_id == "phase3-cross-row"),
        "Browse filter excluded word hit suppressed visible typo"
    );
    assert!(
        !rows.iter().any(|row| row.work_id == "phase3-hidden"),
        "Browse excluded-tag word hit leaked"
    );
    for query in ["solo leveling nonexistent", "azure beacon nonexistent"] {
        let (_, ids) = catalog::search_works_fts(&st.pool, query, true, &Default::default(), 1, 30)
            .await
            .unwrap();
        assert!(
            ids.is_empty(),
            "fuzzy candidates bypassed per-term AND for {query:?}: {ids:?}"
        );
    }
    let (_, ids) = catalog::search_works_fts(
        &st.pool,
        "solo leveling side",
        true,
        &Default::default(),
        1,
        30,
    )
    .await
    .unwrap();
    assert!(
        !ids.iter()
            .any(|id| id == "w_00000000000040008000000000000001"),
        "missing side term incorrectly returned the base work: {ids:?}"
    );
    let (_, ids) =
        catalog::search_works_fts(&st.pool, "solo only", true, &Default::default(), 1, 30)
            .await
            .unwrap();
    assert!(
        ids.iter()
            .any(|id| id == "w_00000000000040008000000000000001"),
        "title/alias AND lost: {ids:?}"
    );
    st.pool.close().await;
}

#[tokio::test]
async fn postgres_search_typo_recheck_is_bounded_and_transposes() {
    let pool = Database::connect("sqlite::memory:").await.unwrap();
    // Independently stated word-level edit examples, including native Unicode.
    for (query, word, expected) in [
        ("sloo", "solo", true),
        ("levelign", "leveling", true),
        ("leveing", "leveling", true),
        ("levlng", "leveling", true),
        ("stnoe", "stone", true),
        ("famliy", "family", true),
        ("crimsom", "crimson", true),
        ("dusk", "dawn", false),
        ("side", "solo", false),
        ("nonexistent", "leveling", false),
        ("進巨撃人", "進撃巨人", true),
        ("", "", false),
    ] {
        let close: bool = crate::db_types::query_scalar("SELECT public.search_typo_close($1,$2)")
            .bind(query)
            .bind(word)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(close, expected, "word recheck {query:?} versus {word:?}");
    }
    let long = "x".repeat(257);
    let close: bool = crate::db_types::query_scalar("SELECT public.search_typo_close($1,$2)")
        .bind(&long)
        .bind(&long)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !close,
        "fuzzy word rechecks must retain the documented bound"
    );
    let null: Option<bool> =
        crate::db_types::query_scalar("SELECT public.search_typo_close(NULL,$1)")
            .bind("solo")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        null, None,
        "native null propagates through the strict helper"
    );
    pool.close().await;
}
