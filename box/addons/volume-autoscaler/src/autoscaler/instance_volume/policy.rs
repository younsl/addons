//! Compiles `InstanceVolumeAutoscaler` resources into effective resize
//! settings and assigns discovered instances to them. An instance no
//! autoscaler targets is left alone. As with overlapping
//! `HorizontalPodAutoscalers`, an autoscaler that shares an instance with
//! another refuses to scale any of its instances until the overlap is removed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::autoscaler::instance_volume::crd::{InstanceVolumeAutoscaler, TargetKind};
use crate::autoscaler::{
    DEFAULT_UTILIZATION_PERCENT, MIN_COOLDOWN, SelectPolicy, compile_steps, parse_cooldown,
    quantity_bytes, quantity_gib,
};
use crate::awsx::Instance;

pub use crate::autoscaler::Step;

/// The fully resolved resize settings of one autoscaler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// The autoscaler name, used as the `policy` label in logs and metrics.
    pub policy: String,
    pub uid: String,
    pub generation: Option<i64>,
    /// `behavior.scaleUp.selectPolicy: Disabled`. The resizer must not touch
    /// the targeted instances.
    pub paused: bool,
    /// When false, suppresses Alertmanager alerts. Only consulted when
    /// alerting is globally enabled.
    pub alert_enabled: bool,
    pub usage_threshold_percent: i32,
    /// Picks the step from `steps`: the largest unless `Min`.
    pub select_min: bool,
    pub steps: Vec<Step>,
    pub max_volume_size_gib: i32,
    /// The minimum time between two modifications of a target volume.
    pub cooldown: Duration,
}

impl Effective {
    /// Settings equal to an autoscaler with every optional field omitted.
    #[cfg(test)]
    pub(crate) fn defaults(name: &str, max_gib: i32) -> Self {
        use crate::autoscaler::{DEFAULT_STEP_GIB, DEFAULT_STEP_PERCENT};
        Self {
            policy: name.into(),
            uid: format!("uid-{name}"),
            generation: Some(1),
            paused: false,
            alert_enabled: true,
            usage_threshold_percent: DEFAULT_UTILIZATION_PERCENT,
            select_min: false,
            steps: vec![
                Step::Percent(DEFAULT_STEP_PERCENT),
                Step::GiB(DEFAULT_STEP_GIB),
            ],
            max_volume_size_gib: max_gib,
            cooldown: MIN_COOLDOWN,
        }
    }
}

/// The target of one autoscaler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Instance(String),
    Tags(BTreeMap<String, String>),
}

impl Target {
    #[must_use]
    pub fn matches(&self, inst: &Instance) -> bool {
        match self {
            Self::Instance(id) => inst.id == *id,
            Self::Tags(tags) => tags.iter().all(|(k, v)| inst.tags.get(k) == Some(v)),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Instance(id) => write!(f, "Instance/{id}"),
            Self::Tags(tags) => {
                let pairs: Vec<String> = tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
                write!(f, "TagSelector/{}", pairs.join(","))
            }
        }
    }
}

/// A validated autoscaler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub target: Target,
    pub effective: Effective,
}

/// Validates one autoscaler. The CRD schema rejects most of these up front,
/// but a resource written before a schema change, or with validation off,
/// still reaches the controller.
pub fn compile(iva: &InstanceVolumeAutoscaler) -> Result<Compiled, String> {
    let name = iva.metadata.name.clone().unwrap_or_default();
    let spec = &iva.spec;
    let r = &spec.scale_target_ref;
    let target = match r.kind {
        TargetKind::Instance => {
            if r.tags.is_some() {
                return Err("scaleTargetRef.tags is not allowed with kind Instance".into());
            }
            match r.name.as_deref() {
                Some(id) if is_instance_id(id) => Target::Instance(id.to_string()),
                Some(id) => {
                    return Err(format!(
                        "scaleTargetRef.name {id:?} is not an EC2 instance ID"
                    ));
                }
                None => return Err("scaleTargetRef.name is required with kind Instance".into()),
            }
        }
        TargetKind::TagSelector => {
            if r.name.is_some() {
                return Err("scaleTargetRef.name is not allowed with kind TagSelector".into());
            }
            let tags = r.tags.clone().unwrap_or_default();
            if tags.is_empty() {
                return Err("scaleTargetRef.tags is required with kind TagSelector".into());
            }
            if tags.iter().any(|(k, v)| k.is_empty() || v.is_empty()) {
                return Err("scaleTargetRef.tags entries need a non-empty key and value".into());
            }
            if tags.len() > 50 || tags.iter().any(|(k, v)| k.len() > 128 || v.len() > 256) {
                return Err("scaleTargetRef.tags allows up to 50 tags with keys up to 128 and values up to 256 characters".into());
            }
            Target::Tags(tags)
        }
    };

    let max = quantity_gib(&spec.max_capacity).map_err(|e| format!("maxCapacity: {e}"))?;
    if quantity_bytes(&spec.max_capacity).unwrap_or_default() < 1_073_741_824.0 {
        return Err("maxCapacity must be at least 1Gi".into());
    }

    let threshold = spec
        .metrics
        .first()
        .map_or(DEFAULT_UTILIZATION_PERCENT, |m| {
            m.resource.target.average_utilization
        });
    if spec.metrics.len() > 1 {
        return Err("metrics supports a single storage metric".into());
    }
    if !(1..=100).contains(&threshold) {
        return Err(format!(
            "averageUtilization must be between 1 and 100, got {threshold}"
        ));
    }

    let rules = spec
        .behavior
        .as_ref()
        .and_then(|b| b.scale_up.clone())
        .unwrap_or_default();
    let select = rules.select_policy;
    let steps = compile_steps(&rules.policies)?;

    let cooldown = cooldown_of(rules.cooldown_duration.as_deref())?;

    Ok(Compiled {
        target,
        effective: Effective {
            policy: name,
            uid: iva.metadata.uid.clone().unwrap_or_default(),
            generation: iva.metadata.generation,
            paused: select == SelectPolicy::Disabled,
            alert_enabled: spec.alert_enabled,
            usage_threshold_percent: threshold,
            select_min: select == SelectPolicy::Min,
            steps,
            max_volume_size_gib: max,
            cooldown,
        },
    })
}

/// The cooldown an autoscaler asks for, `MIN_COOLDOWN` when unset.
fn cooldown_of(raw: Option<&str>) -> Result<Duration, String> {
    let Some(raw) = raw else {
        return Ok(MIN_COOLDOWN);
    };
    let d = parse_cooldown(raw).map_err(|e| format!("behavior.scaleUp.cooldownDuration: {e}"))?;
    if d < MIN_COOLDOWN {
        return Err(format!(
            "behavior.scaleUp.cooldownDuration {raw:?} is shorter than 6h, which keeps a volume within the EBS limit of four modifications in 24 hours"
        ));
    }
    Ok(d)
}

fn is_instance_id(s: &str) -> bool {
    s.strip_prefix("i-").is_some_and(|hex| {
        (8..=17).contains(&hex.len())
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The autoscalers of one pass, split into usable and rejected.
#[derive(Debug, Default)]
pub struct Resolver {
    /// Sorted by name.
    pub autoscalers: Vec<Compiled>,
    /// `(name, error)` of every autoscaler that failed validation.
    pub invalid: Vec<(String, String)>,
}

/// How one pass's instances map onto its autoscalers.
#[derive(Debug, Default)]
pub struct Plan {
    /// Instance index and the settings it is reconciled with.
    pub managed: Vec<(usize, Effective)>,
    /// Instance index and every autoscaler that targets it, for instances
    /// held back by an ambiguous autoscaler.
    pub refused: Vec<(usize, Vec<String>)>,
    /// Instances no autoscaler targets.
    pub unmanaged: usize,
    /// Each ambiguous autoscaler and the others it overlaps with.
    pub conflicts: BTreeMap<String, BTreeSet<String>>,
}

impl Resolver {
    #[must_use]
    pub fn new(ivas: &[InstanceVolumeAutoscaler]) -> Self {
        let mut r = Self::default();
        for iva in ivas {
            match compile(iva) {
                Ok(c) => r.autoscalers.push(c),
                Err(err) => r
                    .invalid
                    .push((iva.metadata.name.clone().unwrap_or_default(), err)),
            }
        }
        r.autoscalers
            .sort_by(|a, b| a.effective.policy.cmp(&b.effective.policy));
        r
    }

    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.autoscalers
            .iter()
            .map(|c| c.effective.policy.clone())
            .collect()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Compiled> {
        self.autoscalers.iter().find(|c| c.effective.policy == name)
    }

    /// Every autoscaler targeting `inst`, by name order.
    #[must_use]
    pub fn matching(&self, inst: &Instance) -> Vec<&Compiled> {
        self.autoscalers
            .iter()
            .filter(|c| c.target.matches(inst))
            .collect()
    }

    /// Assigns each instance to the single autoscaler targeting it. An
    /// autoscaler sharing any instance with another is ambiguous, and every
    /// instance it targets is refused.
    #[must_use]
    pub fn plan(&self, instances: &[Instance]) -> Plan {
        let matches: Vec<Vec<&Compiled>> = instances.iter().map(|i| self.matching(i)).collect();
        let mut plan = Plan::default();
        for m in &matches {
            if m.len() < 2 {
                continue;
            }
            for c in m {
                let others = m
                    .iter()
                    .filter(|o| o.effective.policy != c.effective.policy)
                    .map(|o| o.effective.policy.clone());
                plan.conflicts
                    .entry(c.effective.policy.clone())
                    .or_default()
                    .extend(others);
            }
        }
        for (i, m) in matches.into_iter().enumerate() {
            match m.as_slice() {
                [] => plan.unmanaged += 1,
                [c] if !plan.conflicts.contains_key(&c.effective.policy) => {
                    plan.managed.push((i, c.effective.clone()));
                }
                _ => plan
                    .refused
                    .push((i, m.iter().map(|c| c.effective.policy.clone()).collect())),
            }
        }
        plan
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use kube::api::ObjectMeta;

    use super::*;
    use crate::autoscaler::instance_volume::crd::{
        InstanceVolumeAutoscalerSpec, MetricSpec, ResourceMetricSource, ResourceName,
        ScaleTargetRef,
    };
    use crate::autoscaler::instance_volume::resizer::tests::instance;
    use crate::autoscaler::{
        Behavior, MetricSourceType, MetricTarget, MetricTargetType, ScalingPolicy,
        ScalingPolicyType, ScalingRules,
    };

    pub(crate) fn iva(name: &str, target: ScaleTargetRef) -> InstanceVolumeAutoscaler {
        InstanceVolumeAutoscaler {
            metadata: ObjectMeta {
                name: Some(name.into()),
                uid: Some(format!("uid-{name}")),
                generation: Some(1),
                ..ObjectMeta::default()
            },
            spec: InstanceVolumeAutoscalerSpec {
                scale_target_ref: target,
                max_capacity: Quantity("1000Gi".into()),
                metrics: Vec::new(),
                behavior: None,
                alert_enabled: true,
            },
            status: None,
        }
    }

    pub(crate) fn by_id(id: &str) -> ScaleTargetRef {
        ScaleTargetRef {
            kind: TargetKind::Instance,
            name: Some(id.into()),
            tags: None,
        }
    }

    pub(crate) fn by_tags(pairs: &[(&str, &str)]) -> ScaleTargetRef {
        ScaleTargetRef {
            kind: TargetKind::TagSelector,
            name: None,
            tags: Some(
                pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            ),
        }
    }

    pub(crate) fn with_utilization(
        mut iva: InstanceVolumeAutoscaler,
        pct: i32,
    ) -> InstanceVolumeAutoscaler {
        iva.spec.metrics = vec![MetricSpec {
            type_: MetricSourceType::Resource,
            resource: ResourceMetricSource {
                name: ResourceName::Storage,
                target: MetricTarget {
                    type_: MetricTargetType::Utilization,
                    average_utilization: pct,
                },
            },
        }];
        iva
    }

    pub(crate) fn with_scale_up(
        mut iva: InstanceVolumeAutoscaler,
        select: Option<SelectPolicy>,
        policies: &[(ScalingPolicyType, i32)],
    ) -> InstanceVolumeAutoscaler {
        iva.spec.behavior = Some(Behavior {
            scale_up: Some(ScalingRules {
                select_policy: select.unwrap_or_default(),
                cooldown_duration: None,
                policies: policies
                    .iter()
                    .map(|(t, v)| ScalingPolicy {
                        type_: *t,
                        value: *v,
                    })
                    .collect(),
            }),
        });
        iva
    }

    #[test]
    fn compile_defaults_and_overrides() {
        let c = compile(&iva("a", by_id("i-0123456789abcdef0"))).unwrap();
        assert_eq!(c.target, Target::Instance("i-0123456789abcdef0".into()));
        assert_eq!(c.effective, Effective::defaults("a", 1000));
        assert_eq!(c.target.to_string(), "Instance/i-0123456789abcdef0");

        let mut x = with_scale_up(
            with_utilization(iva("b", by_tags(&[("Role", "db"), ("Env", "prd")])), 70),
            Some(SelectPolicy::Min),
            &[
                (ScalingPolicyType::GiB, 50),
                (ScalingPolicyType::Percent, 20),
            ],
        );
        x.spec.alert_enabled = false;
        x.spec.max_capacity = Quantity("2Ti".into());
        let c = compile(&x).unwrap();
        assert_eq!(c.target.to_string(), "TagSelector/Env=prd,Role=db");
        let e = c.effective;
        assert_eq!(e.usage_threshold_percent, 70);
        assert!(e.select_min);
        assert!(!e.alert_enabled);
        assert_eq!(e.steps, vec![Step::GiB(50), Step::Percent(20)]);
        assert_eq!(e.max_volume_size_gib, 2048);

        let d = with_scale_up(
            iva("c", by_id("i-12345678")),
            Some(SelectPolicy::Disabled),
            &[],
        );
        assert!(compile(&d).unwrap().effective.paused);
    }

    #[test]
    fn compile_errors() {
        let mut both = by_id("i-12345678");
        both.tags = Some(BTreeMap::from([("a".into(), "b".into())]));
        let mut tag_named = by_tags(&[("a", "b")]);
        tag_named.name = Some("i-12345678".into());
        let mut no_name = by_id("x");
        no_name.name = None;
        let cases: Vec<(InstanceVolumeAutoscaler, &str)> = vec![
            (iva("a", both), "tags is not allowed"),
            (iva("a", by_id("web-1")), "not an EC2 instance ID"),
            (iva("a", by_id("i-XYZ12345")), "not an EC2 instance ID"),
            (iva("a", no_name), "name is required"),
            (iva("a", tag_named), "name is not allowed"),
            (iva("a", by_tags(&[])), "tags is required"),
            (iva("a", by_tags(&[("k", "")])), "non-empty key and value"),
            (
                {
                    let mut x = iva("a", by_id("i-12345678"));
                    x.spec.max_capacity = Quantity("10GB".into());
                    x
                },
                "maxCapacity",
            ),
            (
                {
                    let mut x = iva("a", by_id("i-12345678"));
                    x.spec.max_capacity = Quantity("0".into());
                    x
                },
                "at least 1Gi",
            ),
            (
                with_utilization(iva("a", by_id("i-12345678")), 0),
                "between 1 and 100",
            ),
            (
                {
                    let mut x = with_utilization(iva("a", by_id("i-12345678")), 50);
                    x.spec.metrics.push(x.spec.metrics[0].clone());
                    x
                },
                "single storage metric",
            ),
            (
                with_scale_up(
                    iva("a", by_id("i-12345678")),
                    None,
                    &[(ScalingPolicyType::GiB, 0)],
                ),
                "must be positive",
            ),
        ];
        for (x, want) in cases {
            let err = compile(&x).unwrap_err();
            assert!(err.contains(want), "want {want:?}, got {err}");
        }
    }

    #[test]
    fn plan_assigns_refuses_and_skips() {
        let mut db1 = instance("i-0000000a", 100);
        db1.tags.insert("Role".into(), "db".into());
        let mut db2 = instance("i-0000000b", 100);
        db2.tags.insert("Role".into(), "db".into());
        let web = instance("i-0000000c", 100);
        let lone = instance("i-0000000d", 100);
        let mut bad = iva("broken", by_id("nope"));
        bad.metadata.uid = None;
        let r = Resolver::new(&[
            iva("pinned", by_id("i-0000000a")),
            iva("db", by_tags(&[("Role", "db")])),
            iva("web", by_id("i-0000000c")),
            bad,
        ]);
        assert_eq!(r.names(), vec!["db", "pinned", "web"]);
        assert_eq!(r.invalid.len(), 1);
        assert_eq!(r.invalid[0].0, "broken");
        assert!(r.get("web").is_some());
        assert!(r.get("broken").is_none());

        let plan = r.plan(&[db1, db2, web, lone]);
        assert_eq!(plan.unmanaged, 1);
        assert_eq!(plan.managed.len(), 1);
        assert_eq!(plan.managed[0].0, 2);
        assert_eq!(plan.managed[0].1.policy, "web");
        assert_eq!(
            plan.refused,
            vec![
                (0, vec!["db".to_string(), "pinned".to_string()]),
                (1, vec!["db".to_string()]),
            ],
            "the whole ambiguous autoscaler is held back"
        );
        assert_eq!(
            plan.conflicts,
            BTreeMap::from([
                ("db".to_string(), BTreeSet::from(["pinned".to_string()])),
                ("pinned".to_string(), BTreeSet::from(["db".to_string()])),
            ])
        );
    }

    #[test]
    fn example_manifests_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let ivas: Vec<InstanceVolumeAutoscaler> = ["instance.yaml", "tag-selector.yaml"]
            .iter()
            .map(|f| serde_yaml::from_str(&std::fs::read_to_string(dir.join(f)).unwrap()).unwrap())
            .collect();
        let r = Resolver::new(&ivas);
        assert!(r.invalid.is_empty(), "{:?}", r.invalid);
        assert_eq!(r.names(), vec!["bastion", "database"]);
        assert_eq!(
            r.get("database").unwrap().effective.max_volume_size_gib,
            2048
        );
        assert_eq!(r.get("bastion").unwrap().effective.cooldown, MIN_COOLDOWN);
        assert_eq!(
            r.get("database").unwrap().effective.cooldown,
            Duration::from_mins(390)
        );
    }

    /// Parses one YAML manifest the way a user writes it for `kubectl apply`.
    pub(crate) fn from_yaml(raw: &str) -> InstanceVolumeAutoscaler {
        serde_yaml::from_str(raw).unwrap()
    }

    #[test]
    fn yaml_manifests_compile_to_effective_settings() {
        let minimal = from_yaml(indoc::indoc! {"
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: bastion
            spec:
              scaleTargetRef:
                kind: Instance
                name: i-0123456789abcdef0
              maxCapacity: 100Gi
        "});
        let c = compile(&minimal).unwrap();
        assert_eq!(c.target, Target::Instance("i-0123456789abcdef0".into()));
        let e = c.effective;
        assert_eq!(e.policy, "bastion");
        assert_eq!(e.usage_threshold_percent, 80, "HPA-style default target");
        assert_eq!(e.steps, vec![Step::Percent(10), Step::GiB(1)]);
        assert!(!e.select_min && !e.paused && e.alert_enabled);
        assert_eq!(e.max_volume_size_gib, 100);

        let full = from_yaml(indoc::indoc! {"
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: database
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Role: database
                  Env: prod
              maxCapacity: 1.5Ti
              metrics:
                - type: Resource
                  resource:
                    name: storage
                    target:
                      type: Utilization
                      averageUtilization: 65
              behavior:
                scaleUp:
                  selectPolicy: Min
                  policies:
                    - type: Percent
                      value: 25
                    - type: GiB
                      value: 40
              alertEnabled: false
        "});
        let c = compile(&full).unwrap();
        assert_eq!(c.target.to_string(), "TagSelector/Env=prod,Role=database");
        let e = c.effective;
        assert_eq!(e.usage_threshold_percent, 65);
        assert!(e.select_min);
        assert!(!e.alert_enabled);
        assert_eq!(e.steps, vec![Step::Percent(25), Step::GiB(40)]);
        assert_eq!(e.max_volume_size_gib, 1536);

        let disabled = from_yaml(indoc::indoc! {"
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: frozen
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Name: legacy-ftp
              maxCapacity: 500G
              behavior:
                scaleUp:
                  selectPolicy: Disabled
        "});
        let e = compile(&disabled).unwrap().effective;
        assert!(e.paused);
        assert_eq!(
            e.max_volume_size_gib, 466,
            "500 GB in whole GiB, rounded up"
        );
        assert_eq!(
            e.steps,
            vec![Step::Percent(10), Step::GiB(1)],
            "empty policies take defaults"
        );
    }

    #[test]
    fn yaml_manifests_the_controller_rejects() {
        let head = "apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1\nkind: InstanceVolumeAutoscaler\nmetadata:\n  name: x\nspec:\n";
        let cases = [
            (
                "  scaleTargetRef:\n    kind: Instance\n    name: i-0123456789abcdef0\n    tags:\n      Role: db\n  maxCapacity: 10Gi\n",
                "tags is not allowed",
            ),
            (
                "  scaleTargetRef:\n    kind: Instance\n    name: web-01\n  maxCapacity: 10Gi\n",
                "not an EC2 instance ID",
            ),
            (
                "  scaleTargetRef:\n    kind: Instance\n  maxCapacity: 10Gi\n",
                "name is required",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    name: i-0123456789abcdef0\n    tags:\n      Role: db\n  maxCapacity: 10Gi\n",
                "name is not allowed",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags: {}\n  maxCapacity: 10Gi\n",
                "tags is required",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      Role: \"\"\n  maxCapacity: 10Gi\n",
                "non-empty key and value",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      Role: db\n  maxCapacity: 10GB\n",
                "maxCapacity",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      Role: db\n  maxCapacity: 512Mi\n",
                "at least 1Gi",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      Role: db\n  maxCapacity: 100m\n",
                "at least 1Gi",
            ),
            (
                &format!(
                    "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      {}: db\n  maxCapacity: 10Gi\n",
                    "k".repeat(129)
                ),
                "up to 50 tags",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      Role: db\n  maxCapacity: 10Gi\n  behavior:\n    scaleUp:\n      policies:\n        - type: GiB\n          value: -5\n",
                "must be positive",
            ),
            (
                "  scaleTargetRef:\n    kind: TagSelector\n    tags:\n      Role: db\n  maxCapacity: 10Gi\n  metrics:\n    - type: Resource\n      resource:\n        name: storage\n        target:\n          type: Utilization\n          averageUtilization: 101\n",
                "between 1 and 100",
            ),
        ];
        for (spec, want) in cases {
            let iva = from_yaml(&format!("{head}{spec}"));
            let err = compile(&iva).unwrap_err();
            assert!(err.contains(want), "{spec}\nwant {want:?}, got {err}");
        }
    }

    #[test]
    fn yaml_manifests_that_do_not_parse() {
        let head = "apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1\nkind: InstanceVolumeAutoscaler\nmetadata:\n  name: x\nspec:\n";
        for spec in [
            "  scaleTargetRef:\n    kind: Deployment\n    name: web\n  maxCapacity: 10Gi\n",
            "  scaleTargetRef:\n    kind: Instance\n    name: i-0123456789abcdef0\n",
            "  maxCapacity: 10Gi\n",
            "  scaleTargetRef:\n    kind: Instance\n    name: i-0123456789abcdef0\n  maxCapacity: 10Gi\n  behavior:\n    scaleUp:\n      selectPolicy: Average\n",
            "  scaleTargetRef:\n    kind: Instance\n    name: i-0123456789abcdef0\n  maxCapacity: 10Gi\n  behavior:\n    scaleUp:\n      policies:\n        - type: Pods\n          value: 4\n",
            "  scaleTargetRef:\n    kind: Instance\n    name: i-0123456789abcdef0\n  maxCapacity: 10Gi\n  metrics:\n    - type: Resource\n      resource:\n        name: cpu\n        target:\n          type: Utilization\n          averageUtilization: 50\n",
        ] {
            let raw = format!("{head}{spec}");
            assert!(
                serde_yaml::from_str::<InstanceVolumeAutoscaler>(&raw).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn yaml_multi_document_overlap_is_ambiguous() {
        let raw = indoc::indoc! {"
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: by-id
            spec:
              scaleTargetRef:
                kind: Instance
                name: i-0000000a
              maxCapacity: 100Gi
            ---
            apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
            kind: InstanceVolumeAutoscaler
            metadata:
              name: by-tag
            spec:
              scaleTargetRef:
                kind: TagSelector
                tags:
                  Managed: \"true\"
              maxCapacity: 100Gi
        "};
        let ivas: Vec<InstanceVolumeAutoscaler> = serde_yaml::Deserializer::from_str(raw)
            .map(|d| serde::Deserialize::deserialize(d).unwrap())
            .collect();
        let plan =
            Resolver::new(&ivas).plan(&[instance("i-0000000a", 10), instance("i-0000000b", 10)]);
        assert!(
            plan.managed.is_empty(),
            "the overlap holds back both autoscalers"
        );
        assert_eq!(plan.refused.len(), 2);
        assert_eq!(plan.conflicts.len(), 2);
    }

    #[test]
    fn cooldowns_compile() {
        let with = |raw: Option<&str>| {
            let mut x = iva("a", by_id("i-12345678"));
            x.spec.behavior = Some(Behavior {
                scale_up: Some(ScalingRules {
                    cooldown_duration: raw.map(Into::into),
                    ..ScalingRules::default()
                }),
            });
            compile(&x)
        };
        assert_eq!(with(None).unwrap().effective.cooldown, MIN_COOLDOWN);
        assert_eq!(
            with(Some("6h30m")).unwrap().effective.cooldown,
            Duration::from_mins(390)
        );
        assert_eq!(
            with(Some("12h")).unwrap().effective.cooldown,
            Duration::from_hours(12)
        );
        let err = with(Some("5h59m")).unwrap_err();
        assert!(err.contains("shorter than 6h"), "{err}");
        let err = with(Some("1d")).unwrap_err();
        assert!(err.contains("use h, m, and s"), "{err}");
    }
}
