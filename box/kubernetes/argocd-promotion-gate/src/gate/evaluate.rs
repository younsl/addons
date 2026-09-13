//! The verdict itself.

use crate::config::{Config, ImageTagMode, OnError};

use super::chain::app_name_for;
use super::image::compare_images;
use super::types::{AppSnapshot, Code, Decision, ImageComparison, ImageRef, UpstreamStatus};

/// Everything a verdict depends on, gathered before evaluation so the decision
/// itself stays pure.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// The Application whose sync is being judged.
    pub app: AppSnapshot,
    /// The upstream Application, `None` when it does not exist.
    pub upstream: Option<AppSnapshot>,
    /// The images the pending sync would deploy, `None` when the lookup was
    /// skipped or failed.
    pub desired_images: Option<Vec<ImageRef>>,
    /// Explains why a fact could not be read.
    pub lookup_error: String,
}

/// Applies the configured checks and returns the verdict.
///
/// The order is deliberate: structural checks first, then upstream status,
/// then the image comparison, which is the only check that depends on a remote
/// lookup. A denial therefore never costs an Argo CD API call it did not need.
///
/// Every message is written for the person who pressed Sync and reads it in
/// the Argo CD error toast. It names the application, the upstream it is
/// waiting on, the observed state, and what to do next, because that toast is
/// usually the only explanation anyone gets.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn evaluate(input: &Input, cfg: &Config) -> Decision {
    let app = &input.app;

    if !cfg.is_gated(&app.project) {
        return Decision::not_gated(&app.name, &app.project, &app.identity);
    }

    // is_gated is true only for an env with a predecessor in the chain, so the
    // upstream env always resolves from here on.
    let upstream_env = cfg.upstream_env(&app.project).unwrap_or_default();
    let upstream_app = app_name_for(upstream_env, &app.identity);
    let name = app.name.as_str();
    let env = app.project.as_str();

    let mut base = Decision::gated(app);

    if app.skip_requested {
        base.allowed = true;
        base.code = Code::Exempt;
        base.message = format!(
            "Sync of {name} is allowed. The Application carries the annotation {} set to true which opts it out of the promotion gate entirely. No upstream environment was checked and no image tag was compared. Remove that annotation to bring {name} back under enforcement.",
            cfg.exempt.annotation
        );
        return base;
    }

    // A rollback is judged before anything upstream is consulted. The revision
    // ran here before, so nothing new can enter the environment, and an
    // incident must not depend on the upstream being healthy enough to satisfy
    // a gate.
    if cfg.rollback.allow_previously_deployed_revision && app.is_rollback() {
        base.allowed = true;
        base.code = Code::Rollback;
        base.message = format!(
            "Sync of {name} is allowed. It targets revision {} which this environment has already deployed so this is a rollback rather than a promotion. A revision that ran here before cannot introduce an image this environment has never run. The upstream state and the image tags were therefore not checked.",
            app.pending_revision
        );
        return base;
    }

    let Some(upstream) = input.upstream.as_ref() else {
        // A failed read must not look like an absent upstream: that would open
        // the gate on every Kubernetes hiccup.
        if !input.lookup_error.is_empty() {
            base.allowed = cfg.image_tag.on_error == OnError::Allow;
            base.code = Code::LookupFailed;
            base.message = format!(
                "Sync of {name} is {}. The promotion gate could not read its upstream counterpart {upstream_app} from the Kubernetes API so it cannot tell whether {upstream_env} has been promoted yet. The underlying error was {}. A read failure is never treated as an absent upstream because that would silently open the gate. Retry the sync once the API is healthy or set imageTag.onError to allow if a lookup failure should not stand in the way of a deploy.",
                verb(base.allowed),
                input.lookup_error
            );
            base.warnings = vec![input.lookup_error.clone()];
            return base;
        }
        // An absent upstream is always allowed, and this is not configurable on
        // purpose. An application that exists in no upstream environment has
        // nothing to be promoted from, so refusing it would leave it
        // permanently undeployable rather than governed. In the estate this was
        // built for that describes roughly a quarter of production apps.
        base.allowed = true;
        base.code = Code::UpstreamMissing;
        base.message = format!(
            "Sync of {name} is allowed. No Application named {upstream_app} exists in the {upstream_env} environment so this application has no upstream counterpart to be promoted from and nothing to wait for. The promotion gate only compares an application against an upstream that actually exists. If {upstream_app} is created later the gate starts enforcing on the next sync of {name}."
        );
        return base;
    };

    if cfg.require.sync && !upstream.is_synced() {
        base.allowed = false;
        base.code = Code::UpstreamOutOfSync;
        base.message = format!(
            "Sync of {name} is blocked. Its upstream counterpart {upstream_app} reports sync status {}. Promotion into {env} requires the {upstream_env} environment to be Synced first so that whatever reaches {env} has already been applied one environment earlier. Sync {upstream_app} and wait for it to settle then retry this sync.",
            upstream.sync_or_unknown()
        );
        return base;
    }

    if cfg.require.health && !upstream.is_healthy() {
        base.allowed = false;
        base.code = Code::UpstreamUnhealthy;
        base.message = format!(
            "Sync of {name} is blocked. Its upstream counterpart {upstream_app} is Synced but reports health status {}. Promotion into {env} requires the {upstream_env} environment to be Healthy so that a release which is already failing upstream cannot move any further. Fix {upstream_app} until it reports Healthy then retry this sync.",
            upstream.health_or_unknown()
        );
        return base;
    }

    if !cfg.image_tag.enabled {
        base.allowed = true;
        base.code = Code::Passed;
        base.message = format!(
            "Sync of {name} is allowed. Its upstream counterpart {upstream_app} is Synced and Healthy. Image tag comparison is disabled in this deployment so nothing was checked about which image this sync would deploy."
        );
        return base;
    }

    let Some(desired) = input.desired_images.as_deref() else {
        let reason = if input.lookup_error.is_empty() {
            "the desired image lookup returned nothing at all".to_string()
        } else {
            input.lookup_error.clone()
        };
        base.allowed = cfg.image_tag.on_error == OnError::Allow;
        base.code = Code::LookupFailed;
        base.message = format!(
            "Sync of {name} is {}. Its upstream counterpart {upstream_app} is Synced and Healthy but the promotion gate could not resolve which images this sync would actually deploy so it cannot compare them against what {upstream_env} is running. The underlying error was {reason}. Check that the Argo CD API token is mounted and that argocd-server is reachable from the gate or set imageTag.onError to allow if a lookup failure should not stand in the way of a deploy.",
            verb(base.allowed)
        );
        base.warnings = vec![reason];
        return base;
    };

    let comparisons = compare_images(desired, &upstream.live_images, &cfg.image_tag.ignore_repos);

    if comparisons.is_empty() {
        base.allowed = true;
        base.code = Code::Passed;
        base.message = format!(
            "Sync of {name} is allowed. Its upstream counterpart {upstream_app} is Synced and Healthy. No image repository could be compared between the two so no image tag was verified."
        );
        base.warnings = vec![format!(
            "No image repository is comparable between {name} and {upstream_app} so image tags were not checked at all. This happens when the two environments share no repository basename which is normal if a sidecar or an agent exists on one side only. Add the repositories that differ by design to imageTag.ignoreRepos if you want that stated explicitly."
        )];
        return base;
    }

    let mismatches = mismatch_sentences(&comparisons, env, upstream_env, &upstream_app);
    let compared = comparisons.len();
    base.images = comparisons;

    if mismatches.is_empty() {
        base.allowed = true;
        base.code = Code::Passed;
        base.message = format!(
            "Sync of {name} is allowed. Its upstream counterpart {upstream_app} is Synced and Healthy and already runs the same image {} that this sync would deploy.",
            plural("tag", compared)
        );
        return base;
    }

    let detail = mismatches.join(" ");
    let tags = plural("tag", mismatches.len());
    base.code = Code::ImageTagMismatch;
    if cfg.image_tag.mode == ImageTagMode::Enforce {
        base.allowed = false;
        base.message = format!(
            "Sync of {name} is blocked. Its upstream counterpart {upstream_app} is Synced and Healthy but this sync would deploy an image {tags} that the {upstream_env} environment is not running. {detail} Promote the same {tags} through {upstream_env} first so that every image reaching {env} has already run one environment earlier."
        );
        return base;
    }
    base.allowed = true;
    base.message = format!(
        "Sync of {name} is allowed with a warning. Its upstream counterpart {upstream_app} is Synced and Healthy but this sync would deploy an image {tags} that the {upstream_env} environment is not running. {detail} The gate is running with imageTag.mode set to warn so the mismatch is only reported."
    );
    base.warnings = vec![format!(
        "Image tag mismatch was allowed because imageTag.mode is set to warn. {detail} Switching imageTag.mode to enforce would block this sync."
    )];
    base
}

/// Turns each failing comparison into one self-contained sentence, so a
/// multi-image mismatch reads as prose rather than a list the Argo CD toast
/// would flatten anyway.
fn mismatch_sentences(
    comparisons: &[ImageComparison],
    env: &str,
    upstream_env: &str,
    upstream_app: &str,
) -> Vec<String> {
    comparisons
        .iter()
        .filter(|cmp| !cmp.matched)
        .map(|cmp| {
            format!(
                "Repository {} would be deployed to {env} with {} while {upstream_app} in {upstream_env} is running with {}.",
                cmp.repository,
                ref_phrase(&cmp.desired_tag),
                ref_phrase(&cmp.upstream_tag)
            )
        })
        .collect()
}

/// Describes a tag or digest in a form that reads inside a sentence.
fn ref_phrase(reference: &str) -> String {
    if reference.is_empty() {
        "no resolvable tag or digest".to_string()
    } else if reference.contains(':') {
        format!("digest {reference}")
    } else {
        format!("tag {reference}")
    }
}

fn plural(word: &str, count: usize) -> String {
    if count > 1 {
        format!("{word}s")
    } else {
        word.to_string()
    }
}

const fn verb(allowed: bool) -> &'static str {
    if allowed { "allowed" } else { "blocked" }
}

/// Attaches the upstream summary to a verdict for the API and log surfaces.
#[must_use]
pub fn with_upstream(
    mut verdict: Decision,
    upstream_env: &str,
    upstream_app: &str,
    upstream: Option<&AppSnapshot>,
) -> Decision {
    verdict.upstream = Some(UpstreamStatus {
        app: upstream_app.to_string(),
        env: upstream_env.to_string(),
        exists: upstream.is_some(),
        sync_status: upstream.map(|u| u.sync_status.clone()).unwrap_or_default(),
        health_status: upstream
            .map(|u| u.health_status.clone())
            .unwrap_or_default(),
    });
    verdict
}

#[cfg(test)]
mod tests {
    use crate::gate::image::parse_image;

    use super::*;

    fn cfg(extra: &str) -> Config {
        Config::parse(&format!("chain: [stg, prd]\n{extra}")).unwrap()
    }

    fn app(name: &str, project: &str) -> AppSnapshot {
        AppSnapshot {
            name: name.to_string(),
            project: project.to_string(),
            identity: crate::gate::chain::identity_of(name, project),
            ..AppSnapshot::default()
        }
    }

    fn healthy_upstream(images: &[&str]) -> AppSnapshot {
        AppSnapshot {
            sync_status: "Synced".into(),
            health_status: "Healthy".into(),
            live_images: images.iter().map(|i| parse_image(i)).collect(),
            ..app("stg-api", "stg")
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn images(items: &[&str]) -> Option<Vec<ImageRef>> {
        Some(items.iter().map(|i| parse_image(i)).collect())
    }

    #[test]
    fn not_gated_environments_pass_through() {
        let verdict = evaluate(
            &Input {
                app: app("stg-api", "stg"),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::NotGated);
        assert!(verdict.allowed);
        assert!(!verdict.gated);
    }

    #[test]
    fn skip_annotation_exempts() {
        let mut a = app("prd-api", "prd");
        a.skip_requested = true;
        let verdict = evaluate(
            &Input {
                app: a,
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::Exempt);
        assert!(verdict.allowed && verdict.gated);
        assert!(
            verdict
                .message
                .contains("promotion-gate.younsl.github.io/skip")
        );
    }

    #[test]
    fn rollback_allowed_before_upstream_is_consulted() {
        let mut a = app("prd-api", "prd");
        a.pending_revision = "old".into();
        a.current_revision = "new".into();
        a.deployed_revisions = vec!["old".into(), "new".into()];
        let verdict = evaluate(
            &Input {
                app: a.clone(),
                upstream: Some(AppSnapshot {
                    sync_status: "OutOfSync".into(),
                    ..app("stg-api", "stg")
                }),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::Rollback);
        assert!(verdict.allowed);

        let verdict = evaluate(
            &Input {
                app: a,
                upstream: Some(AppSnapshot {
                    sync_status: "OutOfSync".into(),
                    ..app("stg-api", "stg")
                }),
                ..Input::default()
            },
            &cfg("rollback:\n  allowPreviouslyDeployedRevision: false\n"),
        );
        assert_eq!(
            verdict.code,
            Code::UpstreamOutOfSync,
            "rollback allowance disabled"
        );
    }

    #[test]
    fn kubernetes_failure_is_not_a_missing_upstream() {
        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                lookup_error: "connection refused".into(),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::LookupFailed);
        assert!(!verdict.allowed, "onError defaults to deny");
        assert_eq!(verdict.warnings, vec!["connection refused"]);
        assert!(verdict.message.contains("blocked"));

        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                lookup_error: "connection refused".into(),
                ..Input::default()
            },
            &cfg("imageTag:\n  onError: allow\n"),
        );
        assert!(verdict.allowed);
        assert!(verdict.message.contains("is allowed"));
    }

    #[test]
    fn missing_upstream_is_allowed() {
        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::UpstreamMissing);
        assert!(verdict.allowed);
        assert!(verdict.message.contains("stg-api"));
    }

    #[test]
    fn upstream_status_checks_block() {
        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(app("stg-api", "stg")),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::UpstreamOutOfSync);
        assert!(verdict.message.contains("sync status Unknown"));

        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(AppSnapshot {
                    sync_status: "Synced".into(),
                    health_status: "Degraded".into(),
                    ..app("stg-api", "stg")
                }),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::UpstreamUnhealthy);
        assert!(verdict.message.contains("health status Degraded"));

        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(app("stg-api", "stg")),
                ..Input::default()
            },
            &cfg("require:\n  sync: false\n  health: false\nimageTag:\n  enabled: false\n"),
        );
        assert_eq!(verdict.code, Code::Passed);
        assert!(verdict.message.contains("comparison is disabled"));
    }

    #[test]
    fn desired_image_lookup_failure_follows_on_error() {
        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(healthy_upstream(&["r/api:1"])),
                desired_images: None,
                lookup_error: String::new(),
            },
            &cfg(""),
        );
        assert_eq!(verdict.code, Code::LookupFailed);
        assert!(!verdict.allowed);
        assert!(verdict.warnings[0].contains("returned nothing at all"));

        let verdict = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(healthy_upstream(&["r/api:1"])),
                desired_images: None,
                lookup_error: "401".into(),
            },
            &cfg("imageTag:\n  onError: allow\n"),
        );
        assert!(verdict.allowed);
        assert_eq!(verdict.warnings, vec!["401"]);
    }

    #[test]
    fn image_comparison_outcomes() {
        let no_overlap = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(healthy_upstream(&["r/other:1"])),
                desired_images: images(&["r/api:1"]),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(no_overlap.code, Code::Passed);
        assert!(no_overlap.warnings[0].contains("not checked at all"));
        assert!(no_overlap.images.is_empty());

        let matched = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(healthy_upstream(&["a/api:1", "a/worker:2"])),
                desired_images: images(&["b/api:1", "b/worker:2"]),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(matched.code, Code::Passed);
        assert!(matched.message.contains("same image tags"));
        assert_eq!(matched.images.len(), 2);

        let warned = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(healthy_upstream(&["a/api:1"])),
                desired_images: images(&["b/api:2"]),
                ..Input::default()
            },
            &cfg(""),
        );
        assert_eq!(warned.code, Code::ImageTagMismatch);
        assert!(warned.allowed);
        assert!(warned.message.contains("allowed with a warning"));
        assert!(warned.warnings[0].contains("tag 2 while stg-api in stg is running with tag 1"));

        let blocked = evaluate(
            &Input {
                app: app("prd-api", "prd"),
                upstream: Some(healthy_upstream(&["a/api@sha256:x", "a/w:1"])),
                desired_images: images(&["b/api@sha256:y", "b/w"]),
                ..Input::default()
            },
            &cfg("imageTag:\n  mode: enforce\n"),
        );
        assert_eq!(blocked.code, Code::ImageTagMismatch);
        assert!(!blocked.allowed);
        assert!(blocked.message.contains("image tags that the stg"));
        assert!(blocked.message.contains("digest sha256:y"));
        assert!(blocked.message.contains("no resolvable tag or digest"));
    }

    #[test]
    fn with_upstream_attaches_summary() {
        let verdict = with_upstream(Decision::not_gated("a", "b", "c"), "stg", "stg-a", None);
        let up = verdict.upstream.unwrap();
        assert!(!up.exists);
        assert!(up.sync_status.is_empty());

        let verdict = with_upstream(
            Decision::not_gated("a", "b", "c"),
            "stg",
            "stg-a",
            Some(&healthy_upstream(&[])),
        );
        let up = verdict.upstream.unwrap();
        assert!(up.exists);
        assert_eq!(up.sync_status, "Synced");
        let json = serde_json::to_value(&up).unwrap();
        assert_eq!(json["healthStatus"], "Healthy");
    }
}
