const BOB: &str = "ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c";
const ADMIN: &str = "964771b0-a2fb-4849-857a-6198947115ec";
const MEDIA: &str = "050fb1d0-6dcf-47c4-9c58-85593d33d3c0";

async fn json(schema: &ApiSchema, query: &str, token: Option<&str>) -> serde_json::Value {
    let r = exec(schema, query, token, "comment-deletion").await;
    assert!(r.errors.is_empty(), "{query}: {:?}", r.errors);
    r.data.into_json().unwrap()
}

async fn post(schema: &ApiSchema, token: &str, parent: Option<&str>, media: bool) -> String {
    let parent = parent.map_or(String::new(), |id| format!(r#",parentId:"{id}""#));
    let media = if media {
        format!(r#",mediaId:"{MEDIA}""#)
    } else {
        String::new()
    };
    json(schema, &format!(r#"mutation {{ postComment(input:{{targetType:"series",targetId:"s1",body:"private text",hasSpoiler:true{parent}{media}}}) {{ id deletedBy }} }}"#), Some(token))
        .await["postComment"]["id"].as_str().unwrap().to_owned()
}

async fn delete(schema: &ApiSchema, token: &str, id: &str) -> bool {
    json(
        schema,
        &format!(r#"mutation {{ deleteComment(commentId:"{id}") }}"#),
        Some(token),
    )
    .await["deleteComment"]
        .as_bool()
        .unwrap()
}

async fn thread(schema: &ApiSchema, token: Option<&str>) -> serde_json::Value {
    json(schema, r#"{ comments(targetType:"series",targetId:"s1") { total items { id parentId body hasSpoiler deletedBy author { id username avatarUrl } mediaUrl mediaWidth mediaHeight likes dislikes myVote } } }"#, token).await["comments"].clone()
}

async fn stage_media(pool: &Database) {
    sqlx::query("INSERT INTO comment_media(id,user_id,webp,width,height,created_at) VALUES($1,$2,decode('00','hex'),800,600,now())")
        .bind(crate::db_types::uuid(MEDIA).unwrap())
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(pool).await.unwrap();
}

async fn row_count(pool: &Database, table: &str, column: &str, id: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = $1"))
        .bind(crate::db_types::uuid(id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn author_deletes_own_leaf_and_cleans_attachments() {
    let (s, pool) = setup_full(100).await;
    stage_media(&pool).await;
    let id = post(&s, "bobtok", None, true).await;
    json(
        &s,
        &format!(r#"mutation {{ voteComment(commentId:"{id}",value:1) {{ likes }} }}"#),
        Some("admintok"),
    )
    .await;
    assert_eq!(
        row_count(&pool, "notifications", "comment_id", &id).await,
        1
    );
    assert!(delete(&s, "bobtok", &id).await);
    for table in [
        "comments",
        "comment_media",
        "comment_votes",
        "notifications",
    ] {
        let column = if table == "comments" {
            "id"
        } else {
            "comment_id"
        };
        assert_eq!(row_count(&pool, table, column, &id).await, 0, "{table}");
    }
    assert!(!delete(&s, "bobtok", &id).await);
}

#[tokio::test]
async fn author_tombstone_scrubs_content_preserves_reply_and_notifications() {
    let (s, pool) = setup_full(100).await;
    stage_media(&pool).await;
    sqlx::query("UPDATE users SET avatar_url='/private-avatar.webp' WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let root = post(&s, "bobtok", None, true).await;
    // This reply's notification points to the reply, not the deleted parent.
    let reply = post(&s, "admintok", Some(&root), false).await;
    json(
        &s,
        &format!(r#"mutation {{ voteComment(commentId:"{root}",value:1) {{ likes }} }}"#),
        Some("admintok"),
    )
    .await;
    let before = thread(&s, Some("admintok")).await;
    assert!(delete(&s, "bobtok", &root).await);
    let after = thread(&s, Some("admintok")).await;
    let items = after["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(after["total"], 1);
    let tombstone = items.iter().find(|c| c["id"] == root).unwrap();
    assert_eq!(tombstone["deletedBy"], "author");
    assert_eq!(tombstone["body"], "");
    assert_eq!(tombstone["hasSpoiler"], false);
    assert_eq!(
        tombstone["author"],
        serde_json::json!({"id":"","username":"Deleted user","avatarUrl":null})
    );
    for field in ["mediaUrl", "mediaWidth", "mediaHeight"] {
        assert!(tombstone[field].is_null(), "{field}");
    }
    for field in ["likes", "dislikes", "myVote"] {
        assert_eq!(tombstone[field], 0, "{field}");
    }
    let original_reply = before["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == reply)
        .unwrap();
    assert_eq!(
        items.iter().find(|c| c["id"] == reply).unwrap(),
        original_reply
    );
    let stored: (String, bool, Option<String>) =
        sqlx::query_as("SELECT body,has_spoiler,deleted_by FROM comments WHERE id=$1")
            .bind(crate::db_types::uuid(&root).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, (String::new(), false, Some("author".into())));
    for table in ["comment_media", "comment_votes", "notifications"] {
        assert_eq!(
            row_count(&pool, table, "comment_id", &root).await,
            0,
            "{table}"
        );
    }
    assert_eq!(
        row_count(&pool, "notifications", "comment_id", &reply).await,
        1
    );
    // Delayed notification work must not resurrect a deleted comment's preview.
    let mut milestone = pool.begin().await.unwrap();
    crate::notify::record_like_milestone_on(&mut milestone, BOB, &root, "series", "s1", 5)
        .await
        .unwrap();
    milestone.commit().await.unwrap();
    crate::notify::record(
        &pool,
        ADMIN,
        "reply",
        Some(BOB),
        Some(&root),
        Some("series"),
        Some("s1"),
        None,
    )
    .await;
    assert_eq!(
        row_count(&pool, "notifications", "comment_id", &root).await,
        0
    );
    assert!(!delete(&s, "bobtok", &root).await);
    assert!(!delete(&s, "admintok", &root).await);
    assert_eq!(thread(&s, Some("admintok")).await, after);
}

#[tokio::test]
async fn non_author_cannot_delete_comment() {
    let (s, _pool) = setup_full(100).await;
    let id = post(&s, "admintok", None, false).await;
    let r = exec(
        &s,
        &format!(r#"mutation {{ deleteComment(commentId:"{id}") }}"#),
        Some("bobtok"),
        "comment-deletion",
    )
    .await;
    assert_eq!(first_error(&r), "Admin access required");
    assert!(thread(&s, None).await["items"][0]["deletedBy"].is_null());
}

#[tokio::test]
async fn moderator_tombstone_survives_author_ban_with_replies() {
    let (s, _pool) = setup_full(100).await;
    let root = post(&s, "bobtok", None, false).await;
    let reply = post(&s, "admintok", Some(&root), false).await;
    assert!(delete(&s, "admintok", &root).await);
    json(
        &s,
        &format!(r#"mutation {{ banUser(userId:"{BOB}",banned:true) {{ isBanned }} }}"#),
        Some("admintok"),
    )
    .await;
    let data = thread(&s, None).await;
    assert_eq!(data["total"], 1);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items.iter().find(|c| c["id"] == root).unwrap()["deletedBy"],
        "moderator"
    );
    assert_eq!(
        items.iter().find(|c| c["id"] == reply).unwrap()["author"]["id"],
        ADMIN
    );
}

#[tokio::test]
async fn last_reply_prunes_multiple_tombstones_but_stops_at_live_parent() {
    let (s, pool) = setup_full(100).await;
    let live = post(&s, "bobtok", None, false).await;
    let root = post(&s, "bobtok", Some(&live), false).await;
    let middle = post(&s, "admintok", Some(&root), false).await;
    let leaf = post(&s, "bobtok", Some(&middle), false).await;
    let sibling = post(&s, "admintok", Some(&middle), false).await;
    assert!(delete(&s, "bobtok", &root).await);
    // Admin deleting their own comment is still an author deletion.
    assert!(delete(&s, "admintok", &middle).await);
    let data = thread(&s, None).await;
    assert_eq!(
        data["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == middle)
            .unwrap()["deletedBy"],
        "author"
    );
    assert!(delete(&s, "bobtok", &leaf).await);
    assert_eq!(
        row_count(&pool, "comments", "id", &middle).await,
        1,
        "sibling keeps ancestors alive"
    );
    assert!(delete(&s, "admintok", &sibling).await);
    for id in [&root, &middle, &leaf, &sibling] {
        assert_eq!(row_count(&pool, "comments", "id", id).await, 0);
    }
    assert_eq!(row_count(&pool, "comments", "id", &live).await, 1);
}

#[tokio::test]
async fn tombstone_refuses_reply_vote_and_report() {
    let (s, _pool) = setup_full(100).await;
    let root = post(&s, "bobtok", None, false).await;
    post(&s, "admintok", Some(&root), false).await;
    assert!(delete(&s, "bobtok", &root).await);
    let queries = [
        (
            format!(
                r#"mutation {{ postComment(input:{{targetType:"series",targetId:"s1",body:"reply",hasSpoiler:false,parentId:"{root}"}}) {{ id }} }}"#
            ),
            "You cannot reply to a deleted comment.",
        ),
        (
            format!(r#"mutation {{ voteComment(commentId:"{root}",value:1) {{ likes }} }}"#),
            "You cannot vote on a deleted comment.",
        ),
        (
            format!(r#"mutation {{ voteComment(commentId:"{root}",value:0) {{ likes }} }}"#),
            "You cannot vote on a deleted comment.",
        ),
        (
            format!(
                r#"mutation {{ submitReport(input:{{kind:COMMENT_ABUSE,commentId:"{root}",detail:"This comment has already been deleted."}}) {{ id }} }}"#
            ),
            "That comment no longer exists.",
        ),
    ];
    for (query, expected) in queries {
        let r = exec(&s, &query, Some("admintok"), "comment-deletion").await;
        assert_eq!(first_error(&r), expected, "{query}");
    }
}

#[tokio::test]
async fn my_comments_excludes_tombstones_from_items_total_and_reply_count() {
    let (s, _pool) = setup_full(100).await;
    let live = post(&s, "bobtok", None, false).await;
    let deleted = post(&s, "bobtok", Some(&live), false).await;
    post(&s, "admintok", Some(&deleted), false).await;
    assert!(delete(&s, "bobtok", &deleted).await);
    let data = json(
        &s,
        "{ myComments { total items { id replyCount } } }",
        Some("bobtok"),
    )
    .await;
    assert_eq!(data["myComments"]["total"], 1);
    assert_eq!(
        data["myComments"]["items"],
        serde_json::json!([{"id": live, "replyCount": 0}])
    );
}

#[test]
fn comment_deleted_by_schema_contract_is_nullable_string() {
    let schema = Schema::build(QueryRoot, MutationRoot, EmptySubscription).finish();
    let sdl = schema.sdl();
    let comment = sdl
        .split("type Comment {\n")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    assert!(comment.contains("\tdeletedBy: String\n"), "{comment}");
}

#[test]
fn comment_tombstone_masks_stale_content_author_media_and_votes() {
    // Reading even an imported/stale row must never expose a tombstone's content.
    let comment = Comment::from(CommentJoin {
        id: "comment".into(),
        target_type: "series".into(),
        target_id: "s1".into(),
        parent_id: None,
        deleted_by: Some("moderator".into()),
        body: "private content".into(),
        has_spoiler: 1,
        created_at: "2020-01-01T00:00:00Z".into(),
        author_id: BOB.into(),
        author_username: "bob".into(),
        author_avatar: Some("/private-avatar.webp".into()),
        media_id: Some(MEDIA.into()),
        media_width: Some(800),
        media_height: Some(600),
        likes: 5,
        dislikes: 2,
        my_vote: 1,
    });
    assert_eq!(comment.deleted_by.as_deref(), Some("moderator"));
    assert_eq!(comment.body, "");
    assert!(!comment.has_spoiler);
    assert_eq!(comment.author.id.0, "");
    assert_eq!(comment.author.username, "Deleted user");
    assert!(comment.author.avatar_url.is_none());
    assert!(comment.media_url.is_none());
    assert!(comment.media_width.is_none());
    assert!(comment.media_height.is_none());
    assert_eq!(
        (comment.likes, comment.dislikes, comment.my_vote),
        (0, 0, 0)
    );
}
