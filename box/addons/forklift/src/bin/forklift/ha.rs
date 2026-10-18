use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use forklift::{api, cluster, config, version};

use crate::bridge::block_on;

/// Names the active high-availability/storage topology for the admin status
/// view.
fn ha_mode(cfg: &config::Config) -> &'static str {
    if cfg.replication.enabled {
        "replication"
    } else if cfg.storage.backend == "s3" {
        "object-storage"
    } else if cfg.ha.enabled {
        "shared-volume"
    } else {
        "single"
    }
}

/// Admin-only HA status for the management console: backend/mode, this pod's
/// identity and role, the current Lease holder, the s3 fencing token, and the
/// process start time (for uptime display).
pub(crate) fn ha_status_fn(
    cfg: Arc<config::Config>,
    elector: Option<Arc<cluster::Elector>>,
    leader_state: Arc<AtomicBool>,
    started_at: DateTime<Utc>,
) -> api::HAStatusFn {
    Arc::new(move || {
        let mut st = api::HAStatus {
            enabled: cfg.ha.enabled,
            mode: ha_mode(&cfg).to_string(),
            backend: cfg.storage.backend.clone(),
            identity: cfg.ha.identity.clone(),
            lease_name: cfg.ha.lease_name.clone(),
            is_leader: leader_state.load(Ordering::SeqCst),
            started_at: started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            version: version::VERSION.to_string(),
            runtime: version::rust().to_string(),
            ..Default::default()
        };
        // Where artifacts live: the object-storage bucket/endpoint (s3) or
        // the block-storage data directory (fs).
        if cfg.storage.backend == "s3" {
            let mut bucket = cfg.storage.s3.bucket.clone();
            if !cfg.storage.s3.prefix.is_empty() {
                bucket.push('/');
                bucket.push_str(cfg.storage.s3.prefix.trim_start_matches('/'));
            }
            let endpoint = cfg.storage.s3.endpoint.trim_end_matches('/');
            st.storage_endpoint = if endpoint.is_empty() {
                format!("s3://{bucket}")
            } else {
                format!("{endpoint}/{bucket}")
            };
        } else {
            st.storage_endpoint.clone_from(&cfg.data_dir);
        }
        let Some(elector) = &elector else {
            // Single instance is always the leader and serves itself.
            st.is_leader = true;
            st.leader.clone_from(&cfg.ha.identity);
            st.role = cluster::ROLE_LEADER.to_string();
            return st;
        };
        st.role = if st.is_leader {
            cluster::ROLE_LEADER.to_string()
        } else {
            cluster::ROLE_STANDBY.to_string()
        };
        let elector = Arc::clone(elector);
        let s3 = cfg.storage.backend == "s3";
        let (leader, fence) = block_on(async move {
            let leader = elector.leader_identity().await.ok();
            let fence = if s3 {
                elector.fencing_token().await.ok()
            } else {
                None
            };
            (leader, fence)
        });
        if let Some(leader) = leader {
            st.leader = leader;
        }
        if let Some(fence) = fence {
            st.fencing_token = fence;
        }
        st
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ha_mode_names_each_topology() {
        let mut cfg = config::Config::default();
        assert_eq!(ha_mode(&cfg), "single");
        cfg.ha.enabled = true;
        assert_eq!(ha_mode(&cfg), "shared-volume");
        cfg.storage.backend = "s3".into();
        assert_eq!(ha_mode(&cfg), "object-storage");
        cfg.replication.enabled = true;
        assert_eq!(ha_mode(&cfg), "replication");
    }
}
