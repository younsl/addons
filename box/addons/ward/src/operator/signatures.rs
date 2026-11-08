//! Validates `WardSignatures` and reports the result on each one. CEL checks
//! the shape at admission, compiling the patterns and YARA happens here.

use kube::ResourceExt as _;
use kube::api::{Api, ListParams, Patch, PatchParams};
use serde_json::json;

use crate::content::{YaraSource, compile_yara};
use crate::crd::{WardSignature, WardSignatureStatus};
use crate::signature::{Signature, SignatureDb};

#[derive(Debug, Default)]
pub struct Collected {
    pub signatures: Vec<Signature>,
    pub yara: Vec<YaraSource>,
}

/// The verdict on one `WardSignature`: accepted signatures and YARA, or the
/// first error.
pub fn validate(ws: &WardSignature) -> Result<Collected, String> {
    SignatureDb::compile(&ws.spec.signatures).map_err(|e| e.to_string())?;
    let mut out = Collected {
        signatures: ws.spec.signatures.clone(),
        yara: Vec::new(),
    };
    if let Some(src) = ws.spec.yara.as_ref().filter(|s| !s.trim().is_empty()) {
        let source = YaraSource {
            name: format!("{}/{}", ws.namespace().unwrap_or_default(), ws.name_any()),
            source: src.clone(),
        };
        let (rules, dropped) = compile_yara(std::slice::from_ref(&source));
        if let Some(e) = dropped.first() {
            return Err(e.clone());
        }
        if rules.is_some() {
            out.yara.push(source);
        }
    }
    Ok(out)
}

pub fn status(ws: &WardSignature, verdict: &Result<Collected, String>) -> WardSignatureStatus {
    let (ok, reason, message, count) = match verdict {
        Ok(c) => (
            true,
            "Compiled",
            String::new(),
            c.signatures.len() + c.yara.len(),
        ),
        Err(e) => (false, "Invalid", e.clone(), 0),
    };
    let prev = ws.status.clone().unwrap_or_default();
    WardSignatureStatus {
        observed_generation: ws.metadata.generation,
        signatures: i64::try_from(count).unwrap_or(i64::MAX),
        conditions: vec![super::condition(
            "Accepted",
            ok,
            reason,
            &message,
            &prev.conditions,
            ws.metadata.generation,
        )],
    }
}

/// Every accepted `WardSignature` in `ns`, with each one's status updated.
pub async fn collect(client: &kube::Client, ns: &str) -> Result<Collected, kube::Error> {
    let api: Api<WardSignature> = Api::namespaced(client.clone(), ns);
    let mut out = Collected::default();
    let mut list = api.list(&ListParams::default()).await?.items;
    list.sort_by_key(kube::ResourceExt::name_any);
    for ws in list {
        let verdict = validate(&ws);
        let st = status(&ws, &verdict);
        if ws.status.as_ref() != Some(&st) {
            api.patch_status(
                &ws.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({"status": st})),
            )
            .await?;
        }
        if let Ok(c) = verdict {
            out.signatures.extend(c.signatures);
            out.yara.extend(c.yara);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;
    use crate::crd::WardSignatureSpec;
    use crate::policy::Severity;
    use crate::signature::MatchCondition;

    fn ws(spec: WardSignatureSpec) -> WardSignature {
        WardSignature {
            metadata: ObjectMeta {
                name: Some("team".into()),
                namespace: Some("ns".into()),
                generation: Some(2),
                ..ObjectMeta::default()
            },
            spec,
            status: None,
        }
    }

    fn sig(hex: &str) -> Signature {
        Signature {
            name: "Team.One".into(),
            severity: Severity::High,
            description: String::new(),
            sha256: vec![],
            strings: vec![],
            hex: vec![hex.into()],
            condition: MatchCondition::All,
        }
    }

    #[test]
    fn accepts_valid_and_reports_invalid() {
        let good = ws(WardSignatureSpec {
            signatures: vec![sig("deadbeef")],
            yara: Some("rule T { condition: false }".into()),
        });
        let v = validate(&good);
        let c = v.as_ref().unwrap();
        assert_eq!((c.signatures.len(), c.yara.len()), (1, 1));
        assert_eq!(c.yara[0].name, "ns/team");
        let st = status(&good, &v);
        assert_eq!(st.signatures, 2);
        assert_eq!(st.conditions[0].status, "True");
        assert_eq!(st.observed_generation, Some(2));

        let bad_hex = ws(WardSignatureSpec {
            signatures: vec![sig("abc")],
            yara: None,
        });
        assert!(validate(&bad_hex).is_err());
        let bad_yara = ws(WardSignatureSpec {
            signatures: vec![],
            yara: Some("rule {".into()),
        });
        let v = validate(&bad_yara);
        assert!(v.is_err());
        let st = status(&bad_yara, &v);
        assert_eq!(st.conditions[0].status, "False");
        assert_eq!(st.conditions[0].reason, "Invalid");
        let blank = ws(WardSignatureSpec {
            signatures: vec![],
            yara: Some("  ".into()),
        });
        assert!(validate(&blank).unwrap().yara.is_empty());
    }
}
