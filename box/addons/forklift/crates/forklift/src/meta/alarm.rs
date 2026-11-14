//! Alarm delivery log: one row per attempt to send the Security or Coverage
//! alarm, so each settings page can chart how its alarms went out.

use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::params;

use super::repository::query_all;
use super::time::{format_time, parse_time};
use super::{Error, Result, Store};

/// The Security insights alarm.
pub const ALARM_INSIGHTS: &str = "insights";
/// The Coverage report alarm.
pub const ALARM_COVERAGE: &str = "coverage";

/// Sent by the alarm's cron schedule.
pub const ALARM_TRIGGER_SCHEDULE: &str = "schedule";
/// Sent from the settings page by an administrator.
pub const ALARM_TRIGGER_MANUAL: &str = "manual";

/// How long delivery rows are kept.
pub const ALARM_HISTORY: TimeDelta = TimeDelta::days(90);

/// One delivery attempt to one receiver. `items` is how many findings or
/// projects the message listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlarmDelivery {
    pub feature: String,
    pub sent_at: DateTime<Utc>,
    pub receiver: String,
    pub trigger: String,
    pub ok: bool,
    pub error: String,
    pub items: i64,
}

impl Store {
    /// Records one delivery attempt and drops rows older than `keep_since`.
    pub async fn record_alarm_delivery(
        &self,
        d: AlarmDelivery,
        keep_since: DateTime<Utc>,
    ) -> Result<()> {
        let keep_since = format_time(keep_since);
        self.write(move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| Error::sqlite("record alarm delivery", e))?;
            tx.execute(
                "INSERT INTO alarm_deliveries(feature, sent_at, receiver, trigger, ok, error, items)
                 VALUES(?, ?, ?, ?, ?, ?, ?)",
                params![
                    d.feature,
                    format_time(d.sent_at),
                    d.receiver,
                    d.trigger,
                    i64::from(d.ok),
                    d.error,
                    d.items
                ],
            )
            .map_err(|e| Error::sqlite("record alarm delivery", e))?;
            tx.execute(
                "DELETE FROM alarm_deliveries WHERE sent_at < ?",
                params![keep_since],
            )
            .map_err(|e| Error::sqlite("prune alarm deliveries", e))?;
            tx.commit()
                .map_err(|e| Error::sqlite("record alarm delivery", e))
        })
        .await
    }

    /// Records an attempt made just now. A failed write is logged rather than
    /// returned, since the alarm itself has already gone out or failed.
    pub async fn log_alarm_delivery(
        &self,
        feature: &str,
        receiver: &str,
        trigger: &str,
        outcome: std::result::Result<(), &str>,
        items: i64,
    ) {
        let now = Utc::now();
        let d = AlarmDelivery {
            feature: feature.to_string(),
            sent_at: now,
            receiver: receiver.to_string(),
            trigger: trigger.to_string(),
            ok: outcome.is_ok(),
            error: outcome.err().unwrap_or_default().to_string(),
            items,
        };
        if let Err(e) = self.record_alarm_delivery(d, now - ALARM_HISTORY).await {
            tracing::warn!(feature, err = %e, "alarm: recording the delivery failed");
        }
    }

    /// Returns one alarm's deliveries since `since`, oldest first.
    pub async fn alarm_deliveries(
        &self,
        feature: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<AlarmDelivery>> {
        let (feature, since) = (feature.to_string(), format_time(since));
        self.read(move |conn| {
            query_all(
                conn,
                "alarm deliveries",
                "SELECT feature, sent_at, receiver, trigger, ok, error, items FROM alarm_deliveries
                  WHERE feature = ? AND sent_at >= ? ORDER BY sent_at ASC, id ASC",
                params![feature, since],
                |r| {
                    Ok(AlarmDelivery {
                        feature: r.get(0)?,
                        sent_at: parse_time(&r.get::<_, String>(1)?),
                        receiver: r.get(2)?,
                        trigger: r.get(3)?,
                        ok: r.get::<_, i64>(4)? != 0,
                        error: r.get(5)?,
                        items: r.get(6)?,
                    })
                },
            )
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::testing::meta::test_store;

    #[tokio::test]
    async fn alarm_deliveries_by_feature_and_window() {
        let (s, _dir) = test_store().await;
        let now = Utc::now();
        let delivery = |feature: &str, ago: i64, ok: bool| AlarmDelivery {
            feature: feature.to_string(),
            sent_at: now - TimeDelta::days(ago),
            receiver: "slack-security".to_string(),
            trigger: ALARM_TRIGGER_SCHEDULE.to_string(),
            ok,
            error: if ok {
                String::new()
            } else {
                "timeout".to_string()
            },
            items: 3,
        };
        let keep = now - TimeDelta::days(90);
        for d in [
            delivery(ALARM_INSIGHTS, 40, true),
            delivery(ALARM_INSIGHTS, 2, false),
            delivery(ALARM_COVERAGE, 1, true),
        ] {
            s.record_alarm_delivery(d, keep).await.unwrap();
        }

        let insights = s
            .alarm_deliveries(ALARM_INSIGHTS, now - TimeDelta::days(30))
            .await
            .unwrap();
        assert_eq!(insights.len(), 1);
        assert!(!insights[0].ok);
        assert_eq!(insights[0].error, "timeout");
        assert_eq!(insights[0].items, 3);

        let all = s
            .alarm_deliveries(ALARM_INSIGHTS, now - TimeDelta::days(90))
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].sent_at < all[1].sent_at);

        s.record_alarm_delivery(delivery(ALARM_COVERAGE, 0, true), now - TimeDelta::days(10))
            .await
            .unwrap();
        let pruned = s
            .alarm_deliveries(ALARM_INSIGHTS, now - TimeDelta::days(90))
            .await
            .unwrap();
        assert_eq!(pruned.len(), 1, "rows older than keep_since are dropped");
    }
}
