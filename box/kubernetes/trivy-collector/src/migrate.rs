//! One-shot migration off a PersistentVolume.
//!
//! The old deployment kept authored state — API tokens and report notes — in
//! the same SQLite file as the reports. Reports are a mirror and need no
//! export: the next scraper start relists them from the clusters that own the
//! CRs. Tokens and notes are hashed or typed by a human and cannot be
//! regenerated, so they move to a Secret and a ConfigMap before the volume goes
//! away.
//!
//! Runs as a Job that mounts the existing PVC read-only. Order matters: export
//! and verify both objects, then upgrade, then confirm hydration and that a
//! known token still authenticates, and only then delete the PVC. Until that
//! last step the PVC is the rollback.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams, PostParams};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use tracing::{info, warn};

use crate::storage::notes::{Note, note_key};
use crate::storage::token_store::{StoredToken, TOKEN_PREFIX_LEN};

pub struct ExportRequest {
    pub db_path: String,
    /// Empty means auto-detect from the projected ServiceAccount.
    pub namespace: String,
    pub notes_configmap: String,
    pub api_tokens_secret: String,
    pub dry_run: bool,
}

/// What the export found and wrote, returned so the Job's logs and the tests
/// see the same numbers.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportSummary {
    pub tokens: usize,
    pub notes: usize,
    pub skipped_tokens: usize,
    pub skipped_notes: usize,
}

pub async fn export_state(request: ExportRequest) -> Result<()> {
    if !std::path::Path::new(&request.db_path).exists() {
        bail!("database not found at {}", request.db_path);
    }

    let pool = open_read_only(&request.db_path).await?;
    let (tokens, skipped_tokens) = read_tokens(&pool).await?;
    let (notes, skipped_notes) = read_notes(&pool).await?;
    pool.close().await;

    let summary = ExportSummary {
        tokens: tokens.len(),
        notes: notes.len(),
        skipped_tokens,
        skipped_notes,
    };

    info!(
        db_path = %request.db_path,
        tokens = summary.tokens,
        notes = summary.notes,
        skipped_tokens = summary.skipped_tokens,
        skipped_notes = summary.skipped_notes,
        dry_run = request.dry_run,
        "Read authored state from the legacy database"
    );

    if request.dry_run {
        info!("Dry run — nothing written");
        return Ok(());
    }

    let (client, detected_namespace) = crate::kube_env::client_and_namespace().await?;
    let namespace = if request.namespace.trim().is_empty() {
        detected_namespace
    } else {
        request.namespace.clone()
    };

    write_tokens(&client, &namespace, &request.api_tokens_secret, &tokens).await?;
    write_notes(&client, &namespace, &request.notes_configmap, &notes).await?;

    info!(
        namespace = %namespace,
        secret = %request.api_tokens_secret,
        configmap = %request.notes_configmap,
        tokens = summary.tokens,
        notes = summary.notes,
        "Authored state exported — verify both objects before upgrading, and \
         keep the PVC until a known token and a known note check out"
    );
    Ok(())
}

/// Open the legacy database without creating or migrating anything: the Job
/// mounts the PVC read-only and must not write to it.
async fn open_read_only(db_path: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path))?
        .read_only(true)
        .create_if_missing(false);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("Failed to open {} read-only", db_path))
}

/// Read `api_tokens` rows into their Secret form, keyed by token prefix.
///
/// A row whose prefix is not the expected `tc_` + 8 hex characters cannot be
/// looked up by the validation path, so it is reported rather than written.
async fn read_tokens(pool: &SqlitePool) -> Result<(BTreeMap<String, StoredToken>, usize)> {
    if !table_exists(pool, "api_tokens").await? {
        warn!("Legacy database has no api_tokens table — nothing to export");
        return Ok((BTreeMap::new(), 0));
    }

    let rows = sqlx::query(
        "SELECT user_sub, name, COALESCE(description, ''), token_hash, token_prefix, \
         created_at, expires_at, last_used_at, COALESCE(groups_json, '[]') \
         FROM api_tokens",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read api_tokens")?;

    let mut out = BTreeMap::new();
    let mut skipped = 0;
    for row in rows {
        let prefix: String = row.get(4);
        if !is_valid_prefix(&prefix) {
            warn!(prefix = %prefix, "Skipping token with an unusable prefix");
            skipped += 1;
            continue;
        }
        let groups_json: String = row.get(8);
        out.insert(
            prefix,
            StoredToken {
                user_sub: row.get(0),
                name: row.get(1),
                description: row.get(2),
                token_hash: row.get(3),
                created_at: row.get(5),
                expires_at: row.get(6),
                last_used_at: row.get(7),
                groups: serde_json::from_str(&groups_json).unwrap_or_default(),
            },
        );
    }
    Ok((out, skipped))
}

/// A Secret data key must be the prefix the validation path looks up.
fn is_valid_prefix(prefix: &str) -> bool {
    prefix.len() == TOKEN_PREFIX_LEN
        && prefix.starts_with("tc_")
        && prefix[3..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Read non-empty `reports.notes` into their ConfigMap form.
async fn read_notes(pool: &SqlitePool) -> Result<(BTreeMap<String, Note>, usize)> {
    if !column_exists(pool, "reports", "notes").await? {
        warn!("Legacy database has no reports.notes column — nothing to export");
        return Ok((BTreeMap::new(), 0));
    }

    let rows = sqlx::query(
        "SELECT cluster, report_type, namespace, name, notes, notes_created_at, notes_updated_at \
         FROM reports WHERE notes IS NOT NULL AND TRIM(notes) != ''",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read report notes")?;

    let mut out = BTreeMap::new();
    let mut skipped = 0;
    for row in rows {
        let note = Note {
            cluster: row.get(0),
            report_type: row.get(1),
            namespace: row.get(2),
            name: row.get(3),
            notes: row.get(4),
            notes_created_at: row.get(5),
            notes_updated_at: row.get(6),
        };
        if note.notes.len() > crate::storage::notes::MAX_NOTE_BYTES {
            warn!(
                cluster = %note.cluster,
                name = %note.name,
                bytes = note.notes.len(),
                "Skipping note larger than the per-note limit"
            );
            skipped += 1;
            continue;
        }
        out.insert(
            note_key(
                &note.cluster,
                &note.report_type,
                &note.namespace,
                &note.name,
            ),
            note,
        );
    }
    Ok((out, skipped))
}

async fn write_tokens(
    client: &kube::Client,
    namespace: &str,
    name: &str,
    tokens: &BTreeMap<String, StoredToken>,
) -> Result<()> {
    let mut string_data = BTreeMap::new();
    for (prefix, token) in tokens {
        string_data.insert(prefix.clone(), serde_json::to_string(token)?);
    }

    let api: Api<Secret> = Api::namespaced(client.clone(), namespace);
    let secret = Secret {
        metadata: object_meta(name, namespace, "trivy-collector-api-tokens"),
        string_data: Some(string_data.clone()),
        ..Default::default()
    };
    upsert(
        &api,
        name,
        &secret,
        serde_json::json!({"stringData": string_data}),
    )
    .await
}

async fn write_notes(
    client: &kube::Client,
    namespace: &str,
    name: &str,
    notes: &BTreeMap<String, Note>,
) -> Result<()> {
    let mut data = BTreeMap::new();
    for (key, note) in notes {
        data.insert(key.clone(), serde_json::to_string(note)?);
    }

    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let cm = ConfigMap {
        metadata: object_meta(name, namespace, "trivy-collector-notes"),
        data: Some(data.clone()),
        ..Default::default()
    };
    upsert(&api, name, &cm, serde_json::json!({"data": data})).await
}

/// Create the object, or merge into it when a previous export already ran.
/// A merge patch is used rather than a replace so a re-run is additive and
/// cannot drop state written since.
async fn upsert<K>(api: &Api<K>, name: &str, object: &K, patch: serde_json::Value) -> Result<()>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + serde::Serialize + std::fmt::Debug,
{
    match api.create(&PostParams::default(), object).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 409 => {
            info!(name = %name, "Object exists — merging exported state into it");
            api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .map(|_| ())
                .context("Failed to patch existing object")
        }
        Err(e) => Err(e).context("Failed to create object"),
    }
}

fn object_meta(name: &str, namespace: &str, component: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(namespace.to_string()),
        labels: Some(crate::storage::managed_labels(component)),
        ..Default::default()
    }
}

async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool> {
    let (exists,): (bool,) =
        sqlx::query_as("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=$1")
            .bind(table)
            .fetch_one(pool)
            .await
            .unwrap_or((false,));
    Ok(exists)
}

async fn column_exists(pool: &SqlitePool, table: &str, column: &str) -> Result<bool> {
    if !table_exists(pool, table).await? {
        return Ok(false);
    }
    let query = format!(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('{}') WHERE name=$1",
        table
    );
    // SAFETY: `table` is a hardcoded literal at every call site, never user input.
    let (exists,): (bool,) = sqlx::query_as(sqlx::AssertSqlSafe(query))
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or((false,));
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recreate enough of the legacy schema to exercise the readers.
    async fn legacy_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(
            r#"
            CREATE TABLE api_tokens (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_sub TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT DEFAULT '',
                token_hash TEXT NOT NULL,
                token_prefix TEXT NOT NULL,
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                last_used_at TEXT,
                groups_json TEXT DEFAULT '[]'
            );
            CREATE TABLE reports (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                cluster TEXT NOT NULL,
                namespace TEXT NOT NULL,
                name TEXT NOT NULL,
                report_type TEXT NOT NULL,
                data TEXT NOT NULL,
                notes TEXT DEFAULT '',
                notes_created_at TEXT,
                notes_updated_at TEXT
            );
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert_token(pool: &SqlitePool, prefix: &str, name: &str, groups: &str) {
        sqlx::query(
            "INSERT INTO api_tokens (user_sub, name, description, token_hash, token_prefix, \
             created_at, expires_at, groups_json) VALUES ('u1', $1, 'd', 'hash', $2, \
             '2026-01-01T00:00:00+00:00', '2099-01-01T00:00:00+00:00', $3)",
        )
        .bind(name)
        .bind(prefix)
        .bind(groups)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_report(pool: &SqlitePool, name: &str, notes: &str) {
        sqlx::query(
            "INSERT INTO reports (cluster, namespace, name, report_type, data, notes, \
             notes_created_at, notes_updated_at) VALUES ('prod', 'default', $1, 'sbomreport', \
             '{}', $2, '2026-01-01T00:00:00+00:00', '2026-01-02T00:00:00+00:00')",
        )
        .bind(name)
        .bind(notes)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn prefix_validation_matches_what_the_lookup_path_expects() {
        assert!(is_valid_prefix("tc_ab12cd34"));
        assert!(!is_valid_prefix("tc_ab12cd3"), "too short");
        assert!(!is_valid_prefix("tc_ab12cd345"), "too long");
        assert!(!is_valid_prefix("xx_ab12cd34"), "wrong scheme");
        assert!(!is_valid_prefix("tc_ab12cd3z"), "not hex");
    }

    #[tokio::test]
    async fn tokens_are_keyed_by_prefix_with_groups_preserved() {
        let pool = legacy_db().await;
        insert_token(&pool, "tc_aaaaaaaa", "ci", r#"["platform","sre"]"#).await;

        let (tokens, skipped) = read_tokens(&pool).await.unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(tokens.len(), 1);

        let token = &tokens["tc_aaaaaaaa"];
        assert_eq!(token.name, "ci");
        assert_eq!(token.user_sub, "u1");
        assert_eq!(token.token_hash, "hash");
        assert_eq!(token.groups, vec!["platform", "sre"]);
    }

    #[tokio::test]
    async fn a_token_with_an_unusable_prefix_is_reported_not_written() {
        let pool = legacy_db().await;
        insert_token(&pool, "tc_aaaaaaaa", "good", "[]").await;
        insert_token(&pool, "legacy", "bad", "[]").await;

        let (tokens, skipped) = read_tokens(&pool).await.unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(skipped, 1);
    }

    #[tokio::test]
    async fn malformed_groups_json_does_not_lose_the_token() {
        let pool = legacy_db().await;
        insert_token(&pool, "tc_aaaaaaaa", "ci", "not json").await;

        let (tokens, _) = read_tokens(&pool).await.unwrap();
        assert!(tokens["tc_aaaaaaaa"].groups.is_empty());
    }

    #[tokio::test]
    async fn only_reports_that_actually_carry_a_note_are_exported() {
        let pool = legacy_db().await;
        insert_report(&pool, "nginx", "look at this").await;
        insert_report(&pool, "redis", "").await;
        insert_report(&pool, "postgres", "   ").await;

        let (notes, skipped) = read_notes(&pool).await.unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(notes.len(), 1);

        let key = note_key("prod", "sbomreport", "default", "nginx");
        assert_eq!(notes[&key].notes, "look at this");
        assert_eq!(
            notes[&key].notes_created_at.as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
    }

    #[tokio::test]
    async fn an_oversized_note_is_reported_not_silently_truncated() {
        let pool = legacy_db().await;
        let huge = "x".repeat(crate::storage::notes::MAX_NOTE_BYTES + 1);
        insert_report(&pool, "nginx", &huge).await;

        let (notes, skipped) = read_notes(&pool).await.unwrap();
        assert!(notes.is_empty());
        assert_eq!(skipped, 1);
    }

    #[tokio::test]
    async fn a_database_without_the_legacy_shape_exports_nothing() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        assert_eq!(read_tokens(&pool).await.unwrap(), (BTreeMap::new(), 0));
        assert_eq!(read_notes(&pool).await.unwrap(), (BTreeMap::new(), 0));
    }

    #[tokio::test]
    async fn a_missing_database_is_an_error_not_an_empty_export() {
        let err = export_state(ExportRequest {
            db_path: "/nonexistent/trivy.db".to_string(),
            namespace: "trivy-system".to_string(),
            notes_configmap: "notes".to_string(),
            api_tokens_secret: "tokens".to_string(),
            dry_run: true,
        })
        .await
        .expect_err("must not succeed");
        assert!(err.to_string().contains("database not found"));
    }

    #[tokio::test]
    async fn column_exists_is_false_when_the_table_is_missing() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        assert!(!column_exists(&pool, "reports", "notes").await.unwrap());
    }
}
