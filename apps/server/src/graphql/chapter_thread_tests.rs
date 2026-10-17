const A: &str = "a1111111-1111-4111-8111-111111111111";
const B: &str = "8";
const OTHER: &str = "a2222222-2222-4222-8222-222222222222";
const UNLINKED: &str = "a3333333-3333-4333-8333-333333333333";
const THIRD: &str = "a4444444-4444-4444-8444-444444444444";
const BOB: &str = "ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c";

async fn seed_copies(pool: &Database) {
    // A and B share entry one. OTHER is another entry of the same work;
    // UNLINKED has the same chapter number as A and B but no entry link.
    sqlx::raw_sql(&format!(
        r#"
        INSERT INTO work (id, primary_title, created_at, updated_at)
        VALUES ('w_thread', 'Shared chapter', now(), now());
        INSERT INTO source_series
            (id, work_id, source_type, source_id, source_key, created_at)
        VALUES
            ('ss_thread_md', 'w_thread', 'mangadex', 'mangadex', 'thread-md', now()),
            ('ss_thread_sw', 'w_thread', 'suwayomi', 'src', '42', now());
        INSERT INTO chapter
            (id, source_series_id, external_id, number, chapter_key, created_at)
        VALUES
            ('ch_a', 'ss_thread_md', '{A}', '1', '100', now()),
            ('ch_b', 'ss_thread_sw', '{B}', '1', '100', now()),
            ('ch_other', 'ss_thread_md', '{OTHER}', '2', '200', now()),
            ('ch_unlinked', 'ss_thread_md', '{UNLINKED}', '1', '100', now());
        INSERT INTO work_chapter (id, work_id, match_key, sort_key, number, created_at)
        VALUES
            ('wc_one', 'w_thread', '100', 1, '1', now()),
            ('wc_two', 'w_thread', '200', 2, '2', now());
        INSERT INTO work_chapter_link (chapter_id, work_chapter_id, linked_by, linked_at)
        VALUES
            ('ch_a', 'wc_one', 'matcher', now()),
            ('ch_b', 'wc_one', 'matcher', now()),
            ('ch_other', 'wc_two', 'matcher', now());
        "#
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn json(schema: &ApiSchema, query: &str, token: Option<&str>) -> serde_json::Value {
    let r = exec(schema, query, token, "chapter-thread").await;
    assert!(r.errors.is_empty(), "{query}: {:?}", r.errors);
    r.data.into_json().unwrap()
}

fn post_query(target: &str, parent: Option<&str>) -> String {
    let parent = parent.map_or(String::new(), |id| format!(r#",parentId:"{id}""#));
    format!(
        r#"mutation {{ postComment(input:{{targetType:"chapter",targetId:"{target}",body:"chapter comment",hasSpoiler:false{parent}}}) {{ id }} }}"#
    )
}

async fn post(schema: &ApiSchema, token: &str, target: &str, parent: Option<&str>) -> String {
    json(schema, &post_query(target, parent), Some(token)).await["postComment"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn thread(
    schema: &ApiSchema,
    target: &str,
    page: i32,
    token: Option<&str>,
) -> serde_json::Value {
    json(schema, &format!(r#"{{ comments(targetType:"chapter",targetId:"{target}",page:{page}) {{ total page hasNextPage items {{ id targetType targetId parentId body deletedBy author {{ id username }} likes dislikes myVote }} }} }}"#), token).await["comments"].clone()
}

fn ids(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_ids(page: &serde_json::Value, expected: &[&str]) {
    let mut actual = ids(page);
    actual.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(actual, expected);
}

fn comment<'a>(page: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

#[tokio::test]
async fn two_copies_of_one_entry_share_a_thread() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let mut roots = Vec::new();
    for i in 0..22 {
        // Alternate authors to stay below the per-account posting limit.
        let (target, token) = if i % 2 == 0 {
            (A, "bobtok")
        } else {
            (B, "admintok")
        };
        let root = post(&s, token, target, None).await;
        // Distinct timestamps make the page boundary and returned order deterministic.
        sqlx::query("UPDATE comments SET created_at=$1 WHERE id=$2")
            .bind(crate::db_types::date(&format!("2026-01-01T00:00:{i:02}Z")).unwrap())
            .bind(crate::db_types::uuid(&root).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        roots.push(root);
    }
    for target in [A, B] {
        let first = thread(&s, target, 1, None).await;
        assert_eq!(first["total"], 22);
        assert_eq!(first["page"], 1);
        assert_eq!(first["hasNextPage"], true);
        assert_eq!(ids(&first), roots[..20]);
        let second = thread(&s, target, 2, None).await;
        assert_eq!(second["total"], 22);
        assert_eq!(second["page"], 2);
        assert_eq!(second["hasNextPage"], false);
        assert_eq!(ids(&second), roots[20..]);
        let last = thread(&s, target, 3, None).await;
        assert_eq!(last["total"], 22);
        assert_eq!(last["hasNextPage"], false);
        assert_ids(&last, &[]);
    }
}

#[tokio::test]
async fn reply_across_copies_of_one_entry_keeps_its_own_target() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let root = post(&s, "bobtok", A, None).await;
    let reply = post(&s, "admintok", B, Some(&root)).await;
    let stored: String = sqlx::query_scalar("SELECT target_id FROM comments WHERE id=$1")
        .bind(crate::db_types::uuid(&reply).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, B);
    for target in [A, B] {
        let data = thread(&s, target, 1, None).await;
        assert_eq!(data["total"], 1);
        assert_ids(&data, &[&root, &reply]);
        assert!(comment(&data, &root)["parentId"].is_null());
        assert_eq!(comment(&data, &root)["targetId"], A);
        assert_eq!(comment(&data, &reply)["parentId"], root);
        assert_eq!(comment(&data, &reply)["targetId"], B);
    }
}

#[tokio::test]
async fn reply_across_entries_or_to_unlinked_copy_is_refused() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let linked = post(&s, "bobtok", A, None).await;
    for target in [OTHER, UNLINKED] {
        let outside = post(&s, "bobtok", target, None).await;
        for (target, parent) in [(B, outside.as_str()), (target, linked.as_str())] {
            let r = exec(
                &s,
                &post_query(target, Some(parent)),
                Some("admintok"),
                "chapter-thread",
            )
            .await;
            assert_eq!(first_error(&r), "reply target not found on this thread");
        }
    }
}

#[tokio::test]
async fn unlinked_copy_keeps_its_own_thread() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let linked = post(&s, "bobtok", B, None).await;
    let own = post(&s, "admintok", UNLINKED, None).await;
    let isolated = thread(&s, UNLINKED, 1, None).await;
    assert_eq!(isolated["total"], 1);
    assert_ids(&isolated, &[&own]);
    for target in [A, B] {
        let data = thread(&s, target, 1, None).await;
        assert_eq!(data["total"], 1);
        assert_ids(&data, &[&linked]);
    }
}

#[tokio::test]
async fn split_orphan_reply_shows_top_level_on_its_side_only() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let root = post(&s, "bobtok", A, None).await;
    let reply = post(&s, "admintok", B, Some(&root)).await;
    let child = post(&s, "bobtok", B, Some(&reply)).await;
    sqlx::query("DELETE FROM work_chapter_link WHERE chapter_id='ch_b'")
        .execute(&pool)
        .await
        .unwrap();
    let parent_side = thread(&s, A, 1, None).await;
    assert_eq!(parent_side["total"], 1);
    assert_ids(&parent_side, &[&root]);
    let reply_side = thread(&s, B, 1, None).await;
    assert_eq!(
        reply_side["total"], 1,
        "the orphan is a root; its child is not"
    );
    assert_ids(&reply_side, &[&reply, &child]);
    assert_eq!(comment(&reply_side, &reply)["parentId"], root);
    assert_eq!(comment(&reply_side, &child)["parentId"], reply);
}

#[tokio::test]
async fn detached_numeric_comments_stay_hidden() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let live = post(&s, "bobtok", A, None).await;
    // Include a detached descendant to check the recursive target filter too.
    sqlx::query(
        "INSERT INTO comments (id,target_type,target_id,user_id,body,created_at,parent_id) \
         VALUES ($1,'detached_chapter','old-generation:8',$2,'old root',now(),NULL), \
                ($3,'detached_chapter','old-generation:8',$2,'old reply',now(),$4)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(crate::db_types::uuid(BOB).unwrap())
    .bind(uuid::Uuid::new_v4())
    .bind(crate::db_types::uuid(&live).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    for target in [A, B] {
        let data = thread(&s, target, 1, None).await;
        assert_eq!(data["total"], 1);
        assert_ids(&data, &[&live]);
    }
    for target in [OTHER, UNLINKED, "old-generation:8"] {
        let data = thread(&s, target, 1, None).await;
        assert_eq!(data["total"], 0);
        assert_ids(&data, &[]);
    }
    let r = exec(
        &s,
        r#"{ comments(targetType:"detached_chapter",targetId:"old-generation:8") { total } }"#,
        None,
        "chapter-thread",
    )
    .await;
    assert_eq!(first_error(&r), "invalid comment target type: \"detached_chapter\" (expected \"chapter\", \"series\" or \"scanlator\")");
}

#[tokio::test]
async fn banned_author_tombstone_and_votes_across_copies() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let root = post(&s, "admintok", A, None).await;
    let banned = post(&s, "bobtok", B, Some(&root)).await;
    post(&s, "admintok", A, Some(&banned)).await;
    let tombstone = post(&s, "bobtok", A, None).await;
    let bridge_reply = post(&s, "admintok", B, Some(&tombstone)).await;
    let liked = post(&s, "admintok", B, None).await;
    json(
        &s,
        &format!(r#"mutation {{ voteComment(commentId:"{liked}",value:1) {{ likes }} }}"#),
        Some("bobtok"),
    )
    .await;
    for target in [A, B] {
        let data = thread(&s, target, 1, Some("bobtok")).await;
        let voted = comment(&data, &liked);
        assert_eq!(voted["likes"], 1);
        assert_eq!(voted["dislikes"], 0);
        assert_eq!(voted["myVote"], 1);
    }
    assert_eq!(
        json(
            &s,
            &format!(r#"mutation {{ deleteComment(commentId:"{tombstone}") }}"#),
            Some("admintok")
        )
        .await["deleteComment"],
        true
    );
    json(
        &s,
        &format!(r#"mutation {{ banUser(userId:"{BOB}",banned:true) {{ isBanned }} }}"#),
        Some("admintok"),
    )
    .await;
    for target in [A, B] {
        let data = thread(&s, target, 1, None).await;
        assert_eq!(data["total"], 3);
        assert_ids(&data, &[&root, &tombstone, &bridge_reply, &liked]);
        let deleted = comment(&data, &tombstone);
        assert_eq!(deleted["deletedBy"], "moderator");
        assert_eq!(deleted["body"], "");
        assert_eq!(deleted["author"]["id"], "");
        assert_eq!(comment(&data, &bridge_reply)["parentId"], tombstone);
        assert_eq!(comment(&data, &liked)["likes"], 1);
        assert_eq!(comment(&data, &liked)["myVote"], 0);
    }
}

#[tokio::test]
async fn vanished_copy_comments_leave_the_thread() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    sqlx::raw_sql(&format!(
        "INSERT INTO chapter (id,source_series_id,external_id,number,chapter_key,created_at) \
         VALUES ('ch_third','ss_thread_md','{THIRD}','1','100',now()); \
         INSERT INTO work_chapter_link (chapter_id,work_chapter_id,linked_by,linked_at) \
         VALUES ('ch_third','wc_one','matcher',now())",
    ))
    .execute(&pool)
    .await
    .unwrap();
    let a = post(&s, "bobtok", A, None).await;
    let b = post(&s, "admintok", B, None).await;
    let vanished = post(&s, "bobtok", THIRD, None).await;
    assert_ids(&thread(&s, A, 1, None).await, &[&a, &b, &vanished]);
    assert_eq!(
        sqlx::query("DELETE FROM chapter WHERE id='ch_third'")
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    let links: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_chapter_link WHERE chapter_id='ch_third'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(links, 0);
    for target in [A, B] {
        let data = thread(&s, target, 1, None).await;
        assert_eq!(data["total"], 2);
        assert_ids(&data, &[&a, &b]);
    }
    let old = thread(&s, THIRD, 1, None).await;
    assert_eq!(old["total"], 1);
    assert_ids(&old, &[&vanished]);
}

#[tokio::test]
async fn reply_count_and_notification_cross_copies() {
    let (s, pool) = setup_full(100).await;
    seed_copies(&pool).await;
    let root = post(&s, "bobtok", A, None).await;
    let reply = post(&s, "admintok", B, Some(&root)).await;
    let data = json(&s, "{ myComments { total items { id targetId replyCount } } notifications { kind commentId targetType targetId } }", Some("bobtok")).await;
    assert_eq!(data["myComments"]["total"], 1);
    assert_eq!(
        data["myComments"]["items"],
        serde_json::json!([
            {"id": root, "targetId": A, "replyCount": 1}
        ])
    );
    assert_eq!(
        data["notifications"],
        serde_json::json!([
            {"kind": "reply", "commentId": reply, "targetType": "chapter", "targetId": B}
        ])
    );
}
