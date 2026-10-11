//! Driver compatibility probes. These deliberately execute legacy placeholder styles;
//! the portability checker exempts only the named probe functions below.
use sqlx::{Connection, SqliteConnection};

async fn connection() -> SqliteConnection {
    let mut conn = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(version, "3.46.0");
    conn
}

#[tokio::test]
async fn numbered_parameters_reuse_and_follow_names() {
    let mut conn = connection().await;
    let row: (i64, i64, i64) = sqlx::query_as("SELECT $2, $1, $2")
        .bind(11_i64)
        .bind(22_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row, (22, 11, 22));
    let reused: (i64, i64) = sqlx::query_as("SELECT $1, $1")
        .bind(37_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(reused, (37, 37));
}

#[tokio::test]
async fn numbered_parameters_with_gaps_and_missing_arguments() {
    let mut conn = connection().await;
    let row: (i64, i64) = sqlx::query_as("SELECT $3, $1")
        .bind(11_i64)
        .bind(22_i64)
        .bind(33_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row, (33, 11));
    // The first missing argument stops binding, even a later available $1.
    let row: (Option<i64>, Option<i64>) = sqlx::query_as("SELECT $3, $1")
        .bind(11_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row, (None, None));
}

#[tokio::test]
async fn legacy_mixed_parameter_probe() {
    let mut conn = connection().await;
    let row: (i64, i64, i64, i64) = sqlx::query_as("SELECT ?, $2, ?, $1")
        .bind(11_i64)
        .bind(22_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row, (11, 22, 22, 11));
    let row: (i64, i64) = sqlx::query_as("SELECT $2, ?")
        .bind(11_i64)
        .bind(22_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row, (22, 11));
    let row: (i64, i64) = sqlx::query_as("SELECT ?2, ?1")
        .bind(11_i64)
        .bind(22_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(row, (22, 11));
}

#[tokio::test]
async fn placeholders_in_sql_literals_comments_and_json_are_text() {
    let mut conn = connection().await;
    let row: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT '$9 ? $1', $1 /* $200 ? */, json_extract('{\"path\":7}', '$.path'), \
         'literal $1' LIKE '%$1%' -- $100 ?\n",
    )
    .bind(42_i64)
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(row, ("$9 ? $1".into(), 42, 7, 1));
}

#[tokio::test]
async fn named_parameter_numbers_are_not_sqlite_slot_numbers() {
    let mut conn = connection().await;
    // $N is a name to SQLite: its numeric suffix can exceed the slot limit.
    let mut query = sqlx::query_scalar::<_, i64>("SELECT $32767");
    for value in 1..=32767_i64 {
        query = query.bind(value);
    }
    assert_eq!(query.fetch_one(&mut conn).await.unwrap(), 32767);
    let missing: Option<i64> = sqlx::query_scalar("SELECT $999999999")
        .bind(1_i64)
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(missing, None);
    let options: Vec<String> = sqlx::query_scalar("PRAGMA compile_options")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert!(options.iter().any(|o| o == "MAX_VARIABLE_NUMBER=32766"));
    for count in [32766, 32767] {
        let placeholders = crate::sql_params::placeholders(1, count);
        let sql = format!("SELECT 1 WHERE 1 IN ({placeholders})");
        let result = sqlx::query(&sql).fetch_optional(&mut conn).await;
        assert_eq!(result.is_ok(), count == 32766);
    }
}

#[tokio::test]
async fn sqlite_query_builder_emits_anonymous_placeholders() {
    let mut conn = connection().await;
    let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT ");
    builder.push_bind(11_i64).push(", ").push_bind(22_i64);
    assert_eq!(builder.sql(), "SELECT ?, ?");
    let row: (i64, i64) = builder.build_query_as().fetch_one(&mut conn).await.unwrap();
    assert_eq!(row, (11, 22));
}
