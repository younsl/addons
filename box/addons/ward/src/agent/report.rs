//! Where detections go: a JSON line on stdout, a Kubernetes Event on the
//! Pod, the manager when it is enabled, and the counters the agent Lease
//! carries.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use crate::k8s::Events;
use crate::policy::Detection;

use super::client::ManagerClient;
use super::metrics::AgentMetrics;

const EVENT_DEDUP: Duration = Duration::from_mins(5);
const MANAGER_BUFFER: usize = 5_000;
const MANAGER_BATCH: usize = 200;

/// Detection counters the Lease publishes.
#[derive(Default)]
pub struct Counters {
    total: Mutex<u64>,
    by_content: Mutex<BTreeMap<String, u64>>,
}

impl Counters {
    pub fn record(&self, content: &str) {
        if let Ok(mut t) = self.total.lock() {
            *t += 1;
        }
        if let Ok(mut m) = self.by_content.lock() {
            *m.entry(content.to_string()).or_default() += 1;
        }
    }

    pub fn total(&self) -> u64 {
        self.total.lock().map_or(0, |t| *t)
    }

    pub fn by_content(&self) -> BTreeMap<String, u64> {
        self.by_content
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default()
    }
}

/// The stdout line of a detection: one JSON object with a top-level
/// `detection` field.
pub fn log_line(d: &Detection) -> String {
    serde_json::json!({
        "timestamp": d.time.to_rfc3339(),
        "level": "WARN",
        "message": "threat detected",
        "detection": d,
    })
    .to_string()
}

pub struct Sinks {
    pub events: Option<Events>,
    pub manager: Option<Arc<ManagerClient>>,
    pub metrics: AgentMetrics,
    pub counters: Arc<Counters>,
}

/// Drains `rx` until it closes or `shutdown` fires.
pub async fn run(
    mut rx: mpsc::Receiver<(Detection, String)>,
    sinks: Sinks,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut seen: HashMap<(String, String), Instant> = HashMap::new();
    let mut pending: VecDeque<Detection> = VecDeque::new();
    let mut flush = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            item = rx.recv() => {
                let Some((d, content)) = item else { break };
                println!("{}", log_line(&d));
                sinks.metrics.detection(&d);
                sinks.counters.record(&content);
                if let (Some(events), Some(w)) = (&sinks.events, &d.workload) {
                    let key = (format!("{}/{}", w.namespace, w.pod), d.rule.clone());
                    let fresh = seen.get(&key).is_none_or(|t| t.elapsed() > EVENT_DEDUP);
                    if fresh {
                        seen.insert(key, Instant::now());
                        if let Err(e) = events.publish(&d).await {
                            tracing::warn!(error = %e, "publish detection event");
                        }
                    }
                    if seen.len() > 10_000 {
                        seen.retain(|_, t| t.elapsed() <= EVENT_DEDUP);
                    }
                }
                if sinks.manager.is_some() {
                    if pending.len() >= MANAGER_BUFFER {
                        pending.pop_front();
                    }
                    pending.push_back(d);
                }
            }
            _ = flush.tick() => {
                if let Some(m) = &sinks.manager {
                    while !pending.is_empty() {
                        let n = pending.len().min(MANAGER_BATCH);
                        let batch: Vec<Detection> = pending.iter().take(n).cloned().collect();
                        match m.post_detections(&batch).await {
                            Ok(()) => { pending.drain(..n); }
                            Err(e) => {
                                tracing::debug!(error = %e, pending = pending.len(), "manager unreachable, keeping detections");
                                break;
                            }
                        }
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::registry::Registry;

    use super::*;
    use crate::policy::{Engine, Severity};

    #[test]
    fn counters_and_log_lines() {
        let c = Counters::default();
        c.record("sha256:a");
        c.record("sha256:a");
        c.record("builtin");
        assert_eq!(c.total(), 3);
        assert_eq!(c.by_content()["sha256:a"], 2);
        let d = Detection::new(
            "n",
            Engine::Signature,
            "Ward.Test.EICAR",
            Severity::High,
            "x".into(),
        );
        let v: serde_json::Value = serde_json::from_str(&log_line(&d)).unwrap();
        assert_eq!(v["detection"]["rule"], "Ward.Test.EICAR");
        assert_eq!(v["level"], "WARN");
    }

    #[tokio::test]
    async fn reporter_counts_and_stops() {
        let (tx, rx) = mpsc::channel(8);
        let (stop_tx, stop_rx) = watch::channel(false);
        let counters = Arc::new(Counters::default());
        let sinks = Sinks {
            events: None,
            manager: None,
            metrics: AgentMetrics::register(&mut Registry::default()),
            counters: counters.clone(),
        };
        let task = tokio::spawn(run(rx, sinks, stop_rx));
        let d = Detection::new("n", Engine::Behavior, "r", Severity::Low, String::new());
        tx.send((d, "builtin".into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop_tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(counters.total(), 1);
    }
}
