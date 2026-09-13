use std::sync::Arc;

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;
use serde::Serialize;

use super::{Handler, StatusMessageDTO, write_error, write_json};

/// The high-availability snapshot shown in the admin management console: the
/// active storage/HA topology, this pod's identity and role, the current Lease
/// holder, and (in s3 mode) the fencing token.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HAStatus {
    /// Whether leader election is active (HA mode).
    pub enabled: bool,
    /// The topology: single, shared-volume, replication, or object-storage.
    pub mode: String,
    /// The storage backend: fs or s3.
    pub backend: String,
    /// The address artifacts live at: the object-storage bucket/endpoint (s3) or
    /// the block-storage data directory (fs).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub storage_endpoint: String,
    /// This pod's leader-election identity (its pod name).
    pub identity: String,
    /// The current Lease holder's identity (`""` if none/unknown).
    pub leader: String,
    /// Whether this pod currently holds leadership.
    pub is_leader: bool,
    /// `leader` or `standby`.
    pub role: String,
    /// The leader-election Lease object name (HA mode).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub lease_name: String,
    /// The Lease transition count guarding s3 metadata writes.
    #[serde(skip_serializing_if = "is_zero")]
    pub fencing_token: i64,
    /// This process's start time (RFC3339), for uptime display.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub started_at: String,
    /// This pod's forklift version, shown on the architecture diagram.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// The Rust toolchain this pod's binary was built with, e.g. "rustc 1.98.1".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub runtime: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

impl Handler {
    /// Injects the provider that assembles live HA status. Wired from `main`
    /// once leader election and the storage backend are known.
    pub fn set_ha_status(&self, f: super::HAStatusFn) {
        self.injected.write().ha_status = Some(f);
    }

    /// Injects the manual-failover trigger: it asks this instance to release
    /// leadership, reporting whether it was the leader. Wired from `main` only
    /// in HA mode; left unset for single-instance, where there is nothing to
    /// fail over.
    pub fn set_ha_step_down(&self, f: super::HAStepDownFn) {
        self.injected.write().ha_step_down = Some(f);
    }
}

/// Reports HA/leadership status. Registered under the admin-only route group.
/// When no provider is wired (e.g. tests) it reports a single-instance leader.
pub(super) async fn get_status(State(h): State<Arc<Handler>>) -> Response {
    let status = h.injected.read().ha_status.clone();
    match status {
        Some(f) => write_json(StatusCode::OK, f()),
        None => write_json(
            StatusCode::OK,
            HAStatus {
                mode: "single".to_string(),
                backend: "fs".to_string(),
                is_leader: true,
                role: "leader".to_string(),
                ..Default::default()
            },
        ),
    }
}

/// Triggers a manual failover: the current leader releases its Lease so a
/// standby takes over. Admin-only. Admin traffic is routed to the leader, so the
/// request normally lands there; if it lands on a standby (or HA is off) there is
/// no leadership to release and it reports 409.
pub(super) async fn step_down(State(h): State<Arc<Handler>>) -> Response {
    let step_down = h.injected.read().ha_step_down.clone();
    let Some(step_down) = step_down else {
        return write_error(
            StatusCode::CONFLICT,
            "manual failover is unavailable in single-instance mode",
        );
    };
    if !step_down() {
        return write_error(
            StatusCode::CONFLICT,
            "this instance is not the leader; nothing to step down",
        );
    }
    write_json(
        StatusCode::OK,
        StatusMessageDTO {
            status: "stepping down".to_string(),
        },
    )
}
