//! Pod role labelling: in replication mode the main Service selects the leader
//! by label rather than by readiness.

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};

use super::{Elector, Error, Result};

/// Patched onto pods in replication mode so the main Service can select the
/// leader. With per-pod volumes every replica must stay Ready (a StatefulSet
/// rollout waits on readiness), so readiness can no longer encode leadership;
/// the label takes over traffic routing instead.
pub const ROLE_LABEL: &str = "forklift.io/role";

/// Role label value for the elected leader.
pub const ROLE_LEADER: &str = "leader";
/// Role label value for a standby.
pub const ROLE_STANDBY: &str = "standby";

impl Elector {
    fn pods(&self, namespace: &str) -> Api<Pod> {
        Api::namespaced(self.client().clone(), namespace)
    }

    /// Patches this pod's role label. The leader sets "leader" on promotion and
    /// "standby" on demotion; the pod template default is "standby" so
    /// restarted pods start unlabeled as leader.
    pub async fn set_pod_role(&self, namespace: &str, name: &str, role: &str) -> Result<()> {
        let patch = serde_json::json!({"metadata": {"labels": {ROLE_LABEL: role}}});
        self.pods(namespace)
            .patch(name, &PatchParams::default(), &Patch::Strategic(patch))
            .await
            .map(|_| ())
            .map_err(Error::PatchPodRole)
    }

    /// Patches role=standby onto every pod still labeled leader except self. A
    /// new leader calls this after labeling itself: a former leader that was
    /// demoted because it lost the API server typically cannot remove its own
    /// label, and until someone does, the Service would split traffic between
    /// the stale pod and the new leader.
    pub async fn demote_peers(&self, namespace: &str, self_pod: &str) -> Result<()> {
        let lp = ListParams::default().labels(&format!("{ROLE_LABEL}={ROLE_LEADER}"));
        let pods = self
            .pods(namespace)
            .list(&lp)
            .await
            .map_err(Error::ListLeaderPods)?;
        let mut errs = Vec::new();
        for p in pods.items {
            let Some(name) = p.metadata.name.as_deref() else {
                continue;
            };
            if name == self_pod {
                continue;
            }
            match self.set_pod_role(namespace, name, ROLE_STANDBY).await {
                Err(e) => errs.push(e),
                Ok(()) => tracing::warn!(pod = %name, "demoted stale leader label on peer"),
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(Error::Multiple(errs))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::sync::Arc;

    use http::{Request, Response, StatusCode};
    use k8s_openapi::api::coordination::v1::Lease;
    use k8s_openapi::api::core::v1::Pod;
    use kube::client::Body;

    use crate::cluster::*;

    #[derive(Default)]
    pub(crate) struct FakeState {
        /// namespace/name -> Lease.
        pub(crate) leases: HashMap<String, Lease>,
        /// namespace/name -> Pod.
        pub(crate) pods: HashMap<String, Pod>,
        /// Fail every pod patch with this message ("patch denied").
        pub(crate) patch_pods_err: Option<String>,
        /// Fail every pod list with this message ("list denied").
        pub(crate) list_pods_err: Option<String>,
        /// Fail every lease read with this message.
        pub(crate) get_leases_err: Option<String>,
    }

    /// A `kube::Client` backed by [`FakeState`].
    #[derive(Clone, Default)]
    pub(crate) struct FakeApiServer {
        pub(crate) state: Arc<parking_lot::Mutex<FakeState>>,
    }

    fn key(namespace: &str, name: &str) -> String {
        format!("{namespace}/{name}")
    }

    fn json_response(code: StatusCode, body: Vec<u8>) -> Response<Body> {
        Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("build response")
    }

    fn ok_json<T: serde::Serialize>(v: &T) -> Response<Body> {
        json_response(StatusCode::OK, serde_json::to_vec(v).expect("encode"))
    }

    /// A `metav1.Status` failure the kube client maps onto `Error::Api`.
    fn status_error(code: StatusCode, reason: &str, message: &str) -> Response<Body> {
        let body = serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "message": message,
            "reason": reason,
            "code": code.as_u16(),
        });
        json_response(code, serde_json::to_vec(&body).expect("encode"))
    }

    fn not_found(message: &str) -> Response<Body> {
        status_error(StatusCode::NOT_FOUND, "NotFound", message)
    }

    /// Splits `/apis/coordination.k8s.io/v1/namespaces/tools/leases/forklift-leader`
    /// into its namespace, resource and (optional) name.
    fn route(path: &str) -> Option<(String, String, Option<String>)> {
        let parts = path.trim_start_matches('/').split('/');
        let namespace_idx = parts.clone().position(|p| p == "namespaces")?;
        let mut rest = path
            .trim_start_matches('/')
            .split('/')
            .skip(namespace_idx + 1);
        let namespace = rest.next()?.to_owned();
        let resource = rest.next()?.to_owned();
        let name = rest.next().map(str::to_owned);
        Some((namespace, resource, name))
    }

    impl FakeApiServer {
        pub(crate) fn with_objects(objs: Vec<FakeObject>) -> FakeApiServer {
            let fake = FakeApiServer::default();
            {
                let mut st = fake.state.lock();
                for o in objs {
                    match o {
                        FakeObject::Lease(l) => {
                            let l = *l;
                            let ns = l.metadata.namespace.clone().unwrap_or_default();
                            let name = l.metadata.name.clone().unwrap_or_default();
                            st.leases.insert(key(&ns, &name), l);
                        }
                        FakeObject::Pod(p) => {
                            let p = *p;
                            let ns = p.metadata.namespace.clone().unwrap_or_default();
                            let name = p.metadata.name.clone().unwrap_or_default();
                            st.pods.insert(key(&ns, &name), p);
                        }
                    }
                }
            }
            fake
        }

        /// Builds a `kube::Client` speaking to this fake.
        pub(crate) fn client(&self) -> kube::Client {
            let state = Arc::clone(&self.state);
            let svc = tower::service_fn(move |req: Request<Body>| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(serve(state, req).await) }
            });
            kube::Client::new(svc, "default")
        }

        pub(crate) fn pod(&self, namespace: &str, name: &str) -> Option<Pod> {
            self.state.lock().pods.get(&key(namespace, name)).cloned()
        }

        /// The value of a pod's role label, or `""`.
        pub(crate) fn pod_role(&self, namespace: &str, name: &str) -> String {
            self.pod(namespace, name)
                .and_then(|p| p.metadata.labels)
                .and_then(|l| l.get(ROLE_LABEL).cloned())
                .unwrap_or_default()
        }

        /// Stored Lease, if any.
        pub(crate) fn lease(&self, namespace: &str, name: &str) -> Option<Lease> {
            self.state.lock().leases.get(&key(namespace, name)).cloned()
        }
    }

    pub(crate) enum FakeObject {
        Lease(Box<Lease>),
        Pod(Box<Pod>),
    }

    async fn serve(
        state: Arc<parking_lot::Mutex<FakeState>>,
        req: Request<Body>,
    ) -> Response<Body> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let body = match http_body_util::BodyExt::collect(req.into_body()).await {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                return status_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &e.to_string(),
                );
            }
        };
        let Some((namespace, resource, name)) = route(uri.path()) else {
            return not_found(&format!("no route for {}", uri.path()));
        };
        let mut st = state.lock();
        match (resource.as_str(), method.as_str(), name) {
            ("leases", "GET", Some(name)) => {
                if let Some(msg) = st.get_leases_err.clone() {
                    return status_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", &msg);
                }
                match st.leases.get(&key(&namespace, &name)) {
                    Some(l) => ok_json(l),
                    None => not_found(&format!("leases {name:?} not found")),
                }
            }
            ("leases", "POST", _) => {
                let mut lease: Lease = match serde_json::from_slice(&body) {
                    Ok(l) => l,
                    Err(e) => {
                        return status_error(StatusCode::BAD_REQUEST, "BadRequest", &e.to_string());
                    }
                };
                lease.metadata.namespace = Some(namespace.clone());
                let name = lease.metadata.name.clone().unwrap_or_default();
                if st.leases.contains_key(&key(&namespace, &name)) {
                    return status_error(
                        StatusCode::CONFLICT,
                        "AlreadyExists",
                        &format!("leases {name:?} already exists"),
                    );
                }
                lease.metadata.resource_version = Some("1".into());
                st.leases.insert(key(&namespace, &name), lease.clone());
                ok_json(&lease)
            }
            ("leases", "PUT", Some(name)) => {
                let mut lease: Lease = match serde_json::from_slice(&body) {
                    Ok(l) => l,
                    Err(e) => {
                        return status_error(StatusCode::BAD_REQUEST, "BadRequest", &e.to_string());
                    }
                };
                let k = key(&namespace, &name);
                let Some(stored) = st.leases.get(&k) else {
                    return not_found(&format!("leases {name:?} not found"));
                };
                // Optimistic concurrency, the property leader election relies on.
                if lease.metadata.resource_version != stored.metadata.resource_version {
                    return status_error(
                        StatusCode::CONFLICT,
                        "Conflict",
                        "the object has been modified; please apply your changes to the latest version",
                    );
                }
                let next = stored
                    .metadata
                    .resource_version
                    .as_deref()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1)
                    + 1;
                lease.metadata.namespace = Some(namespace.clone());
                lease.metadata.resource_version = Some(next.to_string());
                st.leases.insert(k, lease.clone());
                ok_json(&lease)
            }
            ("pods", "GET", Some(name)) => match st.pods.get(&key(&namespace, &name)) {
                Some(p) => ok_json(p),
                None => not_found(&format!("pods {name:?} not found")),
            },
            ("pods", "GET", None) => {
                if let Some(msg) = st.list_pods_err.clone() {
                    return status_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", &msg);
                }
                let selector = uri
                    .query()
                    .map(|q| {
                        form_urlencoded::parse(q.as_bytes())
                            .filter(|(k, _)| k == "labelSelector")
                            .map(|(_, v)| v.into_owned())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                let items: Vec<Pod> = st
                    .pods
                    .values()
                    .filter(|p| {
                        p.metadata.namespace.as_deref() == Some(namespace.as_str())
                            && matches_selector(p, &selector)
                    })
                    .cloned()
                    .collect();
                ok_json(&serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "PodList",
                    "metadata": {"resourceVersion": "1"},
                    "items": items,
                }))
            }
            ("pods", "PATCH", Some(name)) => {
                if let Some(msg) = st.patch_pods_err.clone() {
                    return status_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", &msg);
                }
                let k = key(&namespace, &name);
                let Some(pod) = st.pods.get_mut(&k) else {
                    return not_found(&format!("pods {name:?} not found"));
                };
                let patch: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(e) => {
                        return status_error(StatusCode::BAD_REQUEST, "BadRequest", &e.to_string());
                    }
                };
                if let Some(labels) = patch
                    .get("metadata")
                    .and_then(|m| m.get("labels"))
                    .and_then(|l| l.as_object())
                {
                    let dst = pod.metadata.labels.get_or_insert_with(Default::default);
                    for (k, v) in labels {
                        if let Some(v) = v.as_str() {
                            dst.insert(k.clone(), v.to_owned());
                        }
                    }
                }
                ok_json(pod)
            }
            (res, m, n) => not_found(&format!("no handler for {m} {res} {n:?}")),
        }
    }

    /// The one selector shape the elector sends: `key=value` (empty matches all).
    fn matches_selector(pod: &Pod, selector: &str) -> bool {
        if selector.is_empty() {
            return true;
        }
        selector.split(',').all(|term| {
            let Some((k, v)) = term.split_once('=') else {
                return true;
            };
            pod.metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(k))
                .map(|got| got == v)
                .unwrap_or(false)
        })
    }

    pub(crate) fn test_elector(objs: Vec<FakeObject>) -> (Arc<Elector>, FakeApiServer) {
        let fake = FakeApiServer::with_objects(objs);
        let cfg = HAConfig {
            lease_name: "forklift-leader".into(),
            lease_namespace: "tools".into(),
            identity: "forklift-1".into(),
            ..HAConfig::default()
        };
        let elector = Elector::new_with_client(cfg, fake.client());
        (elector, fake)
    }

    /// A Lease seed object with the given namespace/name and spec.
    pub(crate) fn lease_object(name: &str, namespace: &str, spec: LeaseSpec) -> FakeObject {
        FakeObject::Lease(Box::new(Lease {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(namespace.into()),
                resource_version: Some("1".into()),
                ..ObjectMeta::default()
            },
            spec: Some(spec),
        }))
    }

    /// A Pod seed object carrying one role label.
    fn pod_object(name: &str, namespace: &str, role: &str) -> FakeObject {
        FakeObject::Pod(Box::new(Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(namespace.into()),
                labels: Some(std::collections::BTreeMap::from([(
                    ROLE_LABEL.to_owned(),
                    role.to_owned(),
                )])),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        }))
    }

    #[tokio::test]
    async fn leader_identity() {
        // No Lease yet: unknown leader, no error.
        let (e, _fake) = test_elector(vec![]);
        let id = e.leader_identity().await.expect("missing lease");
        assert_eq!(id, "", "missing lease: id {id:?}");

        let (e, _fake) = test_elector(vec![lease_object(
            "forklift-leader",
            "tools",
            LeaseSpec {
                holder_identity: Some("forklift-0".into()),
                ..LeaseSpec::default()
            },
        )]);
        let id = e.leader_identity().await.expect("lease with holder");
        assert_eq!(id, "forklift-0", "id {id:?}");

        // Lease without holder.
        let (e, _fake) = test_elector(vec![lease_object(
            "forklift-leader",
            "tools",
            LeaseSpec::default(),
        )]);
        let id = e.leader_identity().await.expect("holderless lease");
        assert_eq!(id, "", "holderless lease: id {id:?}");
    }

    #[tokio::test]
    async fn leader_identity_surfaces_errors() {
        let (e, fake) = test_elector(vec![]);
        fake.state.lock().get_leases_err = Some("get denied".into());
        let err = e.leader_identity().await.expect_err("expected an error");
        assert!(matches!(err, Error::GetLease(_)), "err = {err}");
    }

    #[tokio::test]
    async fn set_pod_role() {
        let (e, fake) = test_elector(vec![pod_object("forklift-1", "tools", ROLE_STANDBY)]);
        e.set_pod_role("tools", "forklift-1", ROLE_LEADER)
            .await
            .expect("set role");
        let role = fake.pod_role("tools", "forklift-1");
        assert_eq!(role, ROLE_LEADER, "role label = {role:?}");

        e.set_pod_role("tools", "missing-pod", ROLE_LEADER)
            .await
            .expect_err("expected error for missing pod");
    }

    #[tokio::test]
    async fn demote_peers() {
        // A stale peer still labeled leader is demoted; self keeps its label.
        let (e, fake) = test_elector(vec![
            pod_object("forklift-0", "tools", ROLE_LEADER),
            pod_object("forklift-1", "tools", ROLE_LEADER),
        ]);
        e.demote_peers("tools", "forklift-1")
            .await
            .expect("demote peers");
        for (name, want) in [("forklift-0", ROLE_STANDBY), ("forklift-1", ROLE_LEADER)] {
            let got = fake.pod_role("tools", name);
            assert_eq!(got, want, "{name} role = {got:?}, want {want:?}");
        }

        // Clean failover: only self is labeled leader, nothing to demote.
        let (e, fake) = test_elector(vec![
            pod_object("forklift-0", "tools", ROLE_STANDBY),
            pod_object("forklift-1", "tools", ROLE_LEADER),
        ]);
        e.demote_peers("tools", "forklift-1")
            .await
            .expect("demote peers (no-op)");
        let got = fake.pod_role("tools", "forklift-0");
        assert_eq!(got, ROLE_STANDBY, "standby peer role = {got:?}");
    }

    #[tokio::test]
    async fn demote_peers_surfaces_errors() {
        let (e, fake) = test_elector(vec![pod_object("forklift-0", "tools", ROLE_LEADER)]);

        fake.state.lock().patch_pods_err = Some("patch denied".into());
        e.demote_peers("tools", "forklift-1")
            .await
            .expect_err("expected error when peer patch fails");

        fake.state.lock().list_pods_err = Some("list denied".into());
        e.demote_peers("tools", "forklift-1")
            .await
            .expect_err("expected error when pod listing fails");
    }

    mod lease {
        use std::time::Duration;

        use tokio_util::sync::CancellationToken;

        use crate::cluster::podlabel::tests::{lease_object, test_elector};
        use crate::cluster::*;

        /// The fencing token is the single-writer guard: shared-storage writes carry
        /// it, and a superseded former leader is rejected because its token is lower.
        /// So it must be the Lease's transition counter exactly, and the two states
        /// that are not yet a number (no Lease, or a Lease nobody has held) have to
        /// read as zero rather than as an error, which is the normal state before the
        /// first election.
        #[tokio::test]
        async fn fencing_token() {
            let (elector, _fake) = test_elector(vec![]);
            let token = elector.fencing_token().await.expect("no Lease");
            assert_eq!(token, 0, "no Lease = {token}, want 0 and no error");

            let (elector, _fake) = test_elector(vec![lease_object(
                "forklift-leader",
                "tools",
                LeaseSpec::default(),
            )]);
            let token = elector
                .fencing_token()
                .await
                .expect("Lease without transitions");
            assert_eq!(
                token, 0,
                "Lease without transitions = {token}, want 0 and no error"
            );

            let (elector, _fake) = test_elector(vec![lease_object(
                "forklift-leader",
                "tools",
                LeaseSpec {
                    holder_identity: Some("forklift-0".into()),
                    lease_transitions: Some(7),
                    ..LeaseSpec::default()
                },
            )]);
            let token = elector.fencing_token().await.expect("token");
            assert_eq!(token, 7, "token = {token}, want the Lease's 7 transitions");
        }

        /// StepDown is the manual-failover control. An instance that is not currently
        /// leading has nothing to release and must report false rather than cancel a
        /// term it does not hold, which would interrupt a standby's own election loop.
        #[tokio::test]
        async fn step_down() {
            let (elector, _fake) = test_elector(vec![]);
            assert!(
                !elector.step_down(),
                "StepDown on a standby reported a hand-off"
            );

            // Leading, but between terms (no cancel registered): still nothing to
            // release.
            elector.state.lock().leading = true;
            assert!(
                !elector.step_down(),
                "StepDown between terms reported a hand-off"
            );

            // A live term is cancelled, which is what releases the Lease, and the term
            // is marked as a voluntary hand-off so the loop waits long enough for a
            // standby to take the freed Lease instead of re-grabbing it.
            let term = CancellationToken::new();
            elector.state.lock().term_cancel = Some(term.clone());
            assert!(
                elector.step_down(),
                "StepDown while leading reported no hand-off"
            );
            assert!(
                term.is_cancelled(),
                "the leadership term was not cancelled, so the Lease is never released"
            );
            assert!(
                elector.state.lock().stepping_down,
                "the hand-off was not marked, so this instance would re-acquire immediately"
            );
        }

        /// The election loop itself: with no Lease in the cluster the elector creates
        /// one, reports leadership, and on cancellation releases it (ReleaseOnCancel)
        /// so a standby need not wait out a full lease duration.
        #[tokio::test]
        async fn run_acquires_creates_and_releases_the_lease() {
            let fake = crate::cluster::podlabel::tests::FakeApiServer::default();
            let cfg = HAConfig {
                enabled: true,
                lease_name: "forklift-leader".into(),
                lease_namespace: "tools".into(),
                identity: "forklift-1".into(),
                lease_duration: Duration::from_secs(3),
                renew_deadline: Duration::from_secs(2),
                retry_period: Duration::from_secs(1),
            };
            let elector = Elector::new_with_client(cfg, fake.client());

            let started = Arc::new(tokio::sync::Notify::new());
            let stopped = Arc::new(tokio::sync::Notify::new());
            let (s1, s2) = (Arc::clone(&started), Arc::clone(&stopped));
            let cancel = CancellationToken::new();
            let loop_handle = tokio::spawn(Arc::clone(&elector).run(
                cancel.clone(),
                move |_| s1.notify_one(),
                move || s2.notify_one(),
            ));

            tokio::time::timeout(Duration::from_secs(5), started.notified())
                .await
                .expect("leadership was never acquired");
            let lease = fake
                .lease("tools", "forklift-leader")
                .expect("lease created");
            let spec = lease.spec.expect("lease spec");
            assert_eq!(spec.holder_identity.as_deref(), Some("forklift-1"));
            assert_eq!(spec.lease_transitions, Some(0));
            assert!(elector.state.lock().leading, "leadership was not recorded");

            // A voluntary hand-off releases the Lease and ends the term.
            assert!(
                elector.step_down(),
                "StepDown while leading reported nothing"
            );
            tokio::time::timeout(Duration::from_secs(5), stopped.notified())
                .await
                .expect("leadership was never released");
            let spec = fake
                .lease("tools", "forklift-leader")
                .and_then(|l| l.spec)
                .expect("lease spec after release");
            assert_eq!(
                spec.holder_identity.as_deref(),
                Some(""),
                "the Lease was not vacated, so a standby waits out the full lease duration"
            );

            cancel.cancel();
            tokio::time::timeout(Duration::from_secs(10), loop_handle)
                .await
                .expect("the election loop did not stop on cancel")
                .expect("election loop task");
        }
    }
}
