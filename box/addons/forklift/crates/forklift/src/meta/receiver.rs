//! Notification receivers (webhook channels fired when a package is quarantined).

use chrono::{DateTime, Utc};
use rusqlite::{Row, params};

use super::repository::query_all;
use super::{Error, Result, Store, now_rfc3339, parse_time, parse_time_opt};

/// A named notification channel: an alarm (currently a webhook POST) fired
/// when a package is quarantined pending approval. `name` is the unique,
/// human-facing channel identifier; `description` documents its purpose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Receiver {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub webhook_url: String,
    pub enabled: bool,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the webhook last accepted a message. `None` until the first one.
    pub last_sent_at: Option<DateTime<Utc>>,
}

const RECEIVER_COLS: &str =
    "id, name, description, webhook_url, enabled, created_by, created_at, updated_at, last_sent_at";

impl Store {
    /// Inserts a notification receiver. Returns [`Error::Conflict`] when the
    /// name is already taken.
    pub async fn create_receiver(&self, r: Receiver) -> Result<Receiver> {
        self.write(move |conn| {
            let now = now_rfc3339();
            let sql = format!(
                "INSERT INTO notification_receivers(name, description, webhook_url, enabled, created_by, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?, ?, ?)
         RETURNING {RECEIVER_COLS}"
            );
            conn.query_row(
                &sql,
                params![
                    r.name,
                    r.description,
                    r.webhook_url,
                    i64::from(r.enabled),
                    r.created_by,
                    now,
                    now
                ],
                scan_receiver,
            )
            .map_err(|e| {
                if is_unique_violation(&e) {
                    Error::Conflict
                } else {
                    Error::sqlite("create receiver", e)
                }
            })
        })
        .await
    }

    /// Returns one receiver by id.
    pub async fn get_receiver(&self, id: i64) -> Result<Receiver> {
        self.read(move |conn| {
            conn.query_row(
                &format!("SELECT {RECEIVER_COLS} FROM notification_receivers WHERE id = ?"),
                params![id],
                scan_receiver,
            )
            .map_err(|e| Error::sqlite("get receiver", e))
        })
        .await
    }

    /// Returns all receivers, oldest first (stable display order).
    pub async fn list_receivers(&self) -> Result<Vec<Receiver>> {
        self.read(|conn| {
            query_all(
                conn,
                "list receivers",
                &format!("SELECT {RECEIVER_COLS} FROM notification_receivers ORDER BY id ASC"),
                [],
                scan_receiver,
            )
        })
        .await
    }

    /// Returns the enabled receivers (delivery targets).
    pub async fn list_enabled_receivers(&self) -> Result<Vec<Receiver>> {
        self.read(|conn| {
            query_all(
                conn,
                "list receivers",
                &format!("SELECT {RECEIVER_COLS} FROM notification_receivers WHERE enabled = 1 ORDER BY id ASC"),
                [],
                scan_receiver,
            )
        })
        .await
    }

    /// Overwrites a receiver's editable fields. Returns [`Error::NotFound`]
    /// when no row matches and [`Error::Conflict`] when the new name collides.
    pub async fn update_receiver(&self, r: Receiver) -> Result<Receiver> {
        let id = r.id;
        self.write(move |conn| {
            let n = conn
                .execute(
                    "UPDATE notification_receivers
         SET name = ?, description = ?, webhook_url = ?, enabled = ?, updated_at = ?
         WHERE id = ?",
                    params![
                        r.name,
                        r.description,
                        r.webhook_url,
                        i64::from(r.enabled),
                        now_rfc3339(),
                        r.id
                    ],
                )
                .map_err(|e| {
                    if is_unique_violation(&e) {
                        Error::Conflict
                    } else {
                        Error::sqlite("update receiver", e)
                    }
                })?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await?;
        self.get_receiver(id).await
    }

    /// Stamps every receiver posting to `webhook_url` as sent now. Returns how
    /// many rows matched.
    pub async fn mark_receiver_sent(&self, webhook_url: &str) -> Result<usize> {
        let url = webhook_url.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE notification_receivers SET last_sent_at = ? WHERE webhook_url = ?",
                params![now_rfc3339(), url],
            )
            .map_err(|e| Error::sqlite("mark receiver sent", e))
        })
        .await
    }

    /// Removes one receiver.
    pub async fn delete_receiver(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let n = conn
                .execute(
                    "DELETE FROM notification_receivers WHERE id = ?",
                    params![id],
                )
                .map_err(|e| Error::sqlite("delete receiver", e))?;
            if n == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }
}

/// Decodes one row in [`RECEIVER_COLS`] order.
fn scan_receiver(row: &Row<'_>) -> rusqlite::Result<Receiver> {
    let enabled: i64 = row.get(4)?;
    let created: String = row.get(6)?;
    let updated: String = row.get(7)?;
    Ok(Receiver {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        webhook_url: row.get(3)?,
        enabled: enabled != 0,
        created_by: row.get(5)?,
        created_at: parse_time(&created),
        updated_at: parse_time(&updated),
        last_sent_at: parse_time_opt(row.get::<_, Option<String>>(8)?.as_deref()),
    })
}

/// Reports whether `err` is a SQLite UNIQUE (or PRIMARY KEY, which SQLite reports with the same
/// "UNIQUE constraint failed" message) violation.
pub(crate) const fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::meta::test_store;

    #[tokio::test]
    async fn mark_receiver_sent_stamps_matching_url() {
        let (s, _dir) = test_store().await;
        let receiver = |name: &str, url: &str| Receiver {
            name: name.to_string(),
            webhook_url: url.to_string(),
            enabled: true,
            ..Default::default()
        };
        let hit = s
            .create_receiver(receiver("slack", "https://hooks.example.com/a"))
            .await
            .unwrap();
        let miss = s
            .create_receiver(receiver("other", "https://hooks.example.com/b"))
            .await
            .unwrap();
        assert_eq!(hit.last_sent_at, None);

        let n = s
            .mark_receiver_sent("https://hooks.example.com/a")
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(s.get_receiver(hit.id).await.unwrap().last_sent_at.is_some());
        assert_eq!(s.get_receiver(miss.id).await.unwrap().last_sent_at, None);
    }
}
