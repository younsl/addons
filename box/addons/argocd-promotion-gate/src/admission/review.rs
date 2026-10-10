//! The `AdmissionReview` envelope, narrowed to the fields the gate reads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::argocd::application::{nested, nested_str};

const ADMISSION_API_VERSION: &str = "admission.k8s.io/v1";
const ADMISSION_KIND: &str = "AdmissionReview";

/// The inbound `AdmissionReview` envelope.
#[derive(Debug, Default, Deserialize)]
pub struct ReviewRequest {
    #[serde(default)]
    pub request: Option<Request>,
}

/// The admission request itself.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Request {
    pub uid: String,
    pub name: String,
    pub namespace: String,
    pub operation: String,
    pub user_info: UserInfo,
    pub object: Option<Value>,
    pub old_object: Option<Value>,
    /// Set when the API server is evaluating a write it will not persist. The
    /// verdict is still computed and returned, but nothing may be written to
    /// the cluster on this path, which is what the webhook's
    /// `sideEffects: NoneOnDryRun` promises.
    pub dry_run: bool,
}

/// The authenticated principal that issued the write. For a sync started from
/// the UI this is the argocd-server service account, not the person. The
/// person's name travels inside `operation.initiatedBy`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserInfo {
    pub username: String,
    pub groups: Vec<String>,
}

/// The outbound `AdmissionReview` envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewResponse {
    pub api_version: &'static str,
    pub kind: &'static str,
    pub response: Response,
}

/// The admission verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Response {
    pub uid: String,
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Carries the message the Argo CD UI shows in its error toast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub code: i32,
    pub reason: String,
    pub message: String,
}

impl ReviewResponse {
    /// Builds an allowing response.
    #[must_use]
    pub fn allow(uid: &str, warnings: Vec<String>) -> Self {
        Self {
            api_version: ADMISSION_API_VERSION,
            kind: ADMISSION_KIND,
            response: Response {
                uid: uid.to_string(),
                allowed: true,
                status: None,
                warnings,
            },
        }
    }

    /// Builds a denying response. The message is surfaced verbatim by kubectl,
    /// by the Argo CD CLI, and by the Argo CD UI toast, so it is written for
    /// the person reading it.
    #[must_use]
    pub fn deny(uid: &str, reason: &str, message: &str) -> Self {
        Self {
            api_version: ADMISSION_API_VERSION,
            kind: ADMISSION_KIND,
            response: Response {
                uid: uid.to_string(),
                allowed: false,
                status: Some(Status {
                    code: 403,
                    reason: reason.to_string(),
                    message: message.to_string(),
                }),
                warnings: Vec::new(),
            },
        }
    }
}

impl Request {
    /// Reports whether this admission request starts a sync.
    ///
    /// A sync sets the top-level `operation` field. Every other write to an
    /// Application, status updates from the controller and spec changes from
    /// git included, leaves it untouched and must pass straight through.
    #[must_use]
    pub fn is_sync_request(&self) -> bool {
        has_operation(self.object.as_ref()) && !has_operation(self.old_object.as_ref())
    }

    /// Reports whether Argo CD marked the pending operation automated, which
    /// is how an auto-sync differs from a person pressing Sync.
    #[must_use]
    pub fn is_automated(&self) -> bool {
        self.object
            .as_ref()
            .and_then(|obj| nested(obj, &["operation", "initiatedBy", "automated"]))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The user Argo CD recorded as starting the sync, which is more useful in
    /// a log line than the service account that carried the write.
    #[must_use]
    pub fn initiated_by(&self) -> &str {
        self.object.as_ref().map_or("", |obj| {
            nested_str(obj, &["operation", "initiatedBy", "username"])
        })
    }

    /// The revision the pending operation targets, which is what makes a
    /// rollback identifiable.
    #[must_use]
    pub fn sync_revision(&self) -> &str {
        self.object.as_ref().map_or("", |obj| {
            nested_str(obj, &["operation", "sync", "revision"])
        })
    }

    /// The Application's own UID, which is what an Event has to point at. It
    /// is not `uid`: that identifies the admission request and means nothing
    /// to anybody reading the Application later.
    #[must_use]
    pub fn object_uid(&self) -> &str {
        self.object
            .as_ref()
            .map_or("", |obj| nested_str(obj, &["metadata", "uid"]))
    }

    /// The authenticated principal, with a printable fallback.
    #[must_use]
    pub fn username(&self) -> &str {
        if self.user_info.username.trim().is_empty() {
            "<unknown>"
        } else {
            &self.user_info.username
        }
    }
}

fn has_operation(obj: Option<&Value>) -> bool {
    obj.and_then(|o| o.get("operation"))
        .is_some_and(|v| !v.is_null())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(clippy::needless_pass_by_value)]
    fn request(object: Value, old_object: Value) -> Request {
        serde_json::from_value(json!({
            "uid": "req-1",
            "name": "prd-api",
            "namespace": "argocd",
            "operation": "UPDATE",
            "userInfo": {"username": "system:serviceaccount:argocd:argocd-server", "groups": ["a"]},
            "object": object,
            "oldObject": old_object
        }))
        .unwrap()
    }

    #[test]
    fn sync_request_is_a_new_operation_field() {
        let req = request(json!({"operation": {"sync": {}}}), json!({"metadata": {}}));
        assert!(req.is_sync_request());
        let req = request(
            json!({"operation": {"sync": {}}}),
            json!({"operation": {"sync": {}}}),
        );
        assert!(
            !req.is_sync_request(),
            "operation already present is a retry, not a new sync"
        );
        let req = request(json!({"operation": null}), json!({}));
        assert!(!req.is_sync_request());
        let req = Request::default();
        assert!(!req.is_sync_request());
    }

    #[test]
    fn accessors_read_nested_fields() {
        let req = request(
            json!({
                "metadata": {"uid": "app-uid"},
                "operation": {"initiatedBy": {"automated": true, "username": "dev"}, "sync": {"revision": "abc"}}
            }),
            json!({}),
        );
        assert!(req.is_automated());
        assert_eq!(req.initiated_by(), "dev");
        assert_eq!(req.sync_revision(), "abc");
        assert_eq!(req.object_uid(), "app-uid");
        assert_eq!(req.username(), "system:serviceaccount:argocd:argocd-server");
        assert!(!req.dry_run);

        let empty = Request::default();
        assert!(!empty.is_automated());
        assert_eq!(empty.initiated_by(), "");
        assert_eq!(empty.sync_revision(), "");
        assert_eq!(empty.object_uid(), "");
        assert_eq!(empty.username(), "<unknown>");
    }

    #[test]
    fn responses_serialize_like_the_api_server_expects() {
        let allow = serde_json::to_value(ReviewResponse::allow("u", vec![])).unwrap();
        assert_eq!(allow["apiVersion"], "admission.k8s.io/v1");
        assert_eq!(allow["kind"], "AdmissionReview");
        assert_eq!(allow["response"]["uid"], "u");
        assert_eq!(allow["response"]["allowed"], true);
        assert!(allow["response"].get("status").is_none());
        assert!(allow["response"].get("warnings").is_none());

        let warned = serde_json::to_value(ReviewResponse::allow("u", vec!["w".into()])).unwrap();
        assert_eq!(warned["response"]["warnings"][0], "w");

        let deny = serde_json::to_value(ReviewResponse::deny("u", "Blocked", "why")).unwrap();
        assert_eq!(deny["response"]["allowed"], false);
        assert_eq!(deny["response"]["status"]["code"], 403);
        assert_eq!(deny["response"]["status"]["reason"], "Blocked");
        assert_eq!(deny["response"]["status"]["message"], "why");
    }

    #[test]
    fn envelope_without_request_decodes() {
        let review: ReviewRequest =
            serde_json::from_str(r#"{"apiVersion":"admission.k8s.io/v1"}"#).unwrap();
        assert!(review.request.is_none());
    }
}
