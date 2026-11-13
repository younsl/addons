//! The agent: applies every `NodeConfigurationBinding` of the node it runs on
//! and reports each result in the binding status.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Client, ResourceExt as _};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::hostfs::HostRoot;
use crate::metrics::Metrics;
use crate::rules::{self, Outcome, Report};
use crate::script::Scripts;
use node_configuration_common::crd::binding::{
    NODE_LABEL, NodeConfigurationBinding, NodeConfigurationBindingStatus, RuleStatus, node_key,
};
use node_configuration_common::crd::validate;

const RETRY: Duration = Duration::from_secs(60);
const WAIT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API request failed, {0}")]
    Kube(#[from] kube::Error),
    #[error("the rule run was aborted, {0}")]
    Aborted(#[from] tokio::task::JoinError),
}

pub struct Context {
    pub client: Client,
    pub host: HostRoot,
    pub scripts: Scripts,
    pub node_name: String,
    pub metrics: Arc<Metrics>,
    pub schedule: Schedule,
}

/// When each binding runs next, by UID. The status the agent writes comes
/// back as a watch event, and without this every write would run the rules
/// again right away.
#[derive(Default)]
pub struct Schedule(Mutex<HashMap<String, (Option<i64>, Instant)>>);

impl Schedule {
    /// How long the binding still has to wait. `None` runs it now, which a
    /// new generation always does.
    fn wait(&self, binding: &NodeConfigurationBinding) -> Option<Duration> {
        let uid = binding.uid()?;
        let run = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&uid)
            .copied();
        let (generation, due) = run?;
        if generation != binding.metadata.generation {
            return None;
        }
        due.checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    fn set(&self, binding: &NodeConfigurationBinding, after: Duration) {
        if let Some(uid) = binding.uid() {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(uid, (binding.metadata.generation, Instant::now() + after));
        }
    }
}

fn log(report: &Report, configuration: &str) {
    for entry in &report.entries {
        let (rule, kind, target, detail) = (
            entry.rule.as_str(),
            entry.kind,
            entry.target.as_str(),
            entry.detail.as_str(),
        );
        match entry.outcome {
            Outcome::Changed => info!(configuration, rule, kind, target, detail, "Corrected drift"),
            Outcome::Pending => info!(
                configuration,
                rule, kind, target, detail, "Found drift, dry run"
            ),
            Outcome::Failed if entry.ignore_failure => warn!(
                configuration,
                rule, kind, target, detail, "Rule failed, ignored"
            ),
            Outcome::Failed => warn!(configuration, rule, kind, target, detail, "Rule failed"),
            Outcome::Unchanged | Outcome::Skipped => {
                debug!(
                    configuration,
                    rule,
                    kind,
                    target,
                    detail,
                    outcome = entry.outcome.as_str(),
                    "Rule checked"
                );
            }
        }
    }
}

/// The status of a run. The transition time moves only when the revision or
/// the readiness changes, so an unchanged node costs no write per interval.
/// What a run corrected is kept until a later run corrects something again.
fn status_of(
    binding: &NodeConfigurationBinding,
    run: Run,
    now: &str,
) -> NodeConfigurationBindingStatus {
    let Run {
        ready,
        failed,
        message,
        mut rules,
    } = run;
    let previous = binding.status.as_ref();
    let mut changed = 0;
    for rule in &mut rules {
        if rule.outcome == Outcome::Changed.as_str() {
            changed += 1;
            rule.last_change_time = Some(now.to_string());
            rule.last_change.clone_from(&rule.message);
        } else if let Some(before) =
            previous.and_then(|s| s.rules.iter().find(|r| r.name == rule.name))
        {
            rule.last_change_time.clone_from(&before.last_change_time);
            rule.last_change.clone_from(&before.last_change);
        }
    }
    let (last_change_time, last_changed_rules) = if changed > 0 {
        (Some(now.to_string()), changed)
    } else {
        previous.map_or((None, 0), |s| {
            (s.last_change_time.clone(), s.last_changed_rules)
        })
    };
    let revision = Some(binding.spec.revision.clone());
    let unchanged = binding
        .status
        .as_ref()
        .filter(|s| s.observed_revision == revision && s.ready == ready);
    NodeConfigurationBindingStatus {
        observed_revision: revision,
        ready,
        failed_rules: i32::try_from(failed).unwrap_or(i32::MAX),
        message,
        rules,
        last_transition_time: unchanged
            .and_then(|s| s.last_transition_time.clone())
            .or_else(|| Some(now.to_string())),
        last_change_time,
        last_changed_rules,
    }
}

/// What one pass over a binding came to.
struct Run {
    ready: bool,
    failed: usize,
    message: Option<String>,
    rules: Vec<RuleStatus>,
}

impl Run {
    const fn finished(failed: usize, message: Option<String>) -> Self {
        Self {
            ready: failed == 0,
            failed,
            message,
            rules: Vec::new(),
        }
    }
}

fn configured(binding: &NodeConfigurationBinding) -> bool {
    binding
        .status
        .as_ref()
        .is_some_and(|s| s.ready && s.observed_revision.as_ref() == Some(&binding.spec.revision))
}

/// The configuration a grouped binding has to wait for: one of the same group
/// on this node with a higher priority that is not configured yet.
fn blocker<'a>(
    binding: &NodeConfigurationBinding,
    others: &'a [NodeConfigurationBinding],
) -> Option<&'a str> {
    let group = binding.spec.group.as_ref()?;
    others
        .iter()
        .filter(|other| {
            other.spec.node_name == binding.spec.node_name
                && other.spec.group.as_ref() == Some(group)
                && other.spec.priority > binding.spec.priority
                && !configured(other)
        })
        .max_by_key(|other| (other.spec.priority, &other.spec.configuration))
        .map(|other| other.spec.configuration.as_str())
}

async fn waiting_for(
    binding: &NodeConfigurationBinding,
    ctx: &Context,
) -> Result<Option<String>, Error> {
    if binding.spec.group.is_none() {
        return Ok(None);
    }
    let mine = ListParams::default().labels(&format!("{NODE_LABEL}={}", node_key(&ctx.node_name)));
    let others = Api::<NodeConfigurationBinding>::all(ctx.client.clone())
        .list(&mine)
        .await?
        .items;
    Ok(blocker(binding, &others).map(ToString::to_string))
}

async fn run(binding: &NodeConfigurationBinding, ctx: &Context) -> Result<Run, Error> {
    let configuration = binding.spec.configuration.as_str();
    if let Some(ahead) = waiting_for(binding, ctx).await? {
        debug!(configuration, ahead, "Waiting for a higher priority");
        return Ok(Run {
            ready: false,
            failed: 0,
            message: Some(format!("waiting for {ahead}")),
            rules: Vec::new(),
        });
    }
    let problems = validate::rule_problems(&binding.spec.rules);
    if !problems.is_empty() {
        let message = problems.join(", ");
        warn!(configuration, "Binding holds invalid rules, {message}");
        return Ok(Run::finished(problems.len(), Some(message)));
    }

    let host = ctx.host.clone();
    let rules = binding.spec.rules.clone();
    let dry_run = binding.spec.dry_run;
    let scripts = ctx.scripts.clone();
    let report =
        tokio::task::spawn_blocking(move || rules::apply_with(&host, &rules, dry_run, &scripts))
            .await?;
    log(&report, configuration);

    let (changed, pending, failed) = (
        report.count(Outcome::Changed),
        report.count(Outcome::Pending),
        report.blocking_failures(),
    );
    let ignored = report.count(Outcome::Failed) - failed;
    ctx.metrics.record_run(
        configuration,
        &report,
        failed,
        chrono::Utc::now().timestamp(),
    );
    if changed > 0 || pending > 0 || failed > 0 || ignored > 0 {
        info!(
            configuration,
            revision = binding.spec.revision,
            changed,
            pending,
            failed,
            ignored,
            unchanged = report.count(Outcome::Unchanged),
            skipped = report.count(Outcome::Skipped),
            "Applied the node configuration"
        );
    }
    let failure = |ignored: bool| {
        report
            .entries
            .iter()
            .find(|e| e.outcome == Outcome::Failed && e.ignore_failure == ignored)
            .map(|e| {
                format!(
                    "rule {} failed{}, {} {}, {}",
                    e.rule,
                    if ignored { " and was ignored" } else { "" },
                    e.kind,
                    e.target,
                    e.detail
                )
            })
    };
    let message = failure(false)
        .or_else(|| failure(true))
        .or_else(|| (pending > 0).then(|| format!("dry run, {pending} changes pending")));
    let rules = report
        .by_rule()
        .into_iter()
        .map(|(name, outcome, message)| RuleStatus {
            name: name.to_string(),
            outcome: outcome.as_str().to_string(),
            message,
            ..RuleStatus::default()
        })
        .collect();
    Ok(Run {
        rules,
        ..Run::finished(failed, message)
    })
}

pub async fn reconcile(
    binding: Arc<NodeConfigurationBinding>,
    ctx: Arc<Context>,
) -> Result<Action, Error> {
    // The node label may be a hash, so the name itself decides.
    if binding.spec.node_name != ctx.node_name {
        return Ok(Action::await_change());
    }
    if let Some(wait) = ctx.schedule.wait(&binding) {
        return Ok(Action::requeue(wait));
    }
    let result = apply(&binding, &ctx).await;
    ctx.metrics.reconciled(result.is_ok());
    let next = result?;
    ctx.schedule.set(&binding, next);
    Ok(Action::requeue(next))
}

/// Runs the binding and returns how long until its next run.
async fn apply(binding: &NodeConfigurationBinding, ctx: &Context) -> Result<Duration, Error> {
    let run = run(binding, ctx).await?;
    let (ready, failed) = (run.ready, run.failed);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let status = status_of(binding, run, &now);
    if binding.status.as_ref() != Some(&status) {
        Api::<NodeConfigurationBinding>::all(ctx.client.clone())
            .patch_status(
                &binding.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({ "status": status })),
            )
            .await?;
    }
    let interval = Duration::from_secs(u64::from(binding.spec.interval_seconds));
    Ok(if ready {
        interval
    } else if failed == 0 {
        WAIT
    } else {
        RETRY.min(interval)
    })
}

#[allow(clippy::needless_pass_by_value)]
pub fn error_policy(
    binding: Arc<NodeConfigurationBinding>,
    error: &Error,
    _ctx: Arc<Context>,
) -> Action {
    warn!(binding = binding.name_any(), "Run failed, {error}");
    Action::requeue(RETRY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use node_configuration_common::crd::binding::NodeConfigurationBindingSpec;
    use node_configuration_common::crd::{FailurePolicy, FileRule, LineEditRule, NamedRule};
    use prometheus_client::registry::Registry;
    use std::path::Path;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const STATUS_PATH: &str =
        "/apis/node-configuration-operator.sh/v1alpha1/nodeconfigurationbindings/mng.node-1/status";

    async fn context(root: &Path) -> (MockServer, Arc<Context>) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = MockServer::start().await;
        let config = kube::Config::new(server.uri().parse().unwrap());
        let ctx = Arc::new(Context {
            client: Client::try_from(config).unwrap(),
            host: HostRoot::new(root),
            scripts: Scripts::Disabled,
            node_name: "node-1".to_string(),
            metrics: Arc::new(Metrics::new(&mut Registry::default())),
            schedule: Schedule::default(),
        });
        (server, ctx)
    }

    fn binding(path: &str) -> NodeConfigurationBinding {
        NodeConfigurationBinding::new(
            "mng.node-1",
            NodeConfigurationBindingSpec {
                configuration: "mng".to_string(),
                node_name: "node-1".to_string(),
                revision: "rev2".to_string(),
                group: None,
                priority: 0,
                dry_run: false,
                interval_seconds: 120,
                rules: vec![NamedRule::file(
                    "motd",
                    FileRule {
                        path: path.to_string(),
                        content: "hi\n".to_string(),
                        mode: "0644".to_string(),
                    },
                )],
            },
        )
    }

    async fn expect_status(server: &MockServer, binding: &NodeConfigurationBinding, times: u64) {
        Mock::given(method("PATCH"))
            .and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(binding))
            .expect(times)
            .mount(server)
            .await;
    }

    async fn reported(server: &MockServer) -> serde_json::Value {
        let requests = server.received_requests().await.unwrap();
        serde_json::from_slice::<serde_json::Value>(&requests.last().unwrap().body).unwrap()
            ["status"]
            .clone()
    }

    #[tokio::test]
    async fn a_node_in_line_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mut binding = binding("/motd");
        expect_status(&server, &binding, 2).await;

        let action = reconcile(Arc::new(binding.clone()), ctx.clone())
            .await
            .unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(120)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("motd")).unwrap(),
            "hi\n"
        );
        let status = reported(&server).await;
        assert_eq!(status["observedRevision"], "rev2");
        assert_eq!(status["ready"], true);
        assert_eq!(status["failedRules"], 0);
        assert!(status["message"].is_null(), "{status}");
        let changed_at = status["lastChangeTime"].clone();
        assert!(changed_at.is_string(), "{status}");
        assert_eq!(status["lastChangedRules"], 1);
        assert_eq!(
            status["rules"],
            json!([{
                "name": "motd",
                "outcome": "changed",
                "message": "file /motd, created",
                "lastChangeTime": changed_at,
                "lastChange": "file /motd, created"
            }])
        );

        // The next run finds the node in line, and that result is written once.
        binding.status = Some(serde_json::from_value(status).unwrap());
        reconcile(Arc::new(binding.clone()), ctx.clone())
            .await
            .unwrap();
        let status = reported(&server).await;
        assert_eq!(
            status["rules"],
            json!([{
                "name": "motd",
                "outcome": "unchanged",
                "lastChangeTime": changed_at,
                "lastChange": "file /motd, created"
            }]),
            "what the first run corrected stays visible"
        );
        assert_eq!(status["lastChangeTime"], changed_at);
        assert_eq!(status["lastChangedRules"], 1);
        binding.status = Some(serde_json::from_value(status).unwrap());
        reconcile(Arc::new(binding), ctx).await.unwrap();
    }

    #[tokio::test]
    async fn a_dry_run_reports_the_drift_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mut binding = binding("/motd");
        binding.spec.dry_run = true;
        expect_status(&server, &binding, 1).await;

        let action = reconcile(Arc::new(binding), ctx).await.unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(120)));
        assert!(!dir.path().join("motd").exists());
        let status = reported(&server).await;
        assert_eq!(status["ready"], true);
        assert_eq!(status["message"], "dry run, 1 changes pending");
        assert_eq!(
            status["rules"],
            json!([{"name": "motd", "outcome": "pending", "message": "file /motd, created"}])
        );
    }

    fn grouped(name: &str, priority: i32, ready: bool) -> NodeConfigurationBinding {
        let mut other = binding("/motd");
        other.metadata.name = Some(format!("{name}.node-1"));
        other.spec.configuration = name.to_string();
        other.spec.group = Some("hardening".to_string());
        other.spec.priority = priority;
        other.status = Some(NodeConfigurationBindingStatus {
            observed_revision: Some("rev2".to_string()),
            ready,
            ..NodeConfigurationBindingStatus::default()
        });
        other
    }

    #[test]
    fn only_an_unconfigured_higher_priority_of_the_group_blocks() {
        let mine = grouped("mng", 10, false);
        assert_eq!(blocker(&mine, &[]), None);
        assert_eq!(blocker(&mine, &[grouped("lower", 5, false)]), None);
        assert_eq!(blocker(&mine, &[grouped("equal", 10, false)]), None);
        assert_eq!(blocker(&mine, &[grouped("done", 20, true)]), None);

        let mut stale = grouped("stale", 20, true);
        stale.spec.revision = "rev3".to_string();
        let others = [
            grouped("first", 30, false),
            stale,
            grouped("done", 40, true),
        ];
        assert_eq!(blocker(&mine, &others), Some("first"));

        let mut elsewhere = grouped("elsewhere", 30, false);
        elsewhere.spec.node_name = "node-2".to_string();
        let mut other_group = grouped("other", 30, false);
        other_group.spec.group = Some("tuning".to_string());
        assert_eq!(blocker(&mine, &[elsewhere, other_group]), None);

        let mut ungrouped = binding("/motd");
        ungrouped.spec.priority = -5;
        assert_eq!(blocker(&ungrouped, &[grouped("first", 30, false)]), None);
    }

    #[tokio::test]
    async fn a_grouped_binding_waits_for_a_higher_priority() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mine = grouped("mng", 10, false);
        Mock::given(method("GET"))
            .and(path(
                "/apis/node-configuration-operator.sh/v1alpha1/nodeconfigurationbindings",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "apiVersion": "node-configuration-operator.sh/v1alpha1",
                "kind": "NodeConfigurationBindingList",
                "metadata": {},
                "items": [grouped("base", 20, false), mine.clone()],
            })))
            .mount(&server)
            .await;
        expect_status(&server, &mine, 1).await;

        let action = reconcile(Arc::new(mine), ctx).await.unwrap();
        assert_eq!(action, Action::requeue(WAIT));
        assert!(!dir.path().join("motd").exists());
        let status = reported(&server).await;
        assert_eq!(status["ready"], false);
        assert_eq!(status["failedRules"], 0);
        assert_eq!(status["message"], "waiting for base");
    }

    #[tokio::test]
    async fn a_failed_rule_reports_not_ready_and_retries_sooner() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let binding = binding("/missing/motd");
        expect_status(&server, &binding, 1).await;

        let action = reconcile(Arc::new(binding), ctx).await.unwrap();
        assert_eq!(action, Action::requeue(RETRY));
        let status = reported(&server).await;
        assert_eq!(status["ready"], false);
        assert_eq!(status["failedRules"], 1);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .starts_with("rule motd failed, file /missing/motd"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn an_ignored_failure_leaves_the_node_configured() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mut binding = binding("/missing/motd");
        binding.spec.rules[0].failure_policy = FailurePolicy::Ignore;
        binding.spec.rules.push(NamedRule::file(
            "issue",
            FileRule {
                path: "/issue".to_string(),
                content: "hi\n".to_string(),
                mode: "0644".to_string(),
            },
        ));
        expect_status(&server, &binding, 2).await;

        let action = reconcile(Arc::new(binding.clone()), ctx.clone())
            .await
            .unwrap();
        assert_eq!(action, Action::requeue(Duration::from_secs(120)));
        assert!(dir.path().join("issue").exists());
        let status = reported(&server).await;
        assert_eq!(status["ready"], true);
        assert_eq!(status["failedRules"], 0);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .starts_with("rule motd failed and was ignored, file /missing/motd"),
            "{status}"
        );
        assert_eq!(status["rules"][0]["outcome"], "failed");
        assert_eq!(status["rules"][1]["outcome"], "changed");

        // A failure that is not ignored is the one that is reported.
        binding.spec.rules[1] = NamedRule::file(
            "issue",
            FileRule {
                path: "/missing/issue".to_string(),
                content: String::new(),
                mode: "0644".to_string(),
            },
        );
        let action = reconcile(Arc::new(binding), ctx).await.unwrap();
        assert_eq!(action, Action::requeue(RETRY));
        let status = reported(&server).await;
        assert_eq!(status["ready"], false);
        assert_eq!(status["failedRules"], 1);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .starts_with("rule issue failed"),
            "{status}"
        );
    }

    #[tokio::test]
    async fn invalid_rules_apply_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mut binding = binding("/motd");
        binding.spec.rules.push(NamedRule::line_edit(
            "hosts",
            LineEditRule {
                path: "/hosts".to_string(),
                pattern: "(".to_string(),
                replacement: String::new(),
            },
        ));
        expect_status(&server, &binding, 1).await;

        reconcile(Arc::new(binding), ctx).await.unwrap();
        assert!(!dir.path().join("motd").exists());
        let status = reported(&server).await;
        assert_eq!(status["ready"], false);
        assert!(
            status["message"]
                .as_str()
                .unwrap()
                .contains("rule \"hosts\" pattern")
        );
    }

    #[tokio::test]
    async fn a_binding_of_another_node_is_ignored_and_api_errors_retry() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mut other = binding("/motd");
        other.spec.node_name = "node-2".to_string();
        let action = reconcile(Arc::new(other), ctx.clone()).await.unwrap();
        assert_eq!(action, Action::await_change());
        assert!(!dir.path().join("motd").exists());

        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let mine = Arc::new(binding("/motd"));
        let error = reconcile(mine.clone(), ctx.clone()).await.unwrap_err();
        assert_eq!(error_policy(mine, &error, ctx), Action::requeue(RETRY));
    }

    #[tokio::test]
    async fn a_status_write_does_not_run_the_rules_again() {
        let dir = tempfile::tempdir().unwrap();
        let (server, ctx) = context(dir.path()).await;
        let mut binding = binding("/motd");
        binding.metadata.uid = Some("uid-1".to_string());
        binding.metadata.generation = Some(1);
        expect_status(&server, &binding, 2).await;

        reconcile(Arc::new(binding.clone()), ctx.clone())
            .await
            .unwrap();
        // The watch event of the status that was just written.
        binding.status = Some(serde_json::from_value(reported(&server).await).unwrap());
        std::fs::remove_file(dir.path().join("motd")).unwrap();
        let action = reconcile(Arc::new(binding.clone()), ctx.clone())
            .await
            .unwrap();
        assert_ne!(action, Action::await_change());
        assert!(
            !dir.path().join("motd").exists(),
            "the rules wait for the interval"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let left = ctx.schedule.wait(&binding).unwrap();
        assert!(left <= Duration::from_secs(120) && left > Duration::from_secs(110));

        // A new generation runs at once, and so does a binding that was recreated.
        binding.status = None;
        binding.metadata.generation = Some(2);
        assert_eq!(ctx.schedule.wait(&binding), None);
        reconcile(Arc::new(binding.clone()), ctx.clone())
            .await
            .unwrap();
        assert!(dir.path().join("motd").exists());
        binding.metadata.uid = Some("uid-2".to_string());
        assert_eq!(ctx.schedule.wait(&binding), None);
    }

    #[test]
    fn a_change_is_kept_until_a_later_run_changes_something() {
        let rule = |outcome: Outcome, message: Option<&str>| RuleStatus {
            name: "motd".to_string(),
            outcome: outcome.as_str().to_string(),
            message: message.map(ToString::to_string),
            ..RuleStatus::default()
        };
        let run = |rules: Vec<RuleStatus>| Run {
            rules,
            ..Run::finished(0, None)
        };
        let mut binding = binding("/motd");
        let first = status_of(
            &binding,
            run(vec![rule(Outcome::Changed, Some("created"))]),
            "t1",
        );
        assert_eq!(first.last_change_time.as_deref(), Some("t1"));
        assert_eq!(first.last_changed_rules, 1);

        binding.status = Some(first);
        let second = status_of(&binding, run(vec![rule(Outcome::Unchanged, None)]), "t2");
        assert_eq!(second.last_change_time.as_deref(), Some("t1"));
        assert_eq!(second.last_changed_rules, 1);
        assert_eq!(second.rules[0].last_change_time.as_deref(), Some("t1"));
        assert_eq!(second.rules[0].last_change.as_deref(), Some("created"));

        binding.status = Some(second);
        let waiting = status_of(&binding, Run::finished(0, None), "t3");
        assert_eq!(waiting.last_change_time.as_deref(), Some("t1"));
        binding.status = Some(waiting);
        let third = status_of(
            &binding,
            run(vec![rule(Outcome::Changed, Some("rewritten"))]),
            "t4",
        );
        assert_eq!(third.last_change_time.as_deref(), Some("t4"));
        assert_eq!(third.rules[0].last_change.as_deref(), Some("rewritten"));
    }

    #[test]
    fn transition_time_moves_only_with_revision_or_readiness() {
        let mut binding = binding("/motd");
        let first = status_of(&binding, Run::finished(0, None), "t1");
        assert_eq!(first.last_transition_time.as_deref(), Some("t1"));
        binding.status = Some(first);
        assert_eq!(
            status_of(&binding, Run::finished(0, None), "t2")
                .last_transition_time
                .as_deref(),
            Some("t1")
        );
        assert_eq!(
            status_of(&binding, Run::finished(1, None), "t3")
                .last_transition_time
                .as_deref(),
            Some("t3")
        );
        binding.spec.revision = "rev3".to_string();
        assert_eq!(
            status_of(&binding, Run::finished(0, None), "t4")
                .last_transition_time
                .as_deref(),
            Some("t4")
        );
    }
}
