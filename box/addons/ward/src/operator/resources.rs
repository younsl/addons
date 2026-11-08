//! The objects one `Ward` owns, built as plain values so they can be tested
//! without a cluster.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{DaemonSet, StatefulSet};
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service, ServiceAccount};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::api::rbac::v1::{ClusterRoleBinding, RoleBinding};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource as _;
use kube::api::{ApiResource, DynamicObject};
use serde_json::{Value, json};

use crate::crd::{AgentUpdateStrategy, DnsPolicy, SslMode, UpdateStrategyType, Ward, WardSpec};
use crate::k8s::{COMPONENT_AGENT, COMPONENT_MANAGER, COMPONENT_POSTGRESQL, labels};
use crate::policy::POLICY_FILE;
use crate::rollout::ROLLOUT_FILE;

/// Fixed so the Lease admission policy can recognize agents. Only one
/// `Ward` is active per cluster.
pub const AGENT_SERVICE_ACCOUNT: &str = "ward-agent";
pub const AGENT_PORT: i32 = 8443;
pub const ADMIN_PORT: i32 = 8080;
pub const METRICS_PORT: i32 = 8081;
pub const POSTGRESQL_PORT: i32 = 5432;
/// The account and database of the embedded PostgreSQL server.
pub const POSTGRESQL_USER: &str = "ward";
/// Key of the password in the embedded server's auth `Secret`.
pub const POSTGRESQL_PASSWORD_KEY: &str = "password";
/// UID and GID of the `postgres` user in the Alpine based images.
const POSTGRESQL_UID: i64 = 70;
const PG_HBA: &str = "local all all trust\nhostssl all all all scram-sha-256\n";
pub const AGENT_CAPABILITIES: [&str; 9] = [
    "BPF",
    "PERFMON",
    "SYS_RESOURCE",
    "SYS_PTRACE",
    "MAC_ADMIN",
    "KILL",
    "DAC_READ_SEARCH",
    "DAC_OVERRIDE",
    "FOWNER",
];

/// Settings the operator passes down to every `Ward`.
#[derive(Clone, Debug, Default)]
pub struct Defaults {
    pub image: String,
    pub image_pull_policy: String,
    pub workload_cluster_role: String,
    pub lease_cluster_role: String,
    /// The only namespace a `Ward` may live in, the operator's own.
    pub ward_namespace: String,
    /// Prefixes `spec.image` must start with. Empty allows only the
    /// operator's own repository.
    pub allowed_image_prefixes: Vec<String>,
    pub allow_privileged_agent: bool,
    /// Image of the embedded PostgreSQL server.
    pub postgresql_image: String,
}

/// Names of every object a `Ward` owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub name: String,
    pub namespace: String,
}

impl Names {
    pub fn of(ward: &Ward) -> Self {
        Self {
            name: ward.metadata.name.clone().unwrap_or_default(),
            namespace: ward.metadata.namespace.clone().unwrap_or_default(),
        }
    }
    pub fn cluster_role_binding(&self) -> String {
        format!("ward-{}-{}", self.namespace, self.name)
    }
    pub fn lease_role_binding(&self) -> String {
        format!("{}-agent-lease", self.name)
    }
    pub fn policy(&self) -> String {
        format!("{}-policy", self.name)
    }
    pub fn rollout(&self) -> String {
        format!("{}-rollout", self.name)
    }
    pub fn agent(&self) -> String {
        format!("{}-agent", self.name)
    }
    pub fn manager(&self) -> String {
        format!("{}-manager", self.name)
    }
    pub fn ca(&self) -> String {
        format!("{}-ca", self.name)
    }
    pub fn manager_tls(&self) -> String {
        format!("{}-manager-tls", self.name)
    }
    pub fn agent_tls(&self) -> String {
        format!("{}-agent-tls", self.name)
    }
    pub fn api_token(&self) -> String {
        format!("{}-api-token", self.name)
    }
    pub fn manager_url(&self) -> String {
        format!(
            "https://{}.{}.svc:{AGENT_PORT}",
            self.manager(),
            self.namespace
        )
    }
    pub fn postgresql(&self) -> String {
        format!("{}-postgresql", self.name)
    }
    pub fn postgresql_tls(&self) -> String {
        format!("{}-postgresql-tls", self.name)
    }
    pub fn postgresql_auth(&self) -> String {
        format!("{}-postgresql-auth", self.name)
    }
    pub fn postgresql_host(&self) -> String {
        format!("{}.{}.svc", self.postgresql(), self.namespace)
    }
    pub fn postgresql_dns(&self) -> Vec<String> {
        Self::dns(&self.postgresql(), &self.namespace)
    }
    pub fn manager_dns(&self) -> Vec<String> {
        Self::dns(&self.manager(), &self.namespace)
    }
    fn dns(svc: &str, ns: &str) -> Vec<String> {
        let svc = svc.to_string();
        vec![
            svc.clone(),
            format!("{svc}.{ns}"),
            format!("{svc}.{ns}.svc"),
            format!("{svc}.{ns}.svc.cluster.local"),
            format!("*.{svc}.{ns}.svc"),
        ]
    }
}

pub fn owner(ward: &Ward) -> OwnerReference {
    ward.controller_owner_ref(&()).unwrap_or_default()
}

fn meta(ward: &Ward, name: &str, component: &str) -> Value {
    let n = Names::of(ward);
    json!({
        "name": name,
        "namespace": n.namespace,
        "labels": labels(&n.name, component),
        "ownerReferences": [owner(ward)],
    })
}

fn typed<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap_or_else(|e| panic!("operator object is well formed: {e}"))
}

fn image(spec: &WardSpec, d: &Defaults) -> (String, String) {
    (
        spec.image.clone().unwrap_or_else(|| d.image.clone()),
        spec.image_pull_policy
            .map_or_else(|| d.image_pull_policy.clone(), |p| p.as_str().to_string()),
    )
}

fn pull_secrets(spec: &WardSpec) -> Value {
    json!(
        spec.image_pull_secrets
            .iter()
            .map(|s| json!({"name": s.name}))
            .collect::<Vec<_>>()
    )
}

pub fn service_account(ward: &Ward) -> ServiceAccount {
    typed(json!({ "metadata": meta(ward, AGENT_SERVICE_ACCOUNT, COMPONENT_AGENT) }))
}

pub fn cluster_role_binding(ward: &Ward, d: &Defaults) -> ClusterRoleBinding {
    let n = Names::of(ward);
    typed(json!({
        "metadata": {
            "name": n.cluster_role_binding(),
            "labels": labels(&n.name, COMPONENT_AGENT),
            "annotations": {"ward.sh/owner": format!("{}/{}", n.namespace, n.name)},
        },
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": d.workload_cluster_role},
        "subjects": [{"kind": "ServiceAccount", "name": AGENT_SERVICE_ACCOUNT, "namespace": n.namespace}],
    }))
}

pub fn lease_role_binding(ward: &Ward, d: &Defaults) -> RoleBinding {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.lease_role_binding(), COMPONENT_AGENT),
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": d.lease_cluster_role},
        "subjects": [{"kind": "ServiceAccount", "name": AGENT_SERVICE_ACCOUNT, "namespace": n.namespace}],
    }))
}

pub fn config_map(ward: &Ward, name: &str, key: &str, value: &str) -> ConfigMap {
    typed(json!({
        "metadata": meta(ward, name, COMPONENT_AGENT),
        "data": {key: value},
    }))
}

pub fn policy_map(ward: &Ward, policy_json: &str) -> ConfigMap {
    config_map(ward, &Names::of(ward).policy(), POLICY_FILE, policy_json)
}

pub fn rollout_map(ward: &Ward, rollout_json: &str) -> ConfigMap {
    config_map(ward, &Names::of(ward).rollout(), ROLLOUT_FILE, rollout_json)
}

pub fn secret(
    ward: &Ward,
    name: &str,
    data: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
) -> Secret {
    let mut m = meta(ward, name, COMPONENT_MANAGER);
    m["annotations"] = json!(annotations);
    typed(json!({"metadata": m, "type": "Opaque", "stringData": data}))
}

pub fn manager_service(ward: &Ward) -> Service {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.manager(), COMPONENT_MANAGER),
        "spec": {
            "clusterIP": "None",
            "selector": labels(&n.name, COMPONENT_MANAGER).into_iter().filter(|(k, _)| k != crate::k8s::LABEL_MANAGED_BY).collect::<BTreeMap<_, _>>(),
            "ports": [
                {"name": "agent", "port": AGENT_PORT, "targetPort": "agent"},
                {"name": "api", "port": ADMIN_PORT, "targetPort": "api"},
                {"name": "metrics", "port": METRICS_PORT, "targetPort": "metrics"},
            ],
        },
    }))
}

fn selector(n: &Names, component: &str) -> Value {
    json!({"matchLabels": labels(&n.name, component).into_iter().filter(|(k, _)| k != crate::k8s::LABEL_MANAGED_BY).collect::<BTreeMap<_, _>>()})
}

fn probes() -> (Value, Value) {
    (
        json!({"httpGet": {"path": "/readyz", "port": "metrics"}, "periodSeconds": 10}),
        json!({"httpGet": {"path": "/healthz", "port": "metrics"}, "periodSeconds": 20, "failureThreshold": 6}),
    )
}

fn resources(
    r: Option<&k8s_openapi::api::core::v1::ResourceRequirements>,
    cpu: &str,
    mem: &str,
    limit: &str,
) -> Value {
    r.map_or_else(
        || json!({"requests": {"cpu": cpu, "memory": mem}, "limits": {"memory": limit}}),
        |r| serde_json::to_value(r).unwrap_or_default(),
    )
}

pub fn manager_statefulset(ward: &Ward, d: &Defaults) -> StatefulSet {
    let n = Names::of(ward);
    let spec = &ward.spec;
    let (img, pull) = image(spec, d);
    let (ready, live) = probes();
    let mut volumes = vec![
        json!({"name": "policy", "configMap": {"name": n.policy()}}),
        json!({"name": "rollout", "configMap": {"name": n.rollout()}}),
        json!({"name": "tls", "secret": {"secretName": n.manager_tls()}}),
        json!({"name": "token", "secret": {"secretName": n.api_token()}}),
    ];
    let mut mounts = vec![
        json!({"name": "data", "mountPath": "/var/lib/ward"}),
        json!({"name": "policy", "mountPath": "/etc/ward/policy", "readOnly": true}),
        json!({"name": "rollout", "mountPath": "/etc/ward/rollout", "readOnly": true}),
        json!({"name": "tls", "mountPath": "/etc/ward/tls", "readOnly": true}),
        json!({"name": "token", "mountPath": "/etc/ward/api-token", "readOnly": true}),
    ];
    let mut env = vec![
        json!({"name": "WARD_RETENTION", "value": spec.manager.retention.to_string()}),
        json!({"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}}),
    ];
    if let Some(ps) = spec.feed.as_ref().and_then(|f| f.pull_secret_ref.as_ref()) {
        volumes.push(json!({"name": "registry", "secret": {"secretName": ps.name}}));
        mounts
            .push(json!({"name": "registry", "mountPath": "/etc/ward/registry", "readOnly": true}));
        env.push(
            json!({"name": "WARD_REGISTRY_AUTH", "value": "/etc/ward/registry/.dockerconfigjson"}),
        );
    }
    let (pg_env, pg_volumes, pg_mounts) = postgresql_client(ward);
    env.extend(pg_env);
    volumes.extend(pg_volumes);
    mounts.extend(pg_mounts);
    let mut pvc_labels = labels(&n.name, COMPONENT_MANAGER);
    pvc_labels.remove(crate::k8s::LABEL_MANAGED_BY);
    typed(json!({
        "metadata": meta(ward, &n.manager(), COMPONENT_MANAGER),
        "spec": {
            "serviceName": n.manager(),
            "replicas": 1,
            "selector": selector(&n, COMPONENT_MANAGER),
            "template": {
                "metadata": {"labels": labels(&n.name, COMPONENT_MANAGER)},
                "spec": {
                    "serviceAccountName": AGENT_SERVICE_ACCOUNT,
                    "imagePullSecrets": pull_secrets(spec),
                    "nodeSelector": spec.manager.node_selector,
                    "tolerations": spec.manager.tolerations,
                    "affinity": spec.manager.affinity,
                    "dnsPolicy": spec.manager.dns_policy.map(DnsPolicy::as_str),
                    "dnsConfig": spec.manager.dns_config,
                    "topologySpreadConstraints": spec.manager.topology_spread_constraints,
                    "securityContext": {"runAsNonRoot": true, "runAsUser": 65532, "runAsGroup": 65532, "fsGroup": 65532, "seccompProfile": {"type": "RuntimeDefault"}},
                    "terminationGracePeriodSeconds": 20,
                    "containers": [{
                        "name": "manager",
                        "image": img,
                        "imagePullPolicy": pull,
                        "args": ["manager"],
                        "env": env,
                        "ports": [
                            {"name": "agent", "containerPort": AGENT_PORT},
                            {"name": "api", "containerPort": ADMIN_PORT},
                            {"name": "metrics", "containerPort": METRICS_PORT},
                        ],
                        "readinessProbe": ready,
                        "livenessProbe": live,
                        "resources": resources(spec.manager.resources.as_ref(), "50m", "64Mi", "512Mi"),
                        "securityContext": {"allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": {"drop": ["ALL"]}},
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
            "volumeClaimTemplates": [{
                "metadata": {"name": "data", "labels": pvc_labels},
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": spec.manager.storage.storage_class_name,
                    "resources": {"requests": {"storage": spec.manager.storage.size}},
                },
            }],
        },
    }))
}

/// Environment, volumes, and mounts that point the manager at its
/// PostgreSQL server, empty when detections stay in a file.
#[allow(
    clippy::option_if_let_else,
    reason = "the external arm also adds the CA volume"
)]
fn postgresql_client(ward: &Ward) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let n = Names::of(ward);
    let pg = &ward.spec.postgresql;
    if !pg.enabled {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let mut volumes = Vec::new();
    let mut mounts = vec![
        json!({"name": "postgresql-auth", "mountPath": "/etc/ward/postgresql", "readOnly": true}),
    ];
    let (host, port, database, user, password, ssl, ca) = match &pg.external {
        Some(e) => {
            if let Some(ca) = &e.ca_secret_ref {
                volumes.push(json!({"name": "postgresql-ca", "secret": {"secretName": ca.name, "items": [{"key": ca.key, "path": "ca.crt"}]}}));
                mounts.push(json!({"name": "postgresql-ca", "mountPath": "/etc/ward/postgresql-ca", "readOnly": true}));
            }
            (
                e.host.clone(),
                e.port,
                e.database.clone(),
                e.username.clone(),
                (
                    e.password_secret_ref.name.clone(),
                    e.password_secret_ref.key.clone(),
                ),
                e.ssl_mode,
                "/etc/ward/postgresql-ca/ca.crt",
            )
        }
        None => (
            n.postgresql_host(),
            5432,
            POSTGRESQL_USER.to_string(),
            POSTGRESQL_USER.to_string(),
            (n.postgresql_auth(), POSTGRESQL_PASSWORD_KEY.to_string()),
            SslMode::VerifyFull,
            "/etc/ward/tls/ca.crt",
        ),
    };
    volumes.push(json!({"name": "postgresql-auth", "secret": {"secretName": password.0, "items": [{"key": password.1, "path": "password"}]}}));
    let mut env = vec![
        json!({"name": "WARD_POSTGRESQL_HOST", "value": host}),
        json!({"name": "WARD_POSTGRESQL_PORT", "value": port.to_string()}),
        json!({"name": "WARD_POSTGRESQL_DATABASE", "value": database}),
        json!({"name": "WARD_POSTGRESQL_USER", "value": user}),
        json!({"name": "WARD_POSTGRESQL_PASSWORD_FILE", "value": "/etc/ward/postgresql/password"}),
    ];
    if ssl == SslMode::VerifyFull {
        env.push(json!({"name": "WARD_POSTGRESQL_CA_FILE", "value": ca}));
    }
    (env, volumes, mounts)
}

pub fn postgresql_service(ward: &Ward) -> Service {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.postgresql(), COMPONENT_POSTGRESQL),
        "spec": {
            "selector": selector(&n, COMPONENT_POSTGRESQL)["matchLabels"],
            "ports": [{"name": "postgresql", "port": POSTGRESQL_PORT, "targetPort": "postgresql"}],
        },
    }))
}

/// `pg_hba.conf` of the embedded server: TLS with a password over the
/// network, the socket only inside the container.
pub fn postgresql_config(ward: &Ward) -> ConfigMap {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.postgresql(), COMPONENT_POSTGRESQL),
        "data": {"pg_hba.conf": PG_HBA},
    }))
}

/// Only the manager reaches the embedded server.
pub fn postgresql_network_policy(ward: &Ward) -> NetworkPolicy {
    let n = Names::of(ward);
    typed(json!({
        "metadata": meta(ward, &n.postgresql(), COMPONENT_POSTGRESQL),
        "spec": {
            "podSelector": selector(&n, COMPONENT_POSTGRESQL),
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{"podSelector": selector(&n, COMPONENT_MANAGER)}],
                "ports": [{"protocol": "TCP", "port": POSTGRESQL_PORT}],
            }],
        },
    }))
}

pub fn postgresql_statefulset(ward: &Ward, d: &Defaults) -> Option<StatefulSet> {
    let n = Names::of(ward);
    let pg = ward.spec.postgresql.embedded()?;
    let image = pg
        .image
        .clone()
        .unwrap_or_else(|| d.postgresql_image.clone());
    let ready = json!({"exec": {"command": ["pg_isready", "-h", "/var/run/postgresql", "-U", POSTGRESQL_USER, "-d", POSTGRESQL_USER]}, "periodSeconds": 10});
    let mut live = ready.clone();
    live["failureThreshold"] = json!(6);
    let mut startup = ready.clone();
    startup["failureThreshold"] = json!(60);
    startup["periodSeconds"] = json!(5);
    let mut pvc_labels = labels(&n.name, COMPONENT_POSTGRESQL);
    pvc_labels.remove(crate::k8s::LABEL_MANAGED_BY);
    let read_only = 0o440;
    Some(typed(json!({
        "metadata": meta(ward, &n.postgresql(), COMPONENT_POSTGRESQL),
        "spec": {
            "serviceName": n.postgresql(),
            "replicas": 1,
            "selector": selector(&n, COMPONENT_POSTGRESQL),
            "persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Delete", "whenScaled": "Retain"},
            "template": {
                "metadata": {"labels": labels(&n.name, COMPONENT_POSTGRESQL)},
                "spec": {
                    "automountServiceAccountToken": false,
                    "imagePullSecrets": pull_secrets(&ward.spec),
                    "nodeSelector": pg.node_selector,
                    "tolerations": pg.tolerations,
                    "affinity": pg.affinity,
                    "securityContext": {"runAsNonRoot": true, "runAsUser": POSTGRESQL_UID, "runAsGroup": POSTGRESQL_UID, "fsGroup": POSTGRESQL_UID, "seccompProfile": {"type": "RuntimeDefault"}},
                    "terminationGracePeriodSeconds": 60,
                    "containers": [{
                        "name": "postgresql",
                        "image": image,
                        "imagePullPolicy": "IfNotPresent",
                        "args": [
                            "-c", "ssl=on",
                            "-c", "ssl_cert_file=/etc/ward/tls/tls.crt",
                            "-c", "ssl_key_file=/etc/ward/tls/tls.key",
                            "-c", "hba_file=/etc/ward/postgresql/pg_hba.conf",
                            "-c", "password_encryption=scram-sha-256",
                        ],
                        "env": [
                            {"name": "POSTGRES_USER", "value": POSTGRESQL_USER},
                            {"name": "POSTGRES_DB", "value": POSTGRESQL_USER},
                            {"name": "POSTGRES_PASSWORD_FILE", "value": "/etc/ward/auth/password"},
                            {"name": "PGDATA", "value": "/var/lib/postgresql/data/pgdata"},
                        ],
                        "ports": [{"name": "postgresql", "containerPort": POSTGRESQL_PORT}],
                        "startupProbe": startup,
                        "readinessProbe": ready,
                        "livenessProbe": live,
                        "resources": resources(pg.resources.as_ref(), "50m", "128Mi", "512Mi"),
                        "securityContext": {"allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": {"drop": ["ALL"]}},
                        "volumeMounts": [
                            {"name": "data", "mountPath": "/var/lib/postgresql/data"},
                            {"name": "run", "mountPath": "/var/run/postgresql"},
                            {"name": "tmp", "mountPath": "/tmp"},
                            {"name": "tls", "mountPath": "/etc/ward/tls", "readOnly": true},
                            {"name": "config", "mountPath": "/etc/ward/postgresql", "readOnly": true},
                            {"name": "auth", "mountPath": "/etc/ward/auth", "readOnly": true},
                        ],
                    }],
                    "volumes": [
                        {"name": "run", "emptyDir": {}},
                        {"name": "tmp", "emptyDir": {}},
                        {"name": "tls", "secret": {"secretName": n.postgresql_tls(), "defaultMode": read_only}},
                        {"name": "config", "configMap": {"name": n.postgresql()}},
                        {"name": "auth", "secret": {"secretName": n.postgresql_auth(), "defaultMode": read_only}},
                    ],
                },
            },
            "volumeClaimTemplates": [{
                "metadata": {"name": "data", "labels": pvc_labels},
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "storageClassName": pg.storage.storage_class_name,
                    "resources": {"requests": {"storage": pg.storage.size}},
                },
            }],
        },
    })))
}

/// The Gateway API `HTTPRoute` kind, which `k8s-openapi` does not carry.
pub fn http_route_resource() -> ApiResource {
    ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "HTTPRoute".into(),
        plural: "httproutes".into(),
    }
}

/// The `HTTPRoute` to the manager admin API, when the manager runs and the
/// route is enabled.
pub fn manager_http_route(ward: &Ward) -> Option<DynamicObject> {
    let n = Names::of(ward);
    let route = ward.spec.manager.http_route.as_ref()?;
    if !ward.spec.manager.enabled || !route.enabled {
        return None;
    }
    let mut m = meta(ward, &n.manager(), COMPONENT_MANAGER);
    for (k, v) in &route.labels {
        m["labels"][k] = json!(v);
    }
    m["annotations"] = json!(route.annotations);
    let parents: Vec<Value> = route
        .parent_refs
        .iter()
        .map(|p| {
            let mut r =
                json!({"group": p.group, "kind": p.kind, "name": p.name, "namespace": p.namespace});
            if let Some(s) = &p.section_name {
                r["sectionName"] = json!(s);
            }
            if let Some(port) = p.port {
                r["port"] = json!(port);
            }
            r
        })
        .collect();
    let mut obj = DynamicObject::new(&n.manager(), &http_route_resource()).within(&n.namespace);
    obj.metadata = typed(m);
    obj.data = json!({"spec": {
        "parentRefs": parents,
        "hostnames": route.hostnames,
        "rules": [{
            "matches": [{"path": {"type": "PathPrefix", "value": "/v1"}}],
            "backendRefs": [{"group": "", "kind": "Service", "name": n.manager(), "namespace": n.namespace, "port": ADMIN_PORT}],
        }],
    }});
    Some(obj)
}

fn update_strategy(s: Option<&AgentUpdateStrategy>) -> Value {
    s.map_or_else(
        || json!({"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": "10%"}}),
        |s| match (s.kind, &s.rolling_update) {
            (UpdateStrategyType::OnDelete, _) => json!({"type": "OnDelete"}),
            (UpdateStrategyType::RollingUpdate, None) => json!({"type": "RollingUpdate"}),
            (UpdateStrategyType::RollingUpdate, Some(r)) => {
                json!({"type": "RollingUpdate", "rollingUpdate": r})
            }
        },
    )
}

pub fn agent_daemonset(ward: &Ward, d: &Defaults) -> DaemonSet {
    let n = Names::of(ward);
    let spec = &ward.spec;
    let (img, pull) = image(spec, d);
    let (ready, live) = probes();
    let mut env = vec![
        json!({"name": "NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}),
        json!({"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}}),
        json!({"name": "WARD_INSTANCE", "value": n.name}),
    ];
    let mut volumes = vec![
        json!({"name": "policy", "configMap": {"name": n.policy()}}),
        json!({"name": "rollout", "configMap": {"name": n.rollout()}}),
        json!({"name": "state", "hostPath": {"path": format!("/var/lib/ward/{}-{}", n.namespace, n.name), "type": "DirectoryOrCreate"}}),
        json!({"name": "tracefs", "hostPath": {"path": "/sys/kernel/tracing"}}),
    ];
    let mut mounts = vec![
        json!({"name": "policy", "mountPath": "/etc/ward/policy", "readOnly": true}),
        json!({"name": "rollout", "mountPath": "/etc/ward/rollout", "readOnly": true}),
        json!({"name": "state", "mountPath": "/var/lib/ward"}),
        json!({"name": "tracefs", "mountPath": "/sys/kernel/tracing", "readOnly": true}),
    ];
    if spec.manager.enabled {
        env.push(json!({"name": "WARD_MANAGER_URL", "value": n.manager_url()}));
        volumes.push(json!({"name": "tls", "secret": {"secretName": n.agent_tls()}}));
        mounts.push(json!({"name": "tls", "mountPath": "/etc/ward/tls", "readOnly": true}));
        volumes.push(
            json!({"name": "manager-token", "projected": {"sources": [{"serviceAccountToken": {
                "audience": crate::manager::AUDIENCE,
                "expirationSeconds": 3600,
                "path": "token",
            }}]}}),
        );
        mounts.push(json!({"name": "manager-token", "mountPath": "/var/run/secrets/ward/manager", "readOnly": true}));
    } else if let Some(ps) = spec.feed.as_ref().and_then(|f| f.pull_secret_ref.as_ref()) {
        volumes.push(json!({"name": "registry", "secret": {"secretName": ps.name}}));
        mounts
            .push(json!({"name": "registry", "mountPath": "/etc/ward/registry", "readOnly": true}));
        env.push(
            json!({"name": "WARD_REGISTRY_AUTH", "value": "/etc/ward/registry/.dockerconfigjson"}),
        );
    }
    let tolerations = spec
        .agent
        .tolerations
        .clone()
        .map_or_else(|| json!([{"operator": "Exists"}]), |t| json!(t));
    let security = if spec.agent.privileged {
        json!({"privileged": true, "runAsUser": 0, "runAsNonRoot": false, "readOnlyRootFilesystem": true})
    } else {
        json!({
            "runAsUser": 0,
            "runAsNonRoot": false,
            "readOnlyRootFilesystem": true,
            "allowPrivilegeEscalation": false,
            "capabilities": {"drop": ["ALL"], "add": AGENT_CAPABILITIES},
            "seccompProfile": {"type": "Unconfined"},
            "appArmorProfile": {"type": "Unconfined"},
        })
    };
    typed(json!({
        "metadata": meta(ward, &n.agent(), COMPONENT_AGENT),
        "spec": {
            "selector": selector(&n, COMPONENT_AGENT),
            "updateStrategy": update_strategy(spec.agent.update_strategy.as_ref()),
            "template": {
                "metadata": {"labels": labels(&n.name, COMPONENT_AGENT)},
                "spec": {
                    "serviceAccountName": AGENT_SERVICE_ACCOUNT,
                    "hostPID": true,
                    "imagePullSecrets": pull_secrets(spec),
                    "priorityClassName": spec.agent.priority_class_name,
                    "affinity": spec.agent.affinity,
                    "dnsPolicy": spec.agent.dns_policy.map(DnsPolicy::as_str),
                    "dnsConfig": spec.agent.dns_config,
                    "nodeSelector": spec.agent.node_selector,
                    "tolerations": tolerations,
                    "terminationGracePeriodSeconds": 10,
                    "containers": [{
                        "name": "agent",
                        "image": img,
                        "imagePullPolicy": pull,
                        "args": ["agent"],
                        "env": env,
                        "ports": [{"name": "metrics", "containerPort": METRICS_PORT}],
                        "readinessProbe": ready,
                        "livenessProbe": live,
                        "resources": resources(spec.agent.resources.as_ref(), "50m", "128Mi", "512Mi"),
                        "securityContext": security,
                        "volumeMounts": mounts,
                    }],
                    "volumes": volumes,
                },
            },
        },
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;
    use crate::crd::{FeedSpec, LocalObjectRef, RolloutSpec};

    pub fn ward(spec: WardSpec) -> Ward {
        Ward {
            metadata: ObjectMeta {
                name: Some("default".into()),
                namespace: Some("ward-system".into()),
                uid: Some("uid-1".into()),
                ..ObjectMeta::default()
            },
            spec,
            status: None,
        }
    }

    pub fn defaults() -> Defaults {
        Defaults {
            image: "ghcr.io/younsl/ward:0.1.0".into(),
            image_pull_policy: "IfNotPresent".into(),
            workload_cluster_role: "ward-workload".into(),
            lease_cluster_role: "ward-agent-lease".into(),
            ward_namespace: "ward-system".into(),
            allowed_image_prefixes: Vec::new(),
            allow_privileged_agent: false,
            postgresql_image: "docker.io/library/postgres:18.6-alpine3.24".into(),
        }
    }

    #[test]
    fn agent_daemonset_with_and_without_manager() {
        let mut spec = WardSpec::default();
        let ds = serde_json::to_value(agent_daemonset(&ward(spec.clone()), &defaults())).unwrap();
        let pod = &ds["spec"]["template"]["spec"];
        assert_eq!(ds["metadata"]["name"], "default-agent");
        assert_eq!(pod["hostPID"], true);
        assert_eq!(pod["tolerations"][0]["operator"], "Exists");
        let c = &pod["containers"][0];
        assert_eq!(c["ports"][0]["name"], "metrics");
        assert_eq!(
            c["securityContext"]["capabilities"]["add"]
                .as_array()
                .unwrap()
                .len(),
            9
        );
        assert!(
            c["env"]
                .to_string()
                .contains("https://default-manager.ward-system.svc:8443")
        );
        assert_eq!(
            ds["spec"]["template"]["metadata"]["labels"]["app.kubernetes.io/name"],
            "ward"
        );
        assert_eq!(ds["metadata"]["ownerReferences"][0]["kind"], "Ward");

        spec.manager.enabled = false;
        spec.agent.privileged = true;
        spec.image_pull_secrets = vec![LocalObjectRef {
            name: "regcred".into(),
        }];
        spec.feed = Some(FeedSpec {
            image: "ghcr.io/x/ward-db:latest".into(),
            public_key: None,
            insecure_skip_verify: true,
            interval: "6h".into(),
            pull_secret_ref: Some(LocalObjectRef {
                name: "feed-cred".into(),
            }),
            rollout: RolloutSpec::default(),
        });
        let ds = serde_json::to_value(agent_daemonset(&ward(spec), &defaults())).unwrap();
        let pod = &ds["spec"]["template"]["spec"];
        assert_eq!(pod["containers"][0]["securityContext"]["privileged"], true);
        assert!(!pod.to_string().contains("WARD_MANAGER_URL"));
        assert!(pod.to_string().contains("feed-cred"));
        assert_eq!(pod["imagePullSecrets"][0]["name"], "regcred");
        assert_eq!(
            ds["spec"]["updateStrategy"]["rollingUpdate"]["maxUnavailable"],
            "10%"
        );
    }

    #[test]
    fn agent_scheduling_and_update_strategy() {
        use crate::crd::AgentRollingUpdate;
        use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

        let mut spec = WardSpec::default();
        spec.agent.affinity = Some(serde_json::from_value(serde_json::json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchExpressions": [{"key": "eks.amazonaws.com/compute-type", "operator": "NotIn", "values": ["fargate"]}]}]}}})).unwrap());
        spec.agent.update_strategy = Some(AgentUpdateStrategy {
            kind: UpdateStrategyType::RollingUpdate,
            rolling_update: Some(AgentRollingUpdate {
                max_unavailable: Some(IntOrString::Int(0)),
                max_surge: Some(IntOrString::String("25%".into())),
            }),
        });
        let ds = serde_json::to_value(agent_daemonset(&ward(spec.clone()), &defaults())).unwrap();
        assert_eq!(
            ds["spec"]["template"]["spec"]["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
                ["nodeSelectorTerms"][0]["matchExpressions"][0]["values"][0],
            "fargate"
        );
        assert_eq!(
            ds["spec"]["updateStrategy"],
            serde_json::json!({"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 0, "maxSurge": "25%"}})
        );
        spec.agent.update_strategy = Some(AgentUpdateStrategy {
            kind: UpdateStrategyType::OnDelete,
            rolling_update: None,
        });
        let ds = serde_json::to_value(agent_daemonset(&ward(spec.clone()), &defaults())).unwrap();
        assert_eq!(
            ds["spec"]["updateStrategy"],
            serde_json::json!({"type": "OnDelete"})
        );
        spec.agent.update_strategy = Some(AgentUpdateStrategy::default());
        let ds = serde_json::to_value(agent_daemonset(&ward(spec), &defaults())).unwrap();
        assert_eq!(
            ds["spec"]["updateStrategy"],
            serde_json::json!({"type": "RollingUpdate"})
        );
    }

    #[test]
    fn dns_settings_reach_both_pods() {
        let mut spec = WardSpec::default();
        let dns: k8s_openapi::api::core::v1::PodDNSConfig = serde_json::from_value(
            serde_json::json!({"nameservers": ["10.0.0.10"], "options": [{"name": "ndots", "value": "2"}]}),
        )
        .unwrap();
        spec.manager.dns_policy = Some(DnsPolicy::None);
        spec.manager.dns_config = Some(dns.clone());
        spec.agent.dns_policy = Some(DnsPolicy::ClusterFirst);
        spec.agent.dns_config = Some(dns);
        let w = ward(spec);
        let sts = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        let ds = serde_json::to_value(agent_daemonset(&w, &defaults())).unwrap();
        assert_eq!(sts["spec"]["template"]["spec"]["dnsPolicy"], "None");
        assert_eq!(
            sts["spec"]["template"]["spec"]["dnsConfig"]["nameservers"][0],
            "10.0.0.10"
        );
        assert_eq!(ds["spec"]["template"]["spec"]["dnsPolicy"], "ClusterFirst");
        assert_eq!(
            ds["spec"]["template"]["spec"]["dnsConfig"]["options"][0]["name"],
            "ndots"
        );
        let plain =
            serde_json::to_value(agent_daemonset(&ward(WardSpec::default()), &defaults())).unwrap();
        assert!(plain["spec"]["template"]["spec"].get("dnsPolicy").is_none());
    }

    #[test]
    fn manager_http_route_targets_the_admin_api() {
        let mut spec = WardSpec::default();
        assert!(manager_http_route(&ward(spec.clone())).is_none());
        spec.manager.http_route = Some(crate::crd::HttpRouteSpec {
            enabled: true,
            parent_refs: vec![crate::crd::ParentRef {
                group: "gateway.networking.k8s.io".into(),
                kind: "Gateway".into(),
                name: "public".into(),
                namespace: "gateway-system".into(),
                section_name: Some("https".into()),
                port: None,
            }],
            hostnames: vec!["ward.example.com".into()],
            labels: BTreeMap::from([("team".into(), "security".into())]),
            annotations: BTreeMap::from([("example.com/owner".into(), "sec".into())]),
        });
        let route = serde_json::to_value(manager_http_route(&ward(spec.clone())).unwrap()).unwrap();
        assert_eq!(route["apiVersion"], "gateway.networking.k8s.io/v1");
        assert_eq!(route["kind"], "HTTPRoute");
        assert_eq!(route["metadata"]["labels"]["team"], "security");
        assert_eq!(
            route["metadata"]["labels"]["app.kubernetes.io/component"],
            "manager"
        );
        assert_eq!(route["metadata"]["annotations"]["example.com/owner"], "sec");
        let spec_json = &route["spec"];
        assert_eq!(spec_json["parentRefs"][0]["namespace"], "gateway-system");
        assert_eq!(spec_json["parentRefs"][0]["sectionName"], "https");
        assert!(spec_json["parentRefs"][0].get("port").is_none());
        assert_eq!(spec_json["hostnames"][0], "ward.example.com");
        let backend = &spec_json["rules"][0]["backendRefs"][0];
        assert_eq!(backend["kind"], "Service");
        assert_eq!(backend["group"], "");
        assert_eq!(backend["name"], "default-manager");
        assert_eq!(backend["namespace"], "ward-system");
        assert_eq!(backend["port"], ADMIN_PORT);
        assert_eq!(spec_json["rules"][0]["matches"][0]["path"]["value"], "/v1");
        spec.manager.enabled = false;
        assert!(manager_http_route(&ward(spec)).is_none());
    }

    #[test]
    fn postgresql_embedded_and_external() {
        let env = |sts: &Value| -> BTreeMap<String, String> {
            sts["spec"]["template"]["spec"]["containers"][0]["env"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|e| {
                    Some((
                        e["name"].as_str()?.to_string(),
                        e["value"].as_str()?.to_string(),
                    ))
                })
                .collect()
        };
        let mut spec = WardSpec::default();
        let w = ward(spec.clone());
        assert!(postgresql_statefulset(&w, &defaults()).is_none());
        let plain = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        assert!(!env(&plain).contains_key("WARD_POSTGRESQL_HOST"));

        spec.postgresql.enabled = true;
        let w = ward(spec.clone());
        let manager = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        let e = env(&manager);
        assert_eq!(
            e["WARD_POSTGRESQL_HOST"],
            "default-postgresql.ward-system.svc"
        );
        assert_eq!(e["WARD_POSTGRESQL_CA_FILE"], "/etc/ward/tls/ca.crt");
        let pg = serde_json::to_value(postgresql_statefulset(&w, &defaults()).unwrap()).unwrap();
        let pod = &pg["spec"]["template"]["spec"];
        assert_eq!(pod["containers"][0]["image"], defaults().postgresql_image);
        assert_eq!(pod["securityContext"]["runAsUser"], 70);
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(
            pod["volumes"][2]["secret"]["secretName"],
            "default-postgresql-tls"
        );
        assert_eq!(pod["volumes"][2]["secret"]["defaultMode"], 0o440);
        assert_eq!(
            pg["spec"]["volumeClaimTemplates"][0]["spec"]["resources"]["requests"]["storage"],
            "8Gi"
        );
        assert_eq!(
            pg["spec"]["persistentVolumeClaimRetentionPolicy"]["whenDeleted"],
            "Delete"
        );
        let svc = serde_json::to_value(postgresql_service(&w)).unwrap();
        assert_eq!(
            svc["spec"]["selector"]["app.kubernetes.io/component"],
            "postgresql"
        );
        let np = serde_json::to_value(postgresql_network_policy(&w)).unwrap();
        assert_eq!(
            np["spec"]["ingress"][0]["from"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/component"],
            "manager"
        );
        let hba = serde_json::to_value(postgresql_config(&w)).unwrap();
        assert!(
            hba["data"]["pg_hba.conf"]
                .as_str()
                .unwrap()
                .contains("hostssl")
        );

        spec.postgresql.external = Some(crate::crd::ExternalPostgresql {
            host: "db.example.com".into(),
            port: 6432,
            database: "sec".into(),
            username: "ward_rw".into(),
            password_secret_ref: crate::crd::SecretKeyRef {
                name: "db".into(),
                key: "pw".into(),
            },
            ssl_mode: SslMode::Disable,
            ca_secret_ref: None,
        });
        let w = ward(spec);
        assert!(postgresql_statefulset(&w, &defaults()).is_none());
        let manager = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        let e = env(&manager);
        assert_eq!(e["WARD_POSTGRESQL_HOST"], "db.example.com");
        assert_eq!(e["WARD_POSTGRESQL_PORT"], "6432");
        assert_eq!(e["WARD_POSTGRESQL_USER"], "ward_rw");
        assert!(!e.contains_key("WARD_POSTGRESQL_CA_FILE"));
        let vols = manager["spec"]["template"]["spec"]["volumes"]
            .as_array()
            .unwrap();
        let auth = vols
            .iter()
            .find(|v| v["name"] == "postgresql-auth")
            .unwrap();
        assert_eq!(auth["secret"]["items"][0]["key"], "pw");
    }

    #[test]
    fn manager_objects() {
        let mut spec = WardSpec {
            feed: Some(FeedSpec {
                image: "ghcr.io/x/ward-db:latest".into(),
                public_key: None,
                insecure_skip_verify: true,
                interval: "6h".into(),
                pull_secret_ref: Some(LocalObjectRef {
                    name: "feed-cred".into(),
                }),
                rollout: RolloutSpec::default(),
            }),
            ..WardSpec::default()
        };
        spec.manager.node_selector = BTreeMap::from([("pool".into(), "system".into())]);
        spec.manager.tolerations = vec![serde_json::from_value(serde_json::json!({"key": "dedicated", "operator": "Equal", "value": "system", "effect": "NoSchedule"})).unwrap()];
        spec.manager.affinity = Some(serde_json::from_value(serde_json::json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchExpressions": [{"key": "kubernetes.io/arch", "operator": "In", "values": ["arm64"]}]}]}}})).unwrap());
        spec.manager.topology_spread_constraints = vec![serde_json::from_value(serde_json::json!({"maxSkew": 1, "topologyKey": "topology.kubernetes.io/zone", "whenUnsatisfiable": "ScheduleAnyway", "labelSelector": {"matchLabels": {"app.kubernetes.io/component": "manager"}}})).unwrap()];
        let w = ward(spec);
        let sts = serde_json::to_value(manager_statefulset(&w, &defaults())).unwrap();
        let pod = &sts["spec"]["template"]["spec"];
        assert_eq!(pod["nodeSelector"]["pool"], "system");
        assert_eq!(pod["tolerations"][0]["key"], "dedicated");
        assert_eq!(
            pod["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
                [0]["matchExpressions"][0]["values"][0],
            "arm64"
        );
        assert_eq!(
            pod["topologySpreadConstraints"][0]["topologyKey"],
            "topology.kubernetes.io/zone"
        );
        assert_eq!(sts["spec"]["serviceName"], "default-manager");
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["spec"]["resources"]["requests"]["storage"],
            "1Gi"
        );
        assert_eq!(
            sts["spec"]["volumeClaimTemplates"][0]["metadata"]["labels"]["app.kubernetes.io/component"],
            "manager"
        );
        assert!(sts.to_string().contains("WARD_REGISTRY_AUTH"));
        assert!(
            sts.to_string()
                .contains(r#"{"fieldPath":"metadata.namespace"}"#),
            "the manager needs its namespace to accept agent tokens"
        );
        let svc = serde_json::to_value(manager_service(&w)).unwrap();
        assert_eq!(svc["spec"]["clusterIP"], "None");
        assert_eq!(svc["spec"]["ports"].as_array().unwrap().len(), 3);
        let n = Names::of(&w);
        assert_eq!(n.cluster_role_binding(), "ward-ward-system-default");
        assert!(
            n.manager_dns()
                .contains(&"default-manager.ward-system.svc".to_string())
        );
        let crb = serde_json::to_value(cluster_role_binding(&w, &defaults())).unwrap();
        assert_eq!(crb["roleRef"]["name"], "ward-workload");
        assert_eq!(crb["subjects"][0]["namespace"], "ward-system");
        let rb = serde_json::to_value(lease_role_binding(&w, &defaults())).unwrap();
        assert_eq!(rb["roleRef"]["kind"], "ClusterRole");
        let s = secret(
            &w,
            &n.api_token(),
            &BTreeMap::from([("token".into(), "x".into())]),
            &BTreeMap::new(),
        );
        assert_eq!(s.metadata.name.as_deref(), Some("default-api-token"));
        assert_eq!(policy_map(&w, "{}").data.unwrap()[POLICY_FILE], "{}");
        assert_eq!(
            rollout_map(&w, "{}").metadata.name.as_deref(),
            Some("default-rollout")
        );
        assert_eq!(
            service_account(&w).metadata.name.as_deref(),
            Some("ward-agent")
        );
        for name in [n.ca(), n.manager_tls(), n.agent_tls(), n.agent()] {
            assert!(name.starts_with("default-"));
        }
    }
}
