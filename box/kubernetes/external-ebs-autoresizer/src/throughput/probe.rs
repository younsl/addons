//! The one-time startup metrics check.

use std::time::{Duration, Instant};

use super::Recommender;
use super::query::NODE_BATCH;

/// The outcome of the startup metrics check. It exists because connectivity
/// alone does not mean the recommender will work: the preflight proves the
/// backend answers, while this proves the configured node label and the
/// generated query actually return this cluster's nodes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    /// The expression that was evaluated, scoped to the first batch of this
    /// cluster's node names. It is the query an operator pastes into Grafana.
    pub query: String,
    pub cluster_nodes: usize,
    /// How many Nodes are old enough to hold enough history and are
    /// therefore queried at all.
    pub eligible_nodes: usize,
    /// How many series the query returned.
    pub series: usize,
    /// How many of those carried the configured node label. A gap between
    /// `series` and `nodes` means the node label is wrong for this cluster.
    pub nodes: usize,
    pub dropped: usize,
    /// How many node exporter disk series the backend holds regardless of
    /// node name, filled in only when the scoped query returned nothing.
    pub backend_series: usize,
    pub latency: Duration,
    /// The busiest node the probe saw, so the startup log carries a value an
    /// operator can sanity-check against a dashboard. Empty when no node
    /// returned a finite value.
    pub max_node: String,
    pub max_peak_mibps: f64,
}

impl Recommender {
    /// Lists the cluster's Nodes and runs the peak query for the first batch
    /// of them, then summarizes the result. It is read-only. Errors are
    /// returned rather than logged so the caller decides how loud to be.
    #[allow(clippy::result_large_err)]
    pub async fn probe(&self) -> Result<Probe, (Probe, String)> {
        let mut p = Probe::default();
        let node_list = self.nodes.list("").await.map_err(|e| (p.clone(), e))?;
        p.cluster_nodes = node_list.len();
        let (mut names, _) = self.split_by_age(&node_list);
        p.eligible_nodes = names.len();
        names.truncate(NODE_BATCH);
        p.query = self.query.peak(&names);
        if names.is_empty() {
            return Ok(p);
        }

        let start = Instant::now();
        let samples = match self.prom.query(&p.query).await {
            Ok(s) => s,
            Err(err) => {
                p.latency = start.elapsed();
                return Err((p, err.message));
            }
        };
        p.latency = start.elapsed();
        p.series = samples.len();
        for s in &samples {
            let Some(name) = s
                .labels
                .get(&self.query.node_label)
                .filter(|n| !n.is_empty())
            else {
                continue;
            };
            p.nodes += 1;
            // NaN and Inf never win the comparison, so the reported peak is
            // always a number an operator can compare against a dashboard.
            if !s.value.is_finite() {
                continue;
            }
            if p.max_node.is_empty() || s.value > p.max_peak_mibps {
                p.max_node.clone_from(name);
                p.max_peak_mibps = s.value;
            }
        }
        p.dropped = p.series - p.nodes;

        // Scoping the query by node name means a wrong node label yields no
        // series at all rather than series that cannot be attributed, so the
        // two failures look identical from here. One cheap instant query
        // separates them.
        if p.series == 0 {
            p.backend_series = self.count_backend_series().await;
        }
        Ok(p)
    }

    /// How many node exporter disk series the backend holds at all, or 0 when
    /// the query fails. A failure here is not worth surfacing: this runs only
    /// to improve a diagnostic that is already being logged.
    async fn count_backend_series(&self) -> usize {
        let Ok(samples) = self.prom.query(&self.query.presence()).await else {
            return 0;
        };
        let value = samples.iter().map(|s| s.value).fold(f64::NAN, f64::max);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        if value.is_finite() && value >= 0.0 {
            value as usize
        } else {
            0
        }
    }
}
