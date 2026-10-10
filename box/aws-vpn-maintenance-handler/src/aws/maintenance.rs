//! Reading and applying pending tunnel endpoint maintenance.

use super::client::{ApiError, Client};
use super::types::{Connection, Maintenance, TunnelStatus};
use super::vpn::to_chrono;

/// The `PendingMaintenance` value AWS reports when a replacement is queued and
/// can be triggered early. The other is `NOT_AVAILABLE`.
const PENDING_MAINTENANCE_AVAILABLE: &str = "AVAILABLE";

impl Client {
    /// Reads one tunnel's pending endpoint maintenance. This is the
    /// authoritative source, so a missed AWS Health notification cannot hide
    /// queued work.
    pub async fn maintenance_status(
        &self,
        connection_id: &str,
        outside_ip: &str,
    ) -> Result<Maintenance, ApiError> {
        let out = self
            .api
            .get_vpn_tunnel_replacement_status(connection_id, outside_ip)
            .await
            .map_err(|e| {
                e.with_context(&format!(
                    "get vpn tunnel replacement status {connection_id}/{outside_ip}"
                ))
            })?;
        let mut m = Maintenance::default();
        if let Some(d) = out.maintenance_details() {
            m.pending = d
                .pending_maintenance()
                .is_some_and(|p| p.eq_ignore_ascii_case(PENDING_MAINTENANCE_AVAILABLE));
            m.auto_applied_after = d.maintenance_auto_applied_after().and_then(to_chrono);
            m.last_applied = d.last_maintenance_applied().and_then(to_chrono);
        }
        Ok(m)
    }

    /// Reads maintenance state for every tunnel. One call per tunnel: the API
    /// has no batch form.
    pub async fn statuses(&self, conn: &Connection) -> Result<Vec<TunnelStatus>, ApiError> {
        let mut out = Vec::with_capacity(conn.tunnels.len());
        for t in &conn.tunnels {
            let maintenance = self.maintenance_status(&conn.id, &t.outside_ip).await?;
            out.push(TunnelStatus {
                tunnel: t.clone(),
                maintenance,
            });
        }
        Ok(out)
    }

    /// Triggers the pending endpoint maintenance for one tunnel.
    ///
    /// Irreversible: there is no API to undo it or restore the old endpoint,
    /// so every safety check belongs before this call. The tunnel goes DOWN for
    /// the duration, so the caller must have confirmed the peer is carrying
    /// traffic. With `dry_run`, AWS validates and changes nothing, returning
    /// [`ApiError::DryRunSucceeded`]. An [`ApiError::Uncertain`] means the
    /// replacement may or may not be under way, and the caller must verify
    /// rather than report that nothing changed.
    pub async fn replace(
        &self,
        connection_id: &str,
        outside_ip: &str,
        dry_run: bool,
    ) -> Result<(), ApiError> {
        self.api
            .replace_vpn_tunnel(connection_id, outside_ip, dry_run)
            .await
            .map_err(|e| {
                e.with_context(&format!("replace vpn tunnel {connection_id}/{outside_ip}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aws_sdk_ec2::types::MaintenanceDetails;

    use super::super::client::fake::*;
    use super::*;

    #[tokio::test]
    async fn reads_pending_state_case_insensitively() {
        let fake = Arc::new(FakeEc2::default());
        *fake.statuses.lock().unwrap() = vec![
            (
                "vpn-1".into(),
                "1.1.1.1".into(),
                Some(pending(1_800_000_000)),
            ),
            (
                "vpn-1".into(),
                "2.2.2.2".into(),
                Some(
                    MaintenanceDetails::builder()
                        .pending_maintenance("not_available")
                        .build(),
                ),
            ),
        ];
        let client = Client::from_parts(Box::new(fake.clone()), None, None);
        let m = client.maintenance_status("vpn-1", "1.1.1.1").await.unwrap();
        assert!(m.pending);
        assert_eq!(m.auto_applied_after.unwrap().timestamp(), 1_800_000_000);
        assert_eq!(m.last_applied.unwrap().timestamp(), 1_600_000_000);
        let m = client.maintenance_status("vpn-1", "2.2.2.2").await.unwrap();
        assert!(!m.pending);
        assert!(m.auto_applied_after.is_none());
        // No details at all is "nothing pending".
        assert_eq!(
            client.maintenance_status("vpn-9", "9.9.9.9").await.unwrap(),
            Maintenance::default()
        );

        let conn = crate::aws::vpn::convert_connection(&vpn_connection("vpn-1", "p", [true, true]));
        let statuses = client.statuses(&conn).await.unwrap();
        assert_eq!(statuses.len(), 2);
        assert!(statuses[0].maintenance.pending);
        assert!(!statuses[1].maintenance.pending);

        *fake.status_error.lock().unwrap() = Some("denied".into());
        let err = client.statuses(&conn).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "get vpn tunnel replacement status vpn-1/1.1.1.1: denied"
        );
    }

    #[tokio::test]
    async fn replace_passes_flags_and_keeps_classification() {
        let fake = Arc::new(FakeEc2::default());
        let client = Client::from_parts(Box::new(fake.clone()), None, None);
        client.replace("vpn-1", "1.1.1.1", true).await.unwrap();
        assert_eq!(
            *fake.replace_calls.lock().unwrap(),
            vec![("vpn-1".to_string(), "1.1.1.1".to_string(), true)]
        );

        *fake.replace_result.lock().unwrap() = Some(ApiError::DryRunSucceeded);
        assert!(matches!(
            client.replace("vpn-1", "1.1.1.1", true).await,
            Err(ApiError::DryRunSucceeded)
        ));

        *fake.replace_result.lock().unwrap() = Some(ApiError::Uncertain("timeout".into()));
        let err = client.replace("vpn-1", "1.1.1.1", false).await.unwrap_err();
        assert!(
            matches!(&err, ApiError::Uncertain(s) if s == "replace vpn tunnel vpn-1/1.1.1.1: timeout"),
            "{err}"
        );

        *fake.replace_result.lock().unwrap() = Some(ApiError::Rejected("InvalidParameter".into()));
        assert!(matches!(
            client.replace("vpn-1", "1.1.1.1", false).await,
            Err(ApiError::Rejected(_))
        ));
    }
}
