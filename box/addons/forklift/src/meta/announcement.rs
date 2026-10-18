//! The single site-wide announcement row.

use chrono::{DateTime, Utc};
use rusqlite::params;

use super::time::format_time;
use super::{Error, Result, Store, now_rfc3339, parse_time};

/// The single site-wide notice shown on the main page. `body` is Markdown
/// source rendered by the web UI; an empty body means no announcement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announcement {
    pub body: String,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
    /// When the notice stops showing. `None` keeps it up until it is cleared.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether signed-in users may dismiss the notice.
    pub user_dismissible: bool,
}

impl Announcement {
    /// Reports whether the notice should be shown at `now`.
    pub fn active(&self, now: DateTime<Utc>) -> bool {
        !self.body.is_empty() && self.expires_at.is_none_or(|end| now < end)
    }
}

impl Store {
    /// Returns the current announcement. A store that has never had one set
    /// returns [`Error::NotFound`].
    pub async fn get_announcement(&self) -> Result<Announcement> {
        self.read(|conn| {
            conn.query_row(
                "SELECT body, updated_by, updated_at, expires_at, user_dismissible FROM announcement WHERE id = 1",
                [],
                |r| {
                    let updated: String = r.get(2)?;
                    let expires: String = r.get(3)?;
                    Ok(Announcement {
                        body: r.get(0)?,
                        updated_by: r.get(1)?,
                        updated_at: parse_time(&updated),
                        expires_at: (!expires.is_empty()).then(|| parse_time(&expires)),
                        user_dismissible: r.get::<_, i64>(4)? != 0,
                    })
                },
            )
            .map_err(|e| Error::sqlite("get announcement", e))
        })
        .await
    }

    /// Replaces the announcement body, its optional end and whether users may
    /// dismiss it. An empty body clears the notice while keeping who cleared it
    /// and when.
    pub async fn set_announcement(
        &self,
        body: &str,
        updated_by: &str,
        expires_at: Option<DateTime<Utc>>,
        user_dismissible: bool,
    ) -> Result<Announcement> {
        let body = body.to_string();
        let updated_by = updated_by.to_string();
        self.write(move |conn| {
            let now = now_rfc3339();
            let expires = expires_at.map(format_time).unwrap_or_default();
            conn.execute(
                "INSERT INTO announcement(id, body, updated_by, updated_at, expires_at, user_dismissible)
         VALUES(1, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET body = excluded.body,
             updated_by = excluded.updated_by, updated_at = excluded.updated_at,
             expires_at = excluded.expires_at, user_dismissible = excluded.user_dismissible",
                params![body, updated_by, now, expires, i64::from(user_dismissible)],
            )
            .map_err(|e| Error::sqlite("set announcement", e))?;
            Ok(Announcement {
                body,
                updated_by,
                updated_at: parse_time(&now),
                expires_at,
                user_dismissible,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_until_the_end() {
        let now = Utc::now();
        let mut a = Announcement {
            body: "notice".to_string(),
            ..Default::default()
        };
        assert!(a.active(now));
        a.expires_at = Some(now + chrono::TimeDelta::hours(1));
        assert!(a.active(now));
        a.expires_at = Some(now);
        assert!(!a.active(now));
        a.body.clear();
        a.expires_at = None;
        assert!(!a.active(now));
    }
}
