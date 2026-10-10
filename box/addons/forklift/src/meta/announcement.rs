//! The single site-wide announcement row.

use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{Error, Result, Store, now_rfc3339, parse_time};

/// The single site-wide notice shown on the main page. `body` is Markdown
/// source rendered by the web UI; an empty body means no announcement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announcement {
    pub body: String,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    /// Returns the current announcement. A store that has never had one set
    /// returns [`Error::NotFound`].
    pub async fn get_announcement(&self) -> Result<Announcement> {
        self.read(|conn| {
            conn.query_row(
                "SELECT body, updated_by, updated_at FROM announcement WHERE id = 1",
                [],
                |r| {
                    let updated: String = r.get(2)?;
                    Ok(Announcement {
                        body: r.get(0)?,
                        updated_by: r.get(1)?,
                        updated_at: parse_time(&updated),
                    })
                },
            )
            .map_err(|e| Error::sqlite("get announcement", e))
        })
        .await
    }

    /// Replaces the announcement body. An empty body clears the notice while
    /// keeping who cleared it and when.
    pub async fn set_announcement(&self, body: &str, updated_by: &str) -> Result<Announcement> {
        let body = body.to_string();
        let updated_by = updated_by.to_string();
        self.write(move |conn| {
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO announcement(id, body, updated_by, updated_at) VALUES(1, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET body = excluded.body,
             updated_by = excluded.updated_by, updated_at = excluded.updated_at",
                params![body, updated_by, now],
            )
            .map_err(|e| Error::sqlite("set announcement", e))?;
            Ok(Announcement {
                body,
                updated_by,
                updated_at: parse_time(&now),
            })
        })
        .await
    }
}
