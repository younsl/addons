//! Agent metrics.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::policy::Detection;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DetectionLabels {
    pub engine: String,
    pub rule: String,
    pub severity: String,
    pub action: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindLabels {
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TriggerLabels {
    pub trigger: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ContentLabels {
    pub revision: String,
    pub source: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabels {
    pub result: String,
}

#[derive(Clone)]
pub struct AgentMetrics {
    pub detections: Family<DetectionLabels, Counter>,
    pub events: Family<KindLabels, Counter>,
    pub events_dropped: Counter,
    pub files_scanned: Family<TriggerLabels, Counter>,
    pub full_scan_duration: Histogram,
    pub content_info: Family<ContentLabels, Gauge>,
    pub content_swap: Family<ResultLabels, Counter>,
    pub blocklist_entries: Gauge,
}

impl AgentMetrics {
    pub fn register(reg: &mut Registry) -> Self {
        let m = Self {
            detections: Family::default(),
            events: Family::default(),
            events_dropped: Counter::default(),
            files_scanned: Family::default(),
            full_scan_duration: Histogram::new(exponential_buckets(1.0, 2.0, 14)),
            content_info: Family::default(),
            content_swap: Family::default(),
            blocklist_entries: Gauge::default(),
        };
        reg.register(
            "ward_agent_detections",
            "Detections and the action taken",
            m.detections.clone(),
        );
        reg.register(
            "ward_agent_events",
            "Kernel or polled process events",
            m.events.clone(),
        );
        reg.register(
            "ward_agent_events_dropped",
            "Ring buffer records lost because the agent fell behind",
            m.events_dropped.clone(),
        );
        reg.register(
            "ward_agent_files_scanned",
            "Files scanned on exec or by the full scan",
            m.files_scanned.clone(),
        );
        reg.register(
            "ward_agent_full_scan_duration_seconds",
            "Duration of each full scan",
            m.full_scan_duration.clone(),
        );
        reg.register(
            "ward_agent_content_info",
            "1 for the active database revision and event source mode",
            m.content_info.clone(),
        );
        reg.register(
            "ward_agent_content_swap",
            "Database revision swaps",
            m.content_swap.clone(),
        );
        reg.register(
            "ward_agent_blocklist_entries",
            "Inodes in the BPF LSM blocklist",
            m.blocklist_entries.clone(),
        );
        m
    }

    pub fn detection(&self, d: &Detection) {
        let action = d
            .actions
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join("+");
        self.detections
            .get_or_create(&DetectionLabels {
                engine: d.engine.as_str().into(),
                rule: d.rule.clone(),
                severity: d.severity.as_str().into(),
                action,
            })
            .inc();
    }

    pub fn event(&self, kind: &str) {
        self.events
            .get_or_create(&KindLabels { kind: kind.into() })
            .inc();
    }

    pub fn scanned(&self, trigger: &str) {
        self.files_scanned
            .get_or_create(&TriggerLabels {
                trigger: trigger.into(),
            })
            .inc();
    }

    pub fn content(&self, revision: &str, source: &str) {
        self.content_info.clear();
        self.content_info
            .get_or_create(&ContentLabels {
                revision: revision.into(),
                source: source.into(),
            })
            .set(1);
    }

    pub fn swap(&self, ok: bool) {
        self.content_swap
            .get_or_create(&ResultLabels {
                result: if ok { "ok" } else { "rejected" }.into(),
            })
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Engine, Severity};

    #[test]
    fn registers_and_records() {
        let mut reg = Registry::default();
        let m = AgentMetrics::register(&mut reg);
        let d = Detection::new(
            "n",
            Engine::Behavior,
            "reverse-shell",
            Severity::Critical,
            String::new(),
        );
        m.detection(&d);
        m.event("exec");
        m.scanned("exec");
        m.content("r1", "ebpf");
        m.swap(true);
        m.swap(false);
        m.full_scan_duration.observe(1.5);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &reg).unwrap();
        for s in [
            "ward_agent_detections_total{engine=\"Behavior\",rule=\"reverse-shell\",severity=\"Critical\",action=\"Alert\"} 1",
            "ward_agent_events_total{kind=\"exec\"} 1",
            "ward_agent_content_info{revision=\"r1\",source=\"ebpf\"} 1",
            "ward_agent_content_swap_total{result=\"rejected\"} 1",
        ] {
            assert!(out.contains(s), "{s}\n{out}");
        }
    }
}
