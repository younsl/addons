//! Database schema initialization
//!
//! The database now lives on an `emptyDir` owned by the scraper and starts
//! empty on every restart, so there is nothing to migrate: the watcher's
//! initial list rebuilds `reports` from the API servers that own the CRs.
//! Authored state (notes, API tokens) lives in a ConfigMap and a Secret and
//! never touches this schema.

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing::debug;

/// Initialize the database schema
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    debug!("Initializing database schema");

    sqlx::raw_sql(
        r#"
        -- Reports table. Every row is a projection of a VulnerabilityReport or
        -- SbomReport CR that exists right now; the API server is the record.
        CREATE TABLE IF NOT EXISTS reports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            cluster TEXT NOT NULL,
            namespace TEXT NOT NULL,
            name TEXT NOT NULL,
            report_type TEXT NOT NULL,
            app TEXT DEFAULT '',
            image TEXT DEFAULT '',
            registry TEXT DEFAULT '',
            critical_count INTEGER DEFAULT 0,
            high_count INTEGER DEFAULT 0,
            medium_count INTEGER DEFAULT 0,
            low_count INTEGER DEFAULT 0,
            unknown_count INTEGER DEFAULT 0,
            components_count INTEGER DEFAULT 0,
            data TEXT NOT NULL,
            received_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(cluster, namespace, name, report_type)
        );

        -- Indexes for common queries
        CREATE INDEX IF NOT EXISTS idx_reports_cluster ON reports(cluster);
        CREATE INDEX IF NOT EXISTS idx_reports_namespace ON reports(namespace);
        CREATE INDEX IF NOT EXISTS idx_reports_report_type ON reports(report_type);
        CREATE INDEX IF NOT EXISTS idx_reports_app ON reports(app);
        CREATE INDEX IF NOT EXISTS idx_reports_severity ON reports(critical_count, high_count);
        CREATE INDEX IF NOT EXISTS idx_reports_received_at ON reports(received_at);
        -- Composite index that serves the clusters_view aggregation
        -- (GROUP BY cluster with SUM per report_type and MAX(updated_at)).
        -- On a ~300 MB DB a scan-based aggregation can take tens of seconds;
        -- this index lets SQLite answer the whole view from the index alone.
        CREATE INDEX IF NOT EXISTS idx_reports_cluster_type_updated
            ON reports(cluster, report_type, updated_at);
        -- Serves "list newest reports of a given type" (ReportsPage):
        --   SELECT ... WHERE report_type = ? ORDER BY updated_at DESC LIMIT ?
        -- ORDER BY DESC is covered — SQLite walks the index in reverse.
        CREATE INDEX IF NOT EXISTS idx_reports_type_updated
            ON reports(report_type, updated_at);

        -- Clusters view for quick cluster listing
        CREATE VIEW IF NOT EXISTS clusters_view AS
        SELECT
            cluster,
            SUM(CASE WHEN report_type = 'vulnerabilityreport' THEN 1 ELSE 0 END) as vuln_count,
            SUM(CASE WHEN report_type = 'sbomreport' THEN 1 ELSE 0 END) as sbom_count,
            MAX(updated_at) as last_seen
        FROM reports
        GROUP BY cluster;
        "#,
    )
    .execute(pool)
    .await
    .context("Failed to initialize database schema")?;

    let (index_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND tbl_name='reports'",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0,));

    debug!(
        table = "reports",
        indexes = index_count,
        view = "clusters_view",
        "Database schema initialized"
    );

    Ok(())
}

/// Check if a table exists in the database
#[cfg(test)]
async fn table_exists_check(pool: &SqlitePool, table_name: &str) -> Result<bool> {
    let (exists,): (bool,) =
        sqlx::query_as("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=$1")
            .bind(table_name)
            .fetch_one(pool)
            .await
            .unwrap_or((false,));
    Ok(exists)
}

/// Check if a column exists in the given table
#[cfg(test)]
async fn column_exists(pool: &SqlitePool, table_name: &str, column_name: &str) -> Result<bool> {
    let query = format!(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('{}') WHERE name=$1",
        table_name
    );
    // SAFETY: `table_name` is a hardcoded literal at every call site, never user input.
    let (exists,): (bool,) = sqlx::query_as(sqlx::AssertSqlSafe(query))
        .bind(column_name)
        .fetch_one(pool)
        .await
        .unwrap_or((false,));
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_init_schema_fresh_db() {
        let pool = test_pool().await;
        init_schema(&pool).await.unwrap();

        assert!(table_exists_check(&pool, "reports").await.unwrap());
    }

    #[tokio::test]
    async fn test_init_schema_drops_legacy_tables() {
        let pool = test_pool().await;
        init_schema(&pool).await.unwrap();

        // Authored/observational state moved out of SQLite entirely.
        assert!(!table_exists_check(&pool, "api_tokens").await.unwrap());
        assert!(!table_exists_check(&pool, "api_logs").await.unwrap());
        assert!(!table_exists_check(&pool, "cleanup_history").await.unwrap());
        assert!(!column_exists(&pool, "reports", "notes").await.unwrap());
    }

    #[tokio::test]
    async fn test_init_schema_idempotent() {
        let pool = test_pool().await;
        init_schema(&pool).await.unwrap();
        // Running again should not fail
        init_schema(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn test_table_exists_check() {
        let pool = test_pool().await;
        assert!(!table_exists_check(&pool, "reports").await.unwrap());
        init_schema(&pool).await.unwrap();
        assert!(table_exists_check(&pool, "reports").await.unwrap());
        assert!(!table_exists_check(&pool, "nonexistent").await.unwrap());
    }

    #[tokio::test]
    async fn test_column_exists() {
        let pool = test_pool().await;
        init_schema(&pool).await.unwrap();
        assert!(column_exists(&pool, "reports", "data").await.unwrap());
        assert!(
            !column_exists(&pool, "reports", "nonexistent_col")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_indexes_created() {
        let pool = test_pool().await;
        init_schema(&pool).await.unwrap();

        let (index_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type='index'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // Should have at least the report indexes
        assert!(index_count > 0);
    }
}
