//! Agent identity. Every agent presents a projected `ServiceAccount` token
//! with the manager audience. The API server's `TokenReview` confirms the
//! token, the agent `ServiceAccount`, and the node the Pod is bound to, so a
//! compromised node can speak only for itself. The shared mutual TLS client
//! certificate only proves membership, never which node is calling.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, PostParams};

use crate::signature::sha256_hex;

/// The audience agents request their manager token for.
pub const AUDIENCE: &str = "ward-manager";
const NODE_CLAIM: &str = "authentication.kubernetes.io/node-name";
const CACHE_TTL: Duration = Duration::from_mins(5);
const CACHE_MAX: usize = 10_000;

pub enum Authenticator {
    /// Verifies tokens with the API server.
    TokenReview {
        client: kube::Client,
        username: String,
        cache: Mutex<HashMap<String, (String, Instant)>>,
    },
    /// Fixed token to node pairs, for tests.
    #[cfg(test)]
    Static(HashMap<String, String>),
}

/// The node of a reviewed token, when it belongs to `username`.
pub fn node_of_review(review: &TokenReview, username: &str) -> Option<String> {
    let status = review.status.as_ref()?;
    if status.authenticated != Some(true) {
        return None;
    }
    let user = status.user.as_ref()?;
    if user.username.as_deref() != Some(username) {
        return None;
    }
    user.extra
        .as_ref()?
        .get(NODE_CLAIM)?
        .first()
        .filter(|n| !n.is_empty())
        .cloned()
}

impl Authenticator {
    pub fn token_review(client: kube::Client, namespace: &str, service_account: &str) -> Self {
        Self::TokenReview {
            client,
            username: format!("system:serviceaccount:{namespace}:{service_account}"),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The node `token` speaks for, or `None` when it does not verify.
    pub async fn node(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        match self {
            #[cfg(test)]
            Self::Static(m) => m.get(token).cloned(),
            Self::TokenReview {
                client,
                username,
                cache,
            } => {
                let key = sha256_hex(token.as_bytes());
                if let Some((node, at)) = cache.lock().ok()?.get(&key)
                    && at.elapsed() < CACHE_TTL
                {
                    return Some(node.clone());
                }
                let review = TokenReview {
                    spec: TokenReviewSpec {
                        token: Some(token.to_string()),
                        audiences: Some(vec![AUDIENCE.to_string()]),
                    },
                    ..TokenReview::default()
                };
                let reviewed = Api::<TokenReview>::all(client.clone())
                    .create(&PostParams::default(), &review)
                    .await
                    .map_err(|e| tracing::warn!(error = %e, "token review"))
                    .ok()?;
                let Some(node) = node_of_review(&reviewed, username) else {
                    let user = reviewed
                        .status
                        .as_ref()
                        .and_then(|s| s.user.as_ref())
                        .and_then(|u| u.username.clone())
                        .unwrap_or_default();
                    tracing::warn!(user, expected = %username, "rejected an agent token");
                    return None;
                };
                if let Ok(mut c) = cache.lock() {
                    if c.len() >= CACHE_MAX {
                        c.clear();
                    }
                    c.insert(key, (node.clone(), Instant::now()));
                }
                Some(node)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::authentication::v1::{TokenReviewStatus, UserInfo};

    use super::*;

    fn review(ok: bool, user: &str, node: Option<&str>) -> TokenReview {
        TokenReview {
            status: Some(TokenReviewStatus {
                authenticated: Some(ok),
                user: Some(UserInfo {
                    username: Some(user.into()),
                    extra: node
                        .map(|n| BTreeMap::from([(NODE_CLAIM.to_string(), vec![n.to_string()])])),
                    ..UserInfo::default()
                }),
                ..TokenReviewStatus::default()
            }),
            ..TokenReview::default()
        }
    }

    #[tokio::test]
    async fn only_the_agent_account_with_a_node_claim_passes() {
        let sa = "system:serviceaccount:ward-system:ward-agent";
        assert_eq!(
            node_of_review(&review(true, sa, Some("n1")), sa).as_deref(),
            Some("n1")
        );
        assert!(node_of_review(&review(false, sa, Some("n1")), sa).is_none());
        assert!(
            node_of_review(&review(true, "system:serviceaccount:x:y", Some("n1")), sa).is_none()
        );
        assert!(node_of_review(&review(true, sa, None), sa).is_none());
        assert!(node_of_review(&review(true, sa, Some("")), sa).is_none());
        assert!(node_of_review(&TokenReview::default(), sa).is_none());

        let a = Authenticator::Static(HashMap::from([("t1".to_string(), "n1".to_string())]));
        assert_eq!(a.node("t1").await.as_deref(), Some("n1"));
        assert!(a.node("other").await.is_none());
        assert!(a.node("").await.is_none());
    }
}
