//! Signature database rollout. The operator owns the state and writes it to
//! the rollout `ConfigMap`. Agents read it to pick the revision for their
//! node. A new revision runs on the canary ring for the soak period and is
//! promoted, or halted when canary agents fail to load it or detect far more
//! than the broad ring over the same span.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::crd::{RolloutSpec, parse_duration};
use crate::policy::revision_of;

pub const ROLLOUT_FILE: &str = "rollout.json";
pub const RING_LABEL: &str = "ward.sh/ring";
pub const RING_CANARY: &str = "canary";
/// Canary agents that have not converged after this many soak periods halt
/// the rollout.
const CONVERGE_SOAKS: i32 = 3;

/// One database revision, as pulled from the registry.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContentRef {
    pub revision: String,
    /// Digest of the database layer.
    pub digest: String,
    /// Digest of the signature layer. Empty when unsigned.
    #[serde(default)]
    pub signature_digest: String,
    /// `registry/repository`, for agents that pull the blobs themselves.
    pub repository: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Canary,
    Halted,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Canary => "Canary",
            Self::Halted => "Halted",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Rollout {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ContentRef>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canary_nodes: Vec<String>,
    /// Detections each broad agent had under `stable` when the canary began.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub baseline: BTreeMap<String, u64>,
    /// Digests that halted and are never retried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub halted: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What an agent last reported through its Lease.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentReport {
    pub node: String,
    pub content_digest: Option<String>,
    pub content_error: Option<String>,
    pub detections_by_content: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    None,
    Promoted(String),
    Halted(String),
}

impl Rollout {
    pub fn parse(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The revision `node` should run.
    pub fn assigned(&self, node: &str) -> Option<&ContentRef> {
        match &self.target {
            Some(t) if self.canary_nodes.iter().any(|n| n == node) => Some(t),
            _ => self.stable.as_ref(),
        }
    }

    /// Whether `digest` is already stable, rolling out, or refused.
    pub fn knows(&self, digest: &str) -> bool {
        self.stable.as_ref().is_some_and(|c| c.digest == digest)
            || self.target.as_ref().is_some_and(|c| c.digest == digest)
            || self.halted.iter().any(|d| d == digest)
    }

    /// Starts rolling out `candidate`. Without canary nodes it is promoted
    /// at once.
    pub fn begin(
        &mut self,
        candidate: ContentRef,
        canary_nodes: Vec<String>,
        agents: &[AgentReport],
        now: DateTime<Utc>,
    ) -> Decision {
        if canary_nodes.is_empty() {
            let revision = candidate.revision.clone();
            self.promote_to(candidate);
            self.message = Some(format!("promoted {revision} without a canary ring"));
            return Decision::Promoted(revision);
        }
        let stable = self.stable.as_ref().map(|c| c.digest.clone());
        self.baseline = agents
            .iter()
            .filter(|a| !canary_nodes.contains(&a.node))
            .map(|a| {
                let n = stable
                    .as_ref()
                    .and_then(|d| a.detections_by_content.get(d))
                    .copied()
                    .unwrap_or(0);
                (a.node.clone(), n)
            })
            .collect();
        self.message = Some(format!(
            "canary {} on {} nodes",
            candidate.revision,
            canary_nodes.len()
        ));
        self.target = Some(candidate);
        self.canary_nodes = canary_nodes;
        self.phase = Phase::Canary;
        self.started_at = Some(now);
        Decision::None
    }

    fn promote_to(&mut self, c: ContentRef) {
        self.stable = Some(c);
        self.target = None;
        self.phase = Phase::Idle;
        self.started_at = None;
        self.canary_nodes.clear();
        self.baseline.clear();
    }

    fn halt(&mut self, reason: String) -> Decision {
        if let Some(t) = self.target.take() {
            self.halted.push(t.digest);
        }
        self.phase = Phase::Halted;
        self.started_at = None;
        self.canary_nodes.clear();
        self.baseline.clear();
        self.message = Some(reason.clone());
        Decision::Halted(reason)
    }

    /// Promotes or halts a running canary.
    pub fn evaluate(
        &mut self,
        agents: &[AgentReport],
        spec: &RolloutSpec,
        now: DateTime<Utc>,
    ) -> Decision {
        let (Phase::Canary, Some(target), Some(started)) =
            (self.phase, self.target.clone(), self.started_at)
        else {
            return Decision::None;
        };
        let canary: Vec<&AgentReport> = agents
            .iter()
            .filter(|a| self.canary_nodes.contains(&a.node))
            .collect();
        if let Some(a) = canary.iter().find(|a| {
            a.content_error.is_some() && a.content_digest.as_deref() != Some(&target.digest)
        }) {
            return self.halt(format!(
                "{} failed to load {}: {}",
                a.node,
                target.revision,
                a.content_error.clone().unwrap_or_default()
            ));
        }

        let canary_count: u64 = canary
            .iter()
            .map(|a| {
                a.detections_by_content
                    .get(&target.digest)
                    .copied()
                    .unwrap_or(0)
            })
            .sum();
        let stable = self.stable.as_ref().map(|c| c.digest.as_str());
        let broad: Vec<&AgentReport> = agents
            .iter()
            .filter(|a| !self.canary_nodes.contains(&a.node))
            .collect();
        let broad_delta: u64 = broad
            .iter()
            .map(|a| {
                let now = stable
                    .and_then(|d| a.detections_by_content.get(d))
                    .copied()
                    .unwrap_or(0);
                now.saturating_sub(self.baseline.get(&a.node).copied().unwrap_or(now))
            })
            .sum();
        if canary_count >= u64::from(spec.min_detections) && !canary.is_empty() {
            #[allow(clippy::cast_precision_loss)]
            let canary_rate = canary_count as f64 / canary.len() as f64;
            #[allow(clippy::cast_precision_loss)]
            let broad_rate = if broad.is_empty() {
                0.0
            } else {
                broad_delta as f64 / broad.len() as f64
            };
            if canary_rate > f64::from(spec.max_detection_ratio) * broad_rate {
                return self.halt(format!(
                    "detection storm on canary: {canary_count} detections under {}, {broad_delta} on the broad ring",
                    target.revision
                ));
            }
        }

        let soak = parse_duration(&spec.soak).unwrap_or(std::time::Duration::from_hours(1));
        let soak = chrono::Duration::from_std(soak).unwrap_or(chrono::Duration::hours(1));
        if now - started < soak {
            return Decision::None;
        }
        let converged = canary
            .iter()
            .all(|a| a.content_digest.as_deref() == Some(&target.digest));
        if !converged || canary.is_empty() {
            if now - started >= soak * CONVERGE_SOAKS {
                return self.halt(format!(
                    "canary agents did not load {} within {} soak periods",
                    target.revision, CONVERGE_SOAKS
                ));
            }
            return Decision::None;
        }
        let revision = target.revision.clone();
        self.promote_to(target);
        self.message = Some(format!("promoted {revision}"));
        Decision::Promoted(revision)
    }
}

/// The canary ring: nodes labeled `ward.sh/ring=canary`, plus the first
/// `percent` of the rest ordered by a hash of the node name, at least one
/// when `percent` is above zero.
pub fn choose_canary<'a>(
    nodes: impl IntoIterator<Item = (&'a str, bool)>,
    percent: u32,
) -> Vec<String> {
    let mut labeled = BTreeSet::new();
    let mut rest = Vec::new();
    for (name, is_canary) in nodes {
        if is_canary {
            labeled.insert(name.to_string());
        } else {
            rest.push(name.to_string());
        }
    }
    let total = labeled.len() + rest.len();
    let want = (total * percent as usize).div_ceil(100);
    rest.sort_by_key(|n| revision_of(&[n.as_bytes()]));
    let extra = want.saturating_sub(labeled.len());
    labeled.extend(rest.into_iter().take(extra));
    labeled.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(rev: &str) -> ContentRef {
        ContentRef {
            revision: rev.into(),
            digest: format!("sha256:{rev}"),
            signature_digest: String::new(),
            repository: "ghcr.io/x/ward-db".into(),
            size: 1,
        }
    }

    fn agent(node: &str, digest: Option<&str>, counts: &[(&str, u64)]) -> AgentReport {
        AgentReport {
            node: node.into(),
            content_digest: digest.map(String::from),
            content_error: None,
            detections_by_content: counts.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
        }
    }

    fn spec() -> RolloutSpec {
        RolloutSpec {
            canary_percent: 10,
            soak: "1h".into(),
            max_detection_ratio: 5,
            min_detections: 10,
        }
    }

    #[test]
    fn canary_promotes_after_soak() {
        let t0 = Utc::now();
        let mut r = Rollout {
            stable: Some(c("a")),
            ..Rollout::default()
        };
        let agents = vec![
            agent("n1", Some("sha256:a"), &[("sha256:a", 3)]),
            agent("n2", Some("sha256:a"), &[("sha256:a", 4)]),
        ];
        assert_eq!(
            r.begin(c("b"), vec!["n1".into()], &agents, t0),
            Decision::None
        );
        assert_eq!(r.phase, Phase::Canary);
        assert_eq!(r.baseline["n2"], 4);
        assert_eq!(r.assigned("n1").unwrap().revision, "b");
        assert_eq!(r.assigned("n2").unwrap().revision, "a");
        assert!(r.knows("sha256:b") && r.knows("sha256:a") && !r.knows("sha256:c"));

        let converged = vec![
            agent("n1", Some("sha256:b"), &[("sha256:b", 1)]),
            agent("n2", Some("sha256:a"), &[("sha256:a", 5)]),
        ];
        assert_eq!(r.evaluate(&converged, &spec(), t0), Decision::None);
        let later = t0 + chrono::Duration::minutes(61);
        assert_eq!(
            r.evaluate(&converged, &spec(), later),
            Decision::Promoted("b".into())
        );
        assert_eq!(r.stable.as_ref().unwrap().revision, "b");
        assert_eq!(r.phase, Phase::Idle);
        assert!(r.canary_nodes.is_empty());
        assert_eq!(r.evaluate(&converged, &spec(), later), Decision::None);

        let back = Rollout::parse(&r.to_json());
        assert_eq!(back, r);
        assert_eq!(Rollout::parse("junk"), Rollout::default());
    }

    #[test]
    fn detection_storm_halts() {
        let t0 = Utc::now();
        let mut r = Rollout {
            stable: Some(c("a")),
            ..Rollout::default()
        };
        let base = vec![
            agent("n1", None, &[]),
            agent("n2", None, &[("sha256:a", 2)]),
        ];
        r.begin(c("b"), vec!["n1".into()], &base, t0);
        let storm = vec![
            agent("n1", Some("sha256:b"), &[("sha256:b", 50)]),
            agent("n2", Some("sha256:a"), &[("sha256:a", 3)]),
        ];
        let d = r.evaluate(&storm, &spec(), t0);
        assert!(
            matches!(d, Decision::Halted(ref m) if m.contains("storm")),
            "{d:?}"
        );
        assert_eq!(r.phase, Phase::Halted);
        assert!(r.knows("sha256:b"));
        assert_eq!(r.assigned("n1").unwrap().revision, "a");
        assert_eq!(Phase::Halted.as_str(), "Halted");
    }

    #[test]
    fn load_failure_and_no_convergence_halt() {
        let t0 = Utc::now();
        let mut r = Rollout::default();
        r.begin(c("b"), vec!["n1".into()], &[], t0);
        let mut failed = agent("n1", None, &[]);
        failed.content_error = Some("signature does not verify".into());
        assert!(matches!(
            r.evaluate(&[failed], &spec(), t0),
            Decision::Halted(m) if m.contains("failed to load")
        ));

        let mut r = Rollout::default();
        r.begin(c("c"), vec!["n1".into()], &[], t0);
        let stuck = [agent("n1", None, &[])];
        assert_eq!(
            r.evaluate(&stuck, &spec(), t0 + chrono::Duration::hours(2)),
            Decision::None
        );
        assert!(matches!(
            r.evaluate(&stuck, &spec(), t0 + chrono::Duration::hours(3)),
            Decision::Halted(m) if m.contains("did not load")
        ));
        assert_eq!(Phase::Idle.as_str(), "Idle");
        assert_eq!(Phase::Canary.as_str(), "Canary");
    }

    #[test]
    fn no_canary_promotes_at_once() {
        let mut r = Rollout::default();
        assert_eq!(
            r.begin(c("a"), vec![], &[], Utc::now()),
            Decision::Promoted("a".into())
        );
        assert_eq!(r.assigned("any").unwrap().revision, "a");
    }

    #[test]
    fn canary_selection() {
        let nodes = [("a", false), ("b", true), ("c", false), ("d", false)];
        assert_eq!(choose_canary(nodes, 0), vec!["b".to_string()]);
        let ten = choose_canary(nodes, 10);
        assert_eq!(ten, vec!["b".to_string()]);
        assert_eq!(choose_canary(nodes, 100).len(), 4);
        let unlabeled = [("a", false), ("b", false)];
        assert_eq!(choose_canary(unlabeled, 10).len(), 1);
        assert_eq!(choose_canary(unlabeled, 10), choose_canary(unlabeled, 10));
        assert!(choose_canary(std::iter::empty(), 50).is_empty());
    }
}
