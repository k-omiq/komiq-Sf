//! The readers' scanlator list (`scanlatorRatings`, `ratedScanlator`), `rateScanlator`
//! and a scanlator's comment thread, end to end against a real database. The pure parts
//! (the joint-credit fold, the rank, the orders) are unit tested in `scanlators.rs`.

use super::sources_tests::{err, fixture_with_users, mock_engine, ok, run};
use super::*;
use serde_json::json;

const ADMIN: &str = "964771b0-a2fb-4849-857a-6198947115ec";
const BOB: &str = "ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c";

/// An admin, the reader `bob`, and four more readers (`r1tok` … `r4tok`): six accounts,
/// one more than a rank takes.
const USERS: [(&str, &str, i64, &str); 6] = [
    (ADMIN, "admin", 1, "admintok"),
    (BOB, "bob", 0, "bobtok"),
    ("00000000-0000-4000-8000-0000000000a1", "r1", 0, "r1tok"),
    ("00000000-0000-4000-8000-0000000000a2", "r2", 0, "r2tok"),
    ("00000000-0000-4000-8000-0000000000a3", "r3", 0, "r3tok"),
    ("00000000-0000-4000-8000-0000000000a4", "r4", 0, "r4tok"),
];

const FIELDS: &str = "id kind name lang logoUrl seriesCount chapterCount ai rating ratingCount \
                      rank distribution myScore commentCount";

/// A catalogue with two sites and three credits:
///
/// * source 77 carries the safe work `w1`; source 88 carries only the adult work `w2`;
/// * MangaDex credits "Robo Scans", the joint "Human Scans & Robo Scans" and "Robo Scans
///   & Strawberry" on `w1`; source 77 credits "Human Scans".
///
/// Source 77's extension comes from a repo index, so it has a logo; 88's repo is no index
/// this server can read icons from, and the mock engine lists no sources, so it has none.
///
/// "Strawberry" is no credit of its own, so the last one stays a scanlator; the joint
/// credit of two known groups is folded into them.
async fn seeded() -> (ApiSchema, std::sync::Arc<AppState>) {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture_with_users(&engine, &USERS).await;
    for sql in [
        "INSERT INTO work (id, primary_title, is_nsfw, created_at, updated_at) VALUES \
           ('w1', 'One', FALSE, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'), \
           ('w2', 'Two', TRUE, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        "INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, en_chapter_count, created_at) VALUES \
           ('w1', 'w1', 'One', FALSE, 4, '2026-01-01T00:00:00Z'), \
           ('w2', 'w2', 'Two', TRUE, 1, '2026-01-01T00:00:00Z')",
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES \
           ('md1', 'w1', 'mangadex', 'mangadex', '11ac9418-0c9e-4237-9e42-3ace93d7c468', '2026-01-01T00:00:00Z'), \
           ('sy1', 'w1', 'suwayomi', '77', '1', '2026-01-01T00:00:00Z'), \
           ('sy2', 'w2', 'suwayomi', '88', '2', '2026-01-01T00:00:00Z')",
        "INSERT INTO source_extension (source_id, pkg_name, repo_url, lang, updated_at) VALUES \
           ('77', 'ext.en.example', 'https://example.org/repo/index.min.json', 'en', '2026-01-01T00:00:00Z'), \
           ('88', 'ext.all.adult', 'https://example.org/repo', 'all', '2026-01-01T00:00:00Z')",
        "INSERT INTO chapter (id, source_series_id, external_id, scanlator, created_at) VALUES \
           ('c1', 'md1', 'e1', 'Robo Scans', '2026-01-01T00:00:00Z'), \
           ('c2', 'md1', 'e2', 'Human Scans & Robo Scans', '2026-01-01T00:00:00Z'), \
           ('c3', 'sy1', 'e3', 'Human Scans', '2026-01-01T00:00:00Z'), \
           ('c4', 'md1', 'e4', 'Robo Scans & Strawberry', '2026-01-01T00:00:00Z'), \
           ('c5', 'sy2', 'e5', NULL, '2026-01-01T00:00:00Z')",
    ] {
        sqlx::query(sql).execute(&st.pool).await.unwrap();
    }
    (s, st)
}

async fn list(s: &ApiSchema, args: &str, token: Option<&str>) -> serde_json::Value {
    let q =
        format!("{{ scanlatorRatings{args} {{ total page hasNextPage items {{ {FIELDS} }} }} }}");
    ok(run(s, &q, token).await)["scanlatorRatings"].clone()
}

fn ids(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

async fn one(s: &ApiSchema, id: &str, token: Option<&str>) -> serde_json::Value {
    let q = format!(r#"{{ ratedScanlator(id: "{id}") {{ {FIELDS} }} }}"#);
    ok(run(s, &q, token).await)["ratedScanlator"].clone()
}

fn rate_q(id: &str, score: Option<i32>) -> String {
    let score = score.map_or("null".to_string(), |n| n.to_string());
    format!(r#"mutation {{ rateScanlator(id: "{id}", score: {score}) {{ {FIELDS} }} }}"#)
}

async fn rate(s: &ApiSchema, id: &str, score: Option<i32>, token: &str) -> serde_json::Value {
    ok(run(s, &rate_q(id, score), Some(token)).await)["rateScanlator"].clone()
}

#[tokio::test]
async fn the_list_shows_sites_and_groups_with_the_shape_readers_use() {
    let (s, st) = seeded().await;
    for q in [
        r#"mutation { setSourceAi(sourceId: "77", ai: PARTIAL) }"#,
        r#"mutation { setScanlatorAi(name: "robo scans", ai: AI) }"#,
    ] {
        ok(run(&s, q, Some("admintok")).await);
    }

    // Signed out, nothing rated: biggest first, then by name. The joint credit of two
    // known groups is not a row; MangaDex is not a site; the adult-only source is hidden.
    let page = list(&s, "", None).await;
    assert_eq!(
        (&page["total"], &page["page"], &page["hasNextPage"]),
        (&json!(4), &json!(1), &json!(false))
    );
    assert_eq!(
        ids(&page),
        [
            "group:human scans",
            "group:robo scans",
            "group:robo scans & strawberry",
            "site:77",
        ]
    );
    let none = json!([0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    // A group: its own chapter and series plus the joint release's.
    assert_eq!(
        page["items"][1],
        json!({
            "id": "group:robo scans", "kind": "GROUP", "name": "Robo Scans", "lang": null,
            "logoUrl": null, "seriesCount": 2, "chapterCount": 2, "ai": "AI", "rating": null,
            "ratingCount": 0, "rank": null, "distribution": none, "myScore": null,
            "commentCount": 0,
        })
    );
    assert_eq!(page["items"][0]["chapterCount"], 2);
    assert_eq!(page["items"][0]["ai"], json!(null));
    // A name that only looks joint is one scanlator, and takes its member's tag.
    assert_eq!(page["items"][2]["chapterCount"], 1);
    assert_eq!(page["items"][2]["ai"], "AI");
    // A site: named by its package here (the mock engine lists no sources), its
    // extension's icon from this server's origin, no chapter count, and the tag on the
    // source.
    assert_eq!(
        page["items"][3],
        json!({
            "id": "site:77", "kind": "SITE", "name": "example", "lang": "en",
            "logoUrl": st.suwayomi.abs(Some("/ext-icons/ext.en.example.png")),
            "seriesCount": 1, "chapterCount": null, "ai": "PARTIAL", "rating": null,
            "ratingCount": 0, "rank": null, "distribution": none, "myScore": null,
            "commentCount": 0,
        })
    );

    // The admin list is unchanged: it still shows the joint credit as written.
    let admin = ok(run(&s, "{ scanlators { total } }", Some("admintok")).await);
    assert_eq!(admin["scanlators"]["total"], 4);

    // Sites follow `browseSources` for the viewer: bob opts in and sees the adult one.
    assert_eq!(list(&s, "(kind: SITE)", Some("bobtok")).await["total"], 1);
    assert_eq!(one(&s, "site:88", Some("bobtok")).await, json!(null));
    sqlx::query("UPDATE users SET show_nsfw = TRUE WHERE id = $1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(&st.pool)
        .await
        .unwrap();
    let sites = list(&s, "(kind: SITE, sort: NAME)", Some("bobtok")).await;
    assert_eq!(ids(&sites), ["site:88", "site:77"]);
    assert_eq!(sites["items"][0]["name"], "adult");
    // No icon this server can serve: the reader draws the site's initial.
    assert_eq!(sites["items"][0]["logoUrl"], json!(null));
    assert_eq!(one(&s, "site:88", Some("bobtok")).await["lang"], "all");
    assert_eq!(one(&s, "site:88", None).await, json!(null));

    // Filters, orders and paging.
    assert_eq!(
        ids(&list(&s, r#"(search: " ROBO ", sort: NAME)"#, None).await),
        ["group:robo scans", "group:robo scans & strawberry"]
    );
    assert_eq!(
        ids(&list(&s, "(kind: GROUP, sort: CHAPTERS)", None).await),
        [
            "group:human scans",
            "group:robo scans",
            "group:robo scans & strawberry"
        ]
    );
    let past = list(&s, "(page: 2)", None).await;
    assert_eq!(
        (&past["total"], &past["page"], ids(&past).len()),
        (&json!(4), &json!(2), 0)
    );
    assert_eq!(list(&s, "(page: null)", None).await["page"], 1);

    // One scanlator by id: any case for a group; null for what the list does not show.
    assert_eq!(
        one(&s, "group:Robo Scans", None).await["id"],
        "group:robo scans"
    );
    for id in [
        "group:human scans & robo scans",
        "group:nobody",
        "site:mangadex",
        "site:999",
        "robo scans",
    ] {
        assert_eq!(one(&s, id, None).await, json!(null), "{id}");
    }
}

#[tokio::test]
async fn a_reader_rates_changes_and_removes_a_score() {
    let (s, st) = seeded().await;
    const ROBO: &str = "group:robo scans";

    // Signed out: no score of your own, and no rating.
    assert_eq!(
        err(&run(&s, &rate_q(ROBO, Some(8)), None).await),
        "Not authenticated"
    );
    for score in [0, 11, -1] {
        assert_eq!(
            err(&run(&s, &rate_q(ROBO, Some(score)), Some("bobtok")).await),
            "score must be between 1 and 10"
        );
    }
    // Only a scanlator the list shows the viewer can be rated: not an unknown name, not
    // MangaDex, not a folded joint credit, not a source hidden from this viewer.
    for id in [
        "group:nobody",
        "group:human scans & robo scans",
        "site:mangadex",
        "site:999",
        "site:88",
        "nonsense",
        "",
    ] {
        assert_eq!(
            err(&run(&s, &rate_q(id, Some(8)), Some("bobtok")).await),
            "No such scanlator",
            "{id:?}"
        );
    }
    let stored = || async {
        crate::db_types::query_tuple::<(String, String, i64, String)>(
            "SELECT scanlator_id, user_id, score, updated_at FROM scanlator_rating \
             ORDER BY scanlator_id, score",
        )
        .fetch_all(&st.pool)
        .await
        .unwrap()
    };
    assert!(stored().await.is_empty());

    // Rate: the fresh row comes back. A group's id is read in any case.
    let row = rate(&s, "group:Robo Scans", Some(8), "bobtok").await;
    assert_eq!(
        (
            &row["id"],
            &row["rating"],
            &row["ratingCount"],
            &row["myScore"],
            &row["rank"]
        ),
        (
            &json!(ROBO),
            &json!(8.0),
            &json!(1),
            &json!(8),
            &json!(null)
        )
    );
    assert_eq!(row["distribution"], json!([0, 0, 0, 0, 0, 0, 0, 1, 0, 0]));
    let first = stored().await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        (first[0].0.as_str(), first[0].1.as_str(), first[0].2),
        (ROBO, BOB, 8)
    );

    // The same score again writes nothing.
    rate(&s, ROBO, Some(8), "bobtok").await;
    assert_eq!(stored().await, first);

    // Change it: still one row for bob.
    let row = rate(&s, ROBO, Some(6), "bobtok").await;
    assert_eq!((&row["rating"], &row["myScore"]), (&json!(6.0), &json!(6)));
    assert_eq!(row["distribution"], json!([0, 0, 0, 0, 0, 1, 0, 0, 0, 0]));
    assert_eq!(stored().await.len(), 1);

    // A second reader: a plain average, and each viewer reads their own score.
    let row = rate(&s, ROBO, Some(9), "admintok").await;
    assert_eq!(
        (&row["rating"], &row["ratingCount"], &row["myScore"]),
        (&json!(7.5), &json!(2), &json!(9))
    );
    assert_eq!(one(&s, ROBO, Some("bobtok")).await["myScore"], 6);
    let anonymous = one(&s, ROBO, None).await;
    assert_eq!(
        (
            &anonymous["rating"],
            &anonymous["ratingCount"],
            &anonymous["myScore"]
        ),
        (&json!(7.5), &json!(2), &json!(null))
    );
    // A site is rated the same way.
    assert_eq!(rate(&s, "site:77", Some(3), "bobtok").await["myScore"], 3);

    // `mine`: what the viewer rated; nothing when signed out.
    assert_eq!(
        ids(&list(&s, "(mine: true, sort: NAME)", Some("bobtok")).await),
        ["site:77", ROBO]
    );
    assert_eq!(
        ids(&list(&s, "(mine: true)", Some("admintok")).await),
        [ROBO]
    );
    let signed_out = list(&s, "(mine: true)", None).await;
    assert_eq!(
        (&signed_out["total"], ids(&signed_out).len()),
        (&json!(0), 0)
    );
    assert_eq!(list(&s, "(mine: false)", None).await["total"], 4);
    // VOTES: most scores first.
    assert_eq!(
        ids(&list(&s, "(sort: VOTES)", None).await)[..2],
        [ROBO, "site:77"]
    );

    // Remove: the score is gone, the other reader's stays. Removing twice is fine.
    for _ in 0..2 {
        let row = rate(&s, ROBO, None, "bobtok").await;
        assert_eq!(
            (&row["rating"], &row["ratingCount"], &row["myScore"]),
            (&json!(9.0), &json!(1), &json!(null))
        );
    }
    assert_eq!(stored().await.len(), 2);

    // A banned reader's score still counts, as their series rating does.
    sqlx::query("UPDATE users SET is_banned = TRUE WHERE id = $1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(&st.pool)
        .await
        .unwrap();
    assert_eq!(one(&s, "site:77", None).await["ratingCount"], 1);
    // Deleting the account deletes its scores.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(&st.pool)
        .await
        .unwrap();
    assert_eq!(one(&s, "site:77", None).await["ratingCount"], 0);
    assert_eq!(stored().await.len(), 1);
}

#[tokio::test]
async fn a_rank_takes_five_scores_and_is_global() {
    let (s, _st) = seeded().await;
    const ROBO: &str = "group:robo scans";
    const READERS: [&str; 5] = ["bobtok", "r1tok", "r2tok", "r3tok", "r4tok"];

    // Four scores: rated, not ranked.
    for token in &READERS[..4] {
        rate(&s, ROBO, Some(9), token).await;
    }
    let row = one(&s, ROBO, None).await;
    assert_eq!(
        (&row["ratingCount"], &row["rating"], &row["rank"]),
        (&json!(4), &json!(9.0), &json!(null))
    );
    // The fifth gives it a rank.
    let row = rate(&s, ROBO, Some(9), READERS[4]).await;
    assert_eq!((&row["ratingCount"], &row["rank"]), (&json!(5), &json!(1)));

    // A site with five lower scores ranks below it; two scores rank nowhere.
    for token in READERS {
        rate(&s, "site:77", Some(4), token).await;
    }
    rate(&s, "group:human scans", Some(10), "bobtok").await;
    rate(&s, "group:human scans", Some(10), "admintok").await;

    // TOP (the default): the ranked by rank, then the rest by number of scores.
    let page = list(&s, "", None).await;
    assert_eq!(
        ids(&page),
        [
            ROBO,
            "site:77",
            "group:human scans",
            "group:robo scans & strawberry",
        ]
    );
    let ranks: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["rank"].clone())
        .collect();
    assert_eq!(ranks, [json!(1), json!(2), json!(null), json!(null)]);
    assert_eq!(page["items"][1]["rating"], 4.0);
    assert_eq!(
        page["items"][1]["distribution"],
        json!([0, 0, 0, 5, 0, 0, 0, 0, 0, 0])
    );

    // The rank is the server's, not the filter's: the only site is still second.
    let sites = list(&s, "(kind: SITE)", None).await;
    assert_eq!(sites["items"][0]["rank"], 2);
    // Losing a score loses the rank, and the next one moves up.
    let row = rate(&s, ROBO, None, "bobtok").await;
    assert_eq!(
        (&row["ratingCount"], &row["rank"]),
        (&json!(4), &json!(null))
    );
    assert_eq!(one(&s, "site:77", None).await["rank"], 1);
}

#[tokio::test]
async fn a_scanlator_has_a_comment_thread_like_a_series() {
    let (s, st) = seeded().await;
    const ROBO: &str = "group:robo scans";
    let post_q = |target: &str, parent: Option<&str>| {
        let parent = parent.map_or(String::new(), |id| format!(r#", parentId: "{id}""#));
        format!(
            r#"mutation {{ postComment(input: {{ targetType: "scanlator", targetId: "{target}", body: "good work", hasSpoiler: false{parent} }}) {{ id targetType targetId parentId }} }}"#
        )
    };
    let thread_q = |target: &str| {
        format!(
            r#"{{ comments(targetType: "scanlator", targetId: "{target}") {{ total items {{ id parentId body deletedBy likes myVote author {{ username }} }} }} }}"#
        )
    };
    let count = |token: Option<&'static str>| {
        let s = s.clone();
        async move { one(&s, ROBO, token).await["commentCount"].clone() }
    };

    // Signed-in only, and only on a scanlator the list shows the viewer.
    assert_eq!(
        err(&run(&s, &post_q(ROBO, None), None).await),
        "Not authenticated"
    );
    for id in [
        "group:nobody",
        "group:human scans & robo scans",
        "site:mangadex",
        "site:88",
        "w1",
    ] {
        assert_eq!(
            err(&run(&s, &post_q(id, None), Some("bobtok")).await),
            "No such scanlator",
            "{id}"
        );
    }
    assert_eq!(count(None).await, 0);

    // Post: the thread is stored under the id as the list writes it.
    let root = ok(run(&s, &post_q("group:Robo Scans", None), Some("bobtok")).await);
    let root = &root["postComment"];
    assert_eq!(
        (&root["targetType"], &root["targetId"], &root["parentId"]),
        (&json!("scanlator"), &json!(ROBO), &json!(null))
    );
    let root_id = root["id"].as_str().unwrap().to_string();
    // A reply, a like and the reply's notification work as on the other targets.
    let reply = ok(run(&s, &post_q(ROBO, Some(&root_id)), Some("admintok")).await);
    let reply_id = reply["postComment"]["id"].as_str().unwrap().to_string();
    assert_eq!(reply["postComment"]["parentId"], root_id.as_str());
    let vote = format!(
        r#"mutation {{ voteComment(commentId: "{root_id}", value: 1) {{ likes myVote }} }}"#
    );
    assert_eq!(
        ok(run(&s, &vote, Some("admintok")).await)["voteComment"],
        json!({ "likes": 1, "myVote": 1 })
    );
    // A reply must stay on its thread.
    assert_eq!(
        err(&run(&s, &post_q("site:77", Some(&root_id)), Some("admintok")).await),
        "reply target not found on this thread"
    );

    // List: by the id in any case; the site's thread is another thread.
    for target in [ROBO, "group:ROBO SCANS"] {
        let thread = ok(run(&s, &thread_q(target), Some("admintok")).await);
        let thread = &thread["comments"];
        assert_eq!(thread["total"], 1, "{target}");
        assert_eq!(
            thread["items"],
            json!([
                { "id": root_id, "parentId": null, "body": "good work", "deletedBy": null,
                  "likes": 1, "myVote": 1, "author": { "username": "bob" } },
                { "id": reply_id, "parentId": root_id, "body": "good work", "deletedBy": null,
                  "likes": 0, "myVote": 0, "author": { "username": "admin" } },
            ])
        );
    }
    let other = ok(run(&s, &thread_q("site:77"), None).await);
    assert_eq!(other["comments"]["total"], 0);
    assert_eq!(count(None).await, 2);
    assert_eq!(one(&s, "site:77", None).await["commentCount"], 0);

    // The poster's own lists carry the comment with no series behind it.
    let mine = ok(run(
        &s,
        "{ myComments { total items { targetType targetId seriesId seriesTitle chapterNumber replyCount likes } } }",
        Some("bobtok"),
    )
    .await);
    assert_eq!(
        mine["myComments"],
        json!({ "total": 1, "items": [{
            "targetType": "scanlator", "targetId": ROBO, "seriesId": null,
            "seriesTitle": null, "chapterNumber": null, "replyCount": 1, "likes": 1,
        }] })
    );
    let bell = ok(run(
        &s,
        "{ notifications { kind targetType targetId seriesId commentId } }",
        Some("bobtok"),
    )
    .await);
    assert_eq!(
        bell["notifications"],
        json!([
            { "kind": "like_milestone", "targetType": "scanlator", "targetId": ROBO,
              "seriesId": null, "commentId": root_id },
            { "kind": "reply", "targetType": "scanlator", "targetId": ROBO,
              "seriesId": null, "commentId": reply_id },
        ])
    );
    let activity = ok(run(
        &s,
        "{ myActivity { kind targetType targetId } }",
        Some("bobtok"),
    )
    .await);
    assert_eq!(
        activity["myActivity"],
        json!([{ "kind": "comment", "targetType": "scanlator", "targetId": ROBO }])
    );

    // A banned author's comments leave the thread and the count.
    let ban = |banned: bool| {
        let s = s.clone();
        async move {
            let q = format!(
                r#"mutation {{ banUser(userId: "{BOB}", banned: {banned}) {{ isBanned }} }}"#
            );
            ok(run(&s, &q, Some("admintok")).await);
        }
    };
    ban(true).await;
    assert_eq!(count(None).await, 0);
    let thread = ok(run(&s, &thread_q(ROBO), None).await);
    assert_eq!(thread["comments"]["total"], 0);
    ban(false).await;
    sqlx::query(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) \
         VALUES ($1, $2, '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
    )
    .bind(auth::hash_token("bobtok"))
    .bind(crate::db_types::uuid(BOB).unwrap())
    .execute(&st.pool)
    .await
    .unwrap();
    assert_eq!(count(None).await, 2);

    // Deleting a comment that has a reply leaves a tombstone: the thread keeps it, the
    // count does not.
    let delete = format!(r#"mutation {{ deleteComment(commentId: "{root_id}") }}"#);
    assert_eq!(
        ok(run(&s, &delete, Some("bobtok")).await)["deleteComment"],
        true
    );
    let thread = ok(run(&s, &thread_q(ROBO), None).await);
    assert_eq!(thread["comments"]["items"][0]["deletedBy"], "author");
    assert_eq!(thread["comments"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(count(None).await, 1);
    // Deleting the last reply removes the whole thread.
    let delete = format!(r#"mutation {{ deleteComment(commentId: "{reply_id}") }}"#);
    assert_eq!(
        ok(run(&s, &delete, Some("admintok")).await)["deleteComment"],
        true
    );
    assert_eq!(count(None).await, 0);
    let comments: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM comments")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(comments, 0);

    // The other targets still validate as before.
    let bad = r#"{ comments(targetType: "group", targetId: "x") { total } }"#;
    assert!(err(&run(&s, bad, None).await).contains("invalid comment target type"));
}

/// Turn a reader's adult-content opt-in on or off.
async fn set_show_nsfw(st: &AppState, user: &str, on: bool) {
    sqlx::query("UPDATE users SET show_nsfw = $2 WHERE id = $1")
        .bind(crate::db_types::uuid(user).unwrap())
        .bind(on)
        .execute(&st.pool)
        .await
        .unwrap();
}

/// Store five scores on an id without asking whether such a scanlator is listed: what
/// the table holds for a scanlator that left the list after it was rated.
async fn store_five(st: &AppState, id: &str, score: i32) {
    for user in &USERS[1..] {
        sqlx::query(
            "INSERT INTO scanlator_rating (scanlator_id, user_id, score, created_at, updated_at) \
             VALUES ($1, $2, $3, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(crate::db_types::uuid(user.0).unwrap())
        .bind(score)
        .execute(&st.pool)
        .await
        .unwrap();
    }
}

async fn stored_scores(st: &AppState, id: &str) -> i64 {
    crate::db_types::query_scalar("SELECT COUNT(*) FROM scanlator_rating WHERE scanlator_id = $1")
        .bind(id)
        .fetch_one(&st.pool)
        .await
        .unwrap()
}

/// The bare row `rateScanlator` answers with after a removal on an unlisted id.
fn bare(id: &str, kind: &str, name: &str, chapters: serde_json::Value) -> serde_json::Value {
    json!({
        "id": id, "kind": kind, "name": name, "lang": null, "logoUrl": null, "seriesCount": 0,
        "chapterCount": chapters, "ai": null, "rating": null, "ratingCount": 0,
        "rank": null, "distribution": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], "myScore": null,
        "commentCount": 0,
    })
}

#[tokio::test]
async fn only_a_scanlator_that_exists_ranks_and_a_score_can_always_be_removed() {
    let (s, st) = seeded().await;
    const ROBO: &str = "group:robo scans";
    const FOLDED: &str = "group:human scans & robo scans";
    const GONE: &str = "group:nobody";
    const READERS: [&str; 5] = ["bobtok", "r1tok", "r2tok", "r3tok", "r4tok"];

    // Five tens each on a joint credit that is counted with its members, on a name no
    // chapter carries any more, and on the adult-only site, which exists but is hidden
    // from a viewer without the opt-in. Then five nines on a group everyone sees.
    for id in [FOLDED, GONE, "site:88"] {
        store_five(&st, id, 10).await;
    }
    for token in READERS {
        rate(&s, ROBO, Some(9), token).await;
    }

    // The two that do not exist hold no rank and are no row. The hidden site exists, so
    // it keeps the first place: the places this viewer is shown start at 2.
    let page = list(&s, "", None).await;
    assert_eq!(page["total"], 4);
    assert_eq!(ids(&page)[0], ROBO);
    assert_eq!(page["items"][0]["rank"], 2);
    for id in [FOLDED, GONE, "site:88"] {
        assert_eq!(one(&s, id, None).await, json!(null), "{id}");
    }
    // A viewer who sees adult content is shown the site in its place.
    set_show_nsfw(&st, BOB, true).await;
    let page = list(&s, "", Some("bobtok")).await;
    assert_eq!(page["total"], 5);
    assert_eq!(ids(&page)[..2], ["site:88", ROBO]);
    assert_eq!(
        (&page["items"][0]["rank"], &page["items"][1]["rank"]),
        (&json!(1), &json!(2))
    );
    assert_eq!(page["items"][0]["myScore"], 10);
    // The stored scores of the two others are not part of any member's numbers either.
    let human = one(&s, "group:human scans", None).await;
    assert_eq!(
        (&human["ratingCount"], &human["rating"]),
        (&json!(0), &json!(null))
    );

    // Without the opt-in again, bob cannot set a score on the site, but can take back
    // the one he has. The answer says nothing about the site.
    set_show_nsfw(&st, BOB, false).await;
    assert_eq!(
        err(&run(&s, &rate_q("site:88", Some(3)), Some("bobtok")).await),
        "No such scanlator"
    );
    assert_eq!(stored_scores(&st, "site:88").await, 5);
    for _ in 0..2 {
        assert_eq!(
            rate(&s, "site:88", None, "bobtok").await,
            bare("site:88", "SITE", "88", json!(null))
        );
    }
    assert_eq!(stored_scores(&st, "site:88").await, 4);
    // With four scores left the site holds no place, and the group moves up.
    assert_eq!(one(&s, ROBO, None).await["rank"], 1);

    // The same for a credit that was folded into its members and for a name that is
    // gone, in any case of the name: only the viewer's own row goes.
    assert_eq!(
        rate(&s, "group:Human Scans & Robo Scans", None, "bobtok").await,
        bare(FOLDED, "GROUP", "human scans & robo scans", json!(0))
    );
    assert_eq!(stored_scores(&st, FOLDED).await, 4);
    assert_eq!(
        rate(&s, GONE, None, "r1tok").await,
        bare(GONE, "GROUP", "nobody", json!(0))
    );
    assert_eq!(stored_scores(&st, GONE).await, 4);
    // A removal with nothing stored is the same answer.
    assert_eq!(
        rate(&s, "site:999", None, "bobtok").await,
        bare("site:999", "SITE", "999", json!(null))
    );
    // A listed scanlator still answers with its full row.
    let row = rate(&s, ROBO, None, "bobtok").await;
    assert_eq!(
        (&row["name"], &row["ratingCount"], &row["chapterCount"]),
        (&json!("Robo Scans"), &json!(4), &json!(2))
    );

    // An id of the wrong form is refused, and removal stays signed-in only.
    for id in ["nonsense", "", "group:", "site:mangadex"] {
        assert_eq!(
            err(&run(&s, &rate_q(id, None), Some("bobtok")).await),
            "No such scanlator",
            "{id:?}"
        );
    }
    assert_eq!(
        err(&run(&s, &rate_q("site:88", None), None).await),
        "Not authenticated"
    );
}

#[tokio::test]
async fn a_group_with_only_adult_chapters_is_hidden_without_the_opt_in() {
    let (s, st) = seeded().await;
    const LEWD: &str = "group:lewd scans";
    const ROBO: &str = "group:robo scans";
    // On the adult work: a group that released nothing else, and one more chapter of a
    // group that also released the safe work.
    sqlx::query(
        "INSERT INTO chapter (id, source_series_id, external_id, scanlator, created_at) VALUES \
           ('c6', 'sy2', 'e6', 'Lewd Scans', '2026-01-01T00:00:00Z'), \
           ('c7', 'sy2', 'e7', 'Robo Scans', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    set_show_nsfw(&st, USERS[2].0, true).await;
    let post_q = |target: &str| {
        format!(
            r#"mutation {{ postComment(input: {{ targetType: "scanlator", targetId: "{target}", body: "hello", hasSpoiler: false }}) {{ targetId }} }}"#
        )
    };
    let thread_q = |target: &str| {
        format!(r#"{{ comments(targetType: "scanlator", targetId: "{target}") {{ total }} }}"#)
    };

    // Signed out, and signed in without the opt-in: the adult-only group is no row, and
    // the other group is counted by what this viewer can open.
    for token in [None, Some("bobtok")] {
        let page = list(&s, "(sort: NAME)", token).await;
        assert_eq!(
            ids(&page),
            [
                "site:77",
                "group:human scans",
                ROBO,
                "group:robo scans & strawberry",
            ],
            "{token:?}"
        );
        assert_eq!(
            (
                &page["items"][2]["chapterCount"],
                &page["items"][2]["seriesCount"]
            ),
            (&json!(2), &json!(2))
        );
        assert_eq!(one(&s, LEWD, token).await, json!(null));
        assert_eq!(ids(&list(&s, r#"(search: "lewd")"#, token).await).len(), 0);
        // Its thread is closed to them, like a hidden site's and an unknown id's.
        for id in [LEWD, "group:LEWD SCANS", "site:88", "group:nobody", "w1"] {
            assert_eq!(
                err(&run(&s, &thread_q(id), token).await),
                "No such scanlator",
                "{id} {token:?}"
            );
        }
    }
    for q in [rate_q(LEWD, Some(8)), post_q(LEWD)] {
        assert_eq!(err(&run(&s, &q, Some("bobtok")).await), "No such scanlator");
    }
    assert_eq!(stored_scores(&st, LEWD).await, 0);

    // With the opt-in: it is a scanlator like any other, and every chapter counts.
    let page = list(&s, "(sort: NAME)", Some("r1tok")).await;
    assert_eq!(
        ids(&page),
        [
            "site:88",
            "site:77",
            "group:human scans",
            LEWD,
            ROBO,
            "group:robo scans & strawberry",
        ]
    );
    assert_eq!(
        (
            &page["items"][3]["name"],
            &page["items"][3]["chapterCount"],
            &page["items"][3]["seriesCount"]
        ),
        (&json!("Lewd Scans"), &json!(1), &json!(1))
    );
    assert_eq!(
        (
            &page["items"][4]["chapterCount"],
            &page["items"][4]["seriesCount"]
        ),
        (&json!(3), &json!(3))
    );
    assert_eq!(rate(&s, LEWD, Some(8), "r1tok").await["myScore"], 8);
    assert_eq!(
        ok(run(&s, &post_q(LEWD), Some("r1tok")).await)["postComment"]["targetId"],
        LEWD
    );
    for id in [LEWD, "site:88"] {
        let thread = ok(run(&s, &thread_q(id), Some("r1tok")).await);
        assert_eq!(thread["comments"]["total"], i64::from(id == LEWD), "{id}");
    }
    // The score and the thread are there, and still not shown without the opt-in.
    assert_eq!(one(&s, LEWD, None).await, json!(null));
    assert_eq!(
        err(&run(&s, &thread_q(LEWD), Some("bobtok")).await),
        "No such scanlator"
    );
    // A thread everyone may read still opens signed out.
    assert_eq!(
        ok(run(&s, &thread_q(ROBO), None).await)["comments"]["total"],
        0
    );

    // The admin list does not follow a viewer: every credit as written, every chapter.
    let admin = ok(run(
        &s,
        r#"{ scanlators(search: "scans") { total } }"#,
        Some("admintok"),
    )
    .await);
    assert_eq!(admin["scanlators"]["total"], 5);
}

#[tokio::test]
async fn rating_is_rate_limited_per_account() {
    let (s, st) = seeded().await;
    const ROBO: &str = "group:robo scans";
    // Thirty writes in the minute pass, removals and refused ids included.
    for n in 0..28 {
        rate(&s, ROBO, Some(1 + n % 10), "bobtok").await;
    }
    rate(&s, ROBO, None, "bobtok").await;
    assert_eq!(
        err(&run(&s, &rate_q("group:nobody", Some(5)), Some("bobtok")).await),
        "No such scanlator"
    );
    // The next one does not, whatever it asks for, and writes nothing.
    for score in [Some(7), None] {
        let refused = run(&s, &rate_q(ROBO, score), Some("bobtok")).await;
        assert!(
            err(&refused).starts_with("You are rating too fast. Try again in "),
            "{}",
            err(&refused)
        );
    }
    assert_eq!(stored_scores(&st, ROBO).await, 0);
    // The limit is on the account: another reader rates as usual.
    assert_eq!(rate(&s, ROBO, Some(7), "admintok").await["myScore"], 7);
}

/// The contract another client is written against: names, nullability and defaults.
#[tokio::test]
async fn the_schema_exposes_the_scanlator_rating_contract() {
    let (s, _st) = seeded().await;
    let sdl = s.sdl();
    for part in [
        "enum ScanlatorKind {\n\tSITE\n\tGROUP\n}",
        "enum ScanlatorSort {\n\tTOP\n\tVOTES\n\tCHAPTERS\n\tNAME\n}",
        "scanlatorRatings(search: String, kind: ScanlatorKind, mine: Boolean, sort: ScanlatorSort, page: Int = 1): RatedScanlatorPage!",
        "ratedScanlator(id: ID!): RatedScanlator",
        "rateScanlator(id: ID!, score: Int): RatedScanlator!",
        "\tid: ID!\n\tkind: ScanlatorKind!\n\tname: String!\n\tlang: String\n\tlogoUrl: String\n\tseriesCount: Int!\n\tchapterCount: Int\n\tai: ScanlatorAi\n\trating: Float\n\tratingCount: Int!\n\trank: Int\n\tdistribution: [Int!]!\n\tmyScore: Int\n\tcommentCount: Int!\n",
        "\titems: [RatedScanlator!]!\n\tpage: Int!\n\thasNextPage: Boolean!\n\ttotal: Int!\n",
    ] {
        assert!(strip_descriptions(&sdl).contains(part), "missing from the schema: {part}");
    }
}

/// The SDL without its doc strings, so a field list can be matched as one block.
fn strip_descriptions(sdl: &str) -> String {
    let mut out = String::new();
    let mut in_doc = false;
    for line in sdl.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("\"\"\"") {
            // A one-line doc string opens and closes on the same line.
            if !(trimmed.len() > 3 && trimmed.ends_with("\"\"\"")) {
                in_doc = !in_doc;
            }
            continue;
        }
        if !in_doc {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
