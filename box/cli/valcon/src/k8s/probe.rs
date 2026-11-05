//! Ephemeral `valcon-probe` containers used as the network path to cache nodes.
//!
//! One probe per pod, reused across queries and sessions. Ephemeral containers cannot be
//! removed, so a new one is added only when none is running, and the name gets a numeric
//! suffix in that case.

use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use kube::Api;
use kube::api::{Patch, PatchParams};
use serde_json::{Value, json};

use super::error::K8sError;

pub const NAME: &str = "valcon-probe";
const MAX_PROBES: usize = 5;
const POLL: Duration = Duration::from_millis(500);
/// UID and GID of the `valkey` user in the official image.
const UID: i64 = 999;
const GID: i64 = 1000;

fn statuses(pod: &Pod) -> &[ContainerStatus] {
    pod.status
        .as_ref()
        .and_then(|s| s.ephemeral_container_statuses.as_deref())
        .unwrap_or_default()
}

/// Name of a running probe container, if the pod has one.
pub fn running(pod: &Pod) -> Option<String> {
    statuses(pod)
        .iter()
        .find(|s| {
            s.name.starts_with(NAME) && s.state.as_ref().is_some_and(|st| st.running.is_some())
        })
        .map(|s| s.name.clone())
}

fn next_name(pod: &Pod) -> Option<String> {
    let taken: Vec<&str> = pod
        .spec
        .as_ref()
        .and_then(|s| s.ephemeral_containers.as_deref())
        .unwrap_or_default()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    (0..MAX_PROBES)
        .map(|i| {
            if i == 0 {
                NAME.to_owned()
            } else {
                format!("{NAME}-{}", i + 1)
            }
        })
        .find(|n| !taken.contains(&n.as_str()))
}

/// Idles until the pod stops. Locked down so restricted Pod Security admits it.
fn spec(name: &str, image: &str) -> Value {
    json!({
        "name": name,
        "image": image,
        "imagePullPolicy": "IfNotPresent",
        "command": ["sh", "-c", "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done"],
        "securityContext": {
            "runAsNonRoot": true,
            "runAsUser": UID,
            "runAsGroup": GID,
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "capabilities": { "drop": ["ALL"] },
            "seccompProfile": { "type": "RuntimeDefault" }
        }
    })
}

enum Progress {
    Running,
    Waiting,
    Failed(String),
}

fn progress(pod: &Pod, name: &str) -> Progress {
    let Some(state) = statuses(pod)
        .iter()
        .find(|s| s.name == name)
        .and_then(|s| s.state.as_ref())
    else {
        return Progress::Waiting;
    };
    if state.running.is_some() {
        return Progress::Running;
    }
    if let Some(t) = &state.terminated {
        return Progress::Failed(format!(
            "{name} exited: {}",
            t.reason.clone().unwrap_or_default()
        ));
    }
    match state.waiting.as_ref().and_then(|w| w.reason.as_deref()) {
        Some(
            r @ ("ErrImagePull"
            | "ImagePullBackOff"
            | "InvalidImageName"
            | "CreateContainerConfigError"),
        ) => {
            let msg = state
                .waiting
                .as_ref()
                .and_then(|w| w.message.clone())
                .unwrap_or_default();
            Progress::Failed(format!("{name}: {r} {msg}"))
        }
        _ => Progress::Waiting,
    }
}

/// Returns the name of a running probe in `pod`, adding one when needed.
pub async fn ensure(
    api: &Api<Pod>,
    pod: &str,
    image: &str,
    timeout: Duration,
) -> Result<String, K8sError> {
    let current = api.get(pod).await?;
    if let Some(name) = running(&current) {
        return Ok(name);
    }
    let name = next_name(&current).ok_or_else(|| {
        K8sError::Probe(format!("{pod} already has {MAX_PROBES} probe containers"))
    })?;
    let patch = json!({ "spec": { "ephemeralContainers": [spec(&name, image)] } });
    api.patch_ephemeral_containers(pod, &PatchParams::default(), &Patch::Strategic(patch))
        .await?;
    tracing::info!(pod, container = %name, image, "added probe container");

    let started = Instant::now();
    loop {
        match progress(&api.get(pod).await?, &name) {
            Progress::Running => return Ok(name),
            Progress::Failed(e) => return Err(K8sError::Probe(e)),
            Progress::Waiting if started.elapsed() >= timeout => {
                return Err(K8sError::Probe(format!(
                    "{name} not running after {}s",
                    timeout.as_secs()
                )));
            }
            Progress::Waiting => tokio::time::sleep(POLL).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
        EphemeralContainer, PodSpec, PodStatus,
    };

    use super::*;

    fn status(name: &str, state: ContainerState) -> ContainerStatus {
        ContainerStatus {
            name: name.into(),
            state: Some(state),
            ..ContainerStatus::default()
        }
    }

    fn pod(probes: &[&str], statuses: Vec<ContainerStatus>) -> Pod {
        Pod {
            spec: Some(PodSpec {
                ephemeral_containers: Some(
                    probes
                        .iter()
                        .map(|n| EphemeralContainer {
                            name: (*n).into(),
                            ..EphemeralContainer::default()
                        })
                        .collect(),
                ),
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                ephemeral_container_statuses: Some(statuses),
                ..PodStatus::default()
            }),
            ..Pod::default()
        }
    }

    fn run() -> ContainerState {
        ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..ContainerState::default()
        }
    }

    fn waiting(reason: &str) -> ContainerState {
        ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.into()),
                message: Some("detail".into()),
            }),
            ..ContainerState::default()
        }
    }

    fn done() -> ContainerState {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                reason: Some("Completed".into()),
                ..ContainerStateTerminated::default()
            }),
            ..ContainerState::default()
        }
    }

    #[test]
    fn reuses_running_probe_and_names_the_next() {
        let p = pod(
            &[NAME, "valcon-probe-2"],
            vec![status(NAME, done()), status("valcon-probe-2", run())],
        );
        assert_eq!(running(&p).as_deref(), Some("valcon-probe-2"));
        assert_eq!(next_name(&p).as_deref(), Some("valcon-probe-3"));
        let fresh = pod(&[], vec![]);
        assert_eq!(running(&fresh), None);
        assert_eq!(next_name(&fresh).as_deref(), Some(NAME));
        let full: Vec<String> = (0..MAX_PROBES)
            .map(|i| {
                if i == 0 {
                    NAME.into()
                } else {
                    format!("{NAME}-{}", i + 1)
                }
            })
            .collect();
        let full_refs: Vec<&str> = full.iter().map(String::as_str).collect();
        assert_eq!(next_name(&pod(&full_refs, vec![])), None);
        assert_eq!(running(&Pod::default()), None);
    }

    #[test]
    fn progress_reads_container_state() {
        assert!(matches!(
            progress(&pod(&[NAME], vec![status(NAME, run())]), NAME),
            Progress::Running
        ));
        assert!(matches!(
            progress(&pod(&[NAME], vec![]), NAME),
            Progress::Waiting
        ));
        assert!(matches!(
            progress(
                &pod(&[NAME], vec![status(NAME, waiting("ContainerCreating"))]),
                NAME
            ),
            Progress::Waiting
        ));
        let Progress::Failed(e) = progress(
            &pod(&[NAME], vec![status(NAME, waiting("ImagePullBackOff"))]),
            NAME,
        ) else {
            panic!()
        };
        assert!(e.contains("ImagePullBackOff detail"));
        assert!(matches!(
            progress(&pod(&[NAME], vec![status(NAME, done())]), NAME),
            Progress::Failed(_)
        ));
    }

    #[test]
    fn spec_is_locked_down() {
        let s = spec(NAME, "valkey/valkey:9.1.3-alpine");
        assert_eq!(s["securityContext"]["runAsNonRoot"], true);
        assert_eq!(s["securityContext"]["capabilities"]["drop"][0], "ALL");
        assert_eq!(s["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(s["image"], "valkey/valkey:9.1.3-alpine");
    }
}
