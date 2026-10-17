//! Start check for a fenced standby pair (deploy/postgres/STANDBY.md). With
//! `KOMIKA_NODE_ID` set, the server starts only on a database that is not in
//! recovery and whose `komika.node_id` names this node. The hosted PostgreSQL
//! wrapper sets that value only after its fence check passed (or `promote`
//! moved the fence), so a server never writes to a standby or to a database
//! started for another node.

use std::str::FromStr;

use anyhow::Result;
use sqlx::{postgres::PgConnectOptions, Connection, PgConnection};

pub(crate) async fn check(database_url: &str, node_id: &str) -> Result<()> {
    let opts = PgConnectOptions::from_str(database_url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection argument [redacted]"))?;
    let mut conn = PgConnection::connect_with(&opts).await.map_err(|_| {
        anyhow::anyhow!("node fence check: cannot connect to PostgreSQL [redacted]")
    })?;
    check_on(&mut conn, node_id).await
}

pub(crate) async fn check_on(conn: &mut PgConnection, node_id: &str) -> Result<()> {
    let (in_recovery, reported_node, fence_epoch): (bool, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT pg_is_in_recovery(), current_setting('komika.node_id', true), \
             current_setting('komika.fence_epoch', true)",
        )
        .fetch_one(conn)
        .await
        .map_err(|_| anyhow::anyhow!("node fence check: cannot read PostgreSQL node settings"))?;
    decide(node_id, in_recovery, reported_node.as_deref())?;
    tracing::info!(node_id, ?fence_epoch, "PostgreSQL node fence check passed");
    Ok(())
}

fn decide(node_id: &str, in_recovery: bool, reported_node: Option<&str>) -> Result<()> {
    anyhow::ensure!(
        !in_recovery && reported_node == Some(node_id),
        "node fence check failed: expected primary node {node_id:?}; \
         database reported in_recovery={in_recovery}, komika.node_id={reported_node:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_fence_decision_rejects_recovery() {
        let error = decide("node-a", true, Some("node-a"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected primary node \"node-a\""));
        assert!(error.contains("in_recovery=true"));
    }

    #[test]
    fn node_fence_decision_rejects_missing_or_empty_setting() {
        for reported in [None, Some("")] {
            let error = decide("node-a", false, reported).unwrap_err().to_string();
            assert!(error.contains("expected primary node \"node-a\""));
            assert!(error.contains(&format!("komika.node_id={reported:?}")));
        }
    }

    #[test]
    fn node_fence_decision_requires_exact_node_match() {
        for reported in ["node-b", "NODE-A", "node-a "] {
            let error = decide("node-a", false, Some(reported))
                .unwrap_err()
                .to_string();
            assert!(error.contains("expected primary node \"node-a\""));
            assert!(error.contains(&format!("komika.node_id=Some({reported:?})")));
        }
    }

    #[test]
    fn node_fence_decision_accepts_primary_match() {
        decide("node-a", false, Some("node-a")).unwrap();
    }

    #[tokio::test]
    async fn node_fence_checks_database_settings() {
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let mut conn = pool.read().acquire().await.unwrap();
        sqlx::query("SET komika.node_id = 'node-a'")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("SET komika.fence_epoch = '3'")
            .execute(&mut *conn)
            .await
            .unwrap();
        check_on(&mut conn, "node-a").await.unwrap();
        let error = check_on(&mut conn, "node-b").await.unwrap_err().to_string();
        drop(conn);
        pool.close().await;
        assert!(error.contains("expected primary node \"node-b\""));
        assert!(error.contains("komika.node_id=Some(\"node-a\")"));
    }
}
