//! Secret-backed API tokens.
//!
//! Tokens are authored, hashed, and unrecoverable, so they need a durable home
//! that survives a pod with no volume. One Secret holds them all; the data key
//! is the token prefix `create_token` already computes (`tc_` plus 8 hex
//! characters, a valid Secret key as it stands) and the value is the JSON of
//! everything the old row held minus the prefix.
//!
//! Validation keeps its previous shape: take the first 11 characters of the
//! presented Bearer token, look up that one key, then compare the SHA-256 hash
//! in constant time. No scan, and the plaintext still never leaves the response
//! that created it.
//!
//! An API server GET per authenticated request is not acceptable, so the Secret
//! is watched into an in-memory map. Revocation then propagates at watch
//! latency instead of instantly, which is the correct trade for a token that
//! already carries an expiry.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{
    Client,
    api::{Api, Patch, PatchParams, PostParams},
    runtime::watcher::{Config as WatcherConfig, Event, watcher},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{debug, error, info, warn};

use super::models::TokenInfo;

/// Length of the lookup prefix: `tc_` plus 8 hex characters.
pub const TOKEN_PREFIX_LEN: usize = 11;

/// Maximum tokens one user may hold at once.
pub const MAX_TOKENS_PER_USER: usize = 5;

/// How often coalesced `last_used_at` updates are flushed. The field is
/// best-effort and is explicitly allowed to be lost on pod exit.
pub const LAST_USED_FLUSH_SECS: u64 = 300;

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("a token named '{0}' already exists")]
    DuplicateName(String),
    #[error("maximum {MAX_TOKENS_PER_USER} tokens per user")]
    TooMany,
    #[error("token not found")]
    NotFound,
    #[error("kube API error: {0}")]
    Kube(#[from] kube::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Result of a successful token lookup: the subject and the group snapshot
/// captured when the token was issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedToken {
    pub user_sub: String,
    pub groups: Vec<String>,
}

/// One stored token, keyed in the Secret by its prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    pub user_sub: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub token_hash: String,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
}

impl StoredToken {
    fn is_expired(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .map(|exp| now >= exp)
            .unwrap_or(false)
    }

    fn to_info(&self, prefix: &str) -> TokenInfo {
        TokenInfo {
            name: self.name.clone(),
            description: self.description.clone(),
            token_prefix: prefix.to_string(),
            created_at: self.created_at.clone(),
            expires_at: self.expires_at.clone(),
            last_used_at: self.last_used_at.clone(),
        }
    }
}

/// In-memory projection of the tokens Secret, split out so the lookup and
/// listing rules are testable without a cluster.
#[derive(Debug, Default)]
pub struct TokenCache {
    tokens: RwLock<HashMap<String, StoredToken>>,
    /// Prefixes whose `last_used_at` moved since the last flush.
    pending_last_used: RwLock<HashMap<String, String>>,
}

impl TokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, StoredToken>> {
        self.tokens.read().expect("token cache poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, StoredToken>> {
        self.tokens.write().expect("token cache poisoned")
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, prefix: &str) -> Option<StoredToken> {
        self.read().get(prefix).cloned()
    }

    pub fn insert(&self, prefix: String, token: StoredToken) {
        self.write().insert(prefix, token);
    }

    pub fn remove(&self, prefix: &str) {
        self.write().remove(prefix);
        self.pending_last_used
            .write()
            .expect("token cache poisoned")
            .remove(prefix);
    }

    pub fn clear(&self) {
        self.write().clear();
    }

    /// Replace the contents with a freshly observed Secret. Coalesced
    /// `last_used_at` values that have not been flushed yet are re-applied so a
    /// watch event does not visibly roll the field backwards.
    pub fn absorb(&self, secret: &Secret) {
        let mut next = HashMap::new();
        for (prefix, raw) in secret.data.iter().flatten() {
            match serde_json::from_slice::<StoredToken>(&raw.0) {
                Ok(token) => {
                    next.insert(prefix.clone(), token);
                }
                Err(e) => warn!(prefix = %prefix, error = %e, "Skipping malformed API token"),
            }
        }
        for (prefix, raw) in secret.string_data.iter().flatten() {
            match serde_json::from_str::<StoredToken>(raw) {
                Ok(token) => {
                    next.insert(prefix.clone(), token);
                }
                Err(e) => warn!(prefix = %prefix, error = %e, "Skipping malformed API token"),
            }
        }
        {
            let pending = self.pending_last_used.read().expect("token cache poisoned");
            for (prefix, ts) in pending.iter() {
                if let Some(t) = next.get_mut(prefix) {
                    t.last_used_at = Some(ts.clone());
                }
            }
        }
        let count = next.len();
        *self.write() = next;
        debug!(tokens = count, "API token cache refreshed");
    }

    /// Tokens owned by one user, newest first.
    pub fn list_for_user(&self, user_sub: &str) -> Vec<TokenInfo> {
        let mut out: Vec<TokenInfo> = self
            .read()
            .iter()
            .filter(|(_, t)| t.user_sub == user_sub)
            .map(|(prefix, t)| t.to_info(prefix))
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    pub fn user_has_name(&self, user_sub: &str, name: &str) -> bool {
        self.read()
            .values()
            .any(|t| t.user_sub == user_sub && t.name == name)
    }

    /// Validate a plaintext token against the cache. Returns `None` for an
    /// unknown prefix, a hash mismatch, or an expired token.
    pub fn validate(&self, token_plaintext: &str) -> Option<(String, ValidatedToken)> {
        if token_plaintext.len() < TOKEN_PREFIX_LEN {
            return None;
        }
        let prefix = &token_plaintext[..TOKEN_PREFIX_LEN];
        let stored = self.get(prefix)?;
        if !constant_time_eq(&hash_token(token_plaintext), &stored.token_hash) {
            return None;
        }
        if stored.is_expired(chrono::Utc::now()) {
            debug!(prefix = %prefix, "API token expired");
            return None;
        }
        Some((
            prefix.to_string(),
            ValidatedToken {
                user_sub: stored.user_sub,
                groups: stored.groups,
            },
        ))
    }

    /// Record a use without touching the API server. Flushed later in one
    /// batch, and explicitly allowed to be lost if the pod exits first.
    pub fn touch(&self, prefix: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        if let Some(t) = self.write().get_mut(prefix) {
            t.last_used_at = Some(now.clone());
        }
        self.pending_last_used
            .write()
            .expect("token cache poisoned")
            .insert(prefix.to_string(), now);
    }

    /// Drain the coalesced `last_used_at` updates for flushing.
    pub fn take_pending_last_used(&self) -> HashMap<String, String> {
        std::mem::take(
            &mut *self
                .pending_last_used
                .write()
                .expect("token cache poisoned"),
        )
    }
}

#[derive(Clone)]
pub struct TokenStore {
    client: Client,
    namespace: String,
    secret_name: String,
    cache: Arc<TokenCache>,
}

impl TokenStore {
    pub fn new(client: Client, namespace: String, secret_name: String) -> Self {
        Self {
            client,
            namespace,
            secret_name,
            cache: Arc::new(TokenCache::new()),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn secret_name(&self) -> &str {
        &self.secret_name
    }

    pub fn cache(&self) -> &TokenCache {
        &self.cache
    }

    fn api(&self) -> Api<Secret> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    pub async fn ensure_exists(&self) -> Result<(), TokenError> {
        let api = self.api();
        if api.get_opt(&self.secret_name).await?.is_some() {
            return Ok(());
        }
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some(self.secret_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(super::managed_labels("trivy-collector-api-tokens")),
                ..Default::default()
            },
            ..Default::default()
        };
        match api.create(&PostParams::default(), &secret).await {
            Ok(_) => {
                info!(
                    namespace = %self.namespace,
                    name = %self.secret_name,
                    "Created empty API tokens Secret"
                );
                Ok(())
            }
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
            Err(e) => Err(TokenError::Kube(e)),
        }
    }

    /// List one user's tokens (hashes are never included).
    pub fn list(&self, user_sub: &str) -> Vec<TokenInfo> {
        self.cache.list_for_user(user_sub)
    }

    /// Mint a token. `groups` is the issuer's current group list, frozen into
    /// the token so RBAC evaluates Bearer requests with the issuer's roles.
    /// Returns the plaintext (the only time it exists) and its metadata.
    pub async fn create(
        &self,
        user_sub: &str,
        name: &str,
        description: &str,
        expires_days: u32,
        groups: &[String],
    ) -> Result<(String, TokenInfo), TokenError> {
        if self.cache.user_has_name(user_sub, name) {
            return Err(TokenError::DuplicateName(name.to_string()));
        }
        if self.cache.list_for_user(user_sub).len() >= MAX_TOKENS_PER_USER {
            return Err(TokenError::TooMany);
        }

        let plaintext = generate_token();
        let prefix = plaintext[..TOKEN_PREFIX_LEN].to_string();
        let now = chrono::Utc::now();
        let stored = StoredToken {
            user_sub: user_sub.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            token_hash: hash_token(&plaintext),
            created_at: now.to_rfc3339(),
            expires_at: now
                .checked_add_signed(chrono::Duration::days(i64::from(expires_days)))
                .unwrap_or(now)
                .to_rfc3339(),
            last_used_at: None,
            groups: groups.to_vec(),
        };

        self.ensure_exists().await?;
        let patch = serde_json::json!({
            "stringData": { prefix.clone(): serde_json::to_string(&stored)? },
        });
        self.api()
            .patch(
                &self.secret_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;

        let info = stored.to_info(&prefix);
        self.cache.insert(prefix, stored);
        info!(user_sub = %user_sub, token_name = %name, "API token created");
        Ok((plaintext, info))
    }

    /// Delete a token by prefix, only if it belongs to the given user.
    pub async fn delete(&self, user_sub: &str, prefix: &str) -> Result<(), TokenError> {
        match self.cache.get(prefix) {
            Some(t) if t.user_sub == user_sub => {}
            _ => return Err(TokenError::NotFound),
        }
        // Both maps are cleared: a token created by this replica lands in
        // stringData, one absorbed from the API server in data.
        let patch = serde_json::json!({
            "data": { prefix: serde_json::Value::Null },
            "stringData": { prefix: serde_json::Value::Null },
        });
        self.api()
            .patch(
                &self.secret_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
        self.cache.remove(prefix);
        info!(user_sub = %user_sub, prefix = %prefix, "API token deleted");
        Ok(())
    }

    /// Validate a presented Bearer token and record the use in memory.
    pub fn validate(&self, token_plaintext: &str) -> Option<ValidatedToken> {
        let (prefix, validated) = self.cache.validate(token_plaintext)?;
        self.cache.touch(&prefix);
        Some(validated)
    }

    /// Push coalesced `last_used_at` values to the Secret in one patch.
    async fn flush_last_used(&self) {
        let pending = self.cache.take_pending_last_used();
        if pending.is_empty() {
            return;
        }
        let mut data = serde_json::Map::new();
        for (prefix, ts) in pending {
            let Some(mut token) = self.cache.get(&prefix) else {
                continue;
            };
            token.last_used_at = Some(ts);
            match serde_json::to_string(&token) {
                Ok(json) => {
                    data.insert(prefix, serde_json::Value::String(json));
                }
                Err(e) => warn!(error = %e, "Failed to serialize token for last_used flush"),
            }
        }
        if data.is_empty() {
            return;
        }
        let count = data.len();
        let patch = serde_json::json!({ "stringData": data });
        match self
            .api()
            .patch(
                &self.secret_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
        {
            Ok(_) => debug!(tokens = count, "Flushed API token last_used_at"),
            // Best effort: a failed flush loses at most one interval of
            // last_used precision.
            Err(e) => warn!(error = %e, "Failed to flush API token last_used_at"),
        }
    }

    /// Watch the Secret into memory and flush coalesced use timestamps on an
    /// interval.
    pub async fn run_watch(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let cfg = WatcherConfig::default().fields(&format!("metadata.name={}", self.secret_name));
        let mut stream = watcher(self.api(), cfg).boxed();
        let mut flush = tokio::time::interval(std::time::Duration::from_secs(LAST_USED_FLUSH_SECS));
        flush.tick().await;

        info!(
            namespace = %self.namespace,
            secret = %self.secret_name,
            "API tokens Secret watcher started"
        );

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("API tokens Secret watcher shutting down");
                    break;
                }
                _ = flush.tick() => self.flush_last_used().await,
                ev = stream.next() => {
                    match ev {
                        Some(Ok(Event::Apply(s))) | Some(Ok(Event::InitApply(s))) => {
                            self.cache.absorb(&s);
                        }
                        Some(Ok(Event::Delete(_))) => {
                            warn!("API tokens Secret deleted — clearing cache");
                            self.cache.clear();
                        }
                        Some(Ok(Event::Init)) | Some(Ok(Event::InitDone)) => {}
                        Some(Err(e)) => error!(error = %e, "API tokens Secret watcher error"),
                        None => {
                            warn!("API tokens Secret watcher stream ended");
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Generate a random API token: "tc_" plus 32 random bytes as hex.
pub fn generate_token() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    format!("tc_{}", hex::encode(bytes))
}

/// SHA-256 hash a token and return a hex string.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Compare two hex digests without leaking a match position through timing.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(user: &str, name: &str, plaintext: &str, expires_at: &str) -> StoredToken {
        StoredToken {
            user_sub: user.to_string(),
            name: name.to_string(),
            description: String::new(),
            token_hash: hash_token(plaintext),
            created_at: "2026-01-01T00:00:00+00:00".to_string(),
            expires_at: expires_at.to_string(),
            last_used_at: None,
            groups: vec!["platform".to_string()],
        }
    }

    fn far_future() -> String {
        "2099-01-01T00:00:00+00:00".to_string()
    }

    #[test]
    fn generated_tokens_have_a_usable_prefix() {
        let t = generate_token();
        assert!(t.starts_with("tc_"));
        assert_eq!(t.len(), 67);
        let prefix = &t[..TOKEN_PREFIX_LEN];
        // The prefix must be a legal Secret data key as it stands.
        assert!(
            prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        );
    }

    #[test]
    fn hash_token_is_deterministic_and_hex() {
        let h = hash_token("tc_abc");
        assert_eq!(h, hash_token("tc_abc"));
        assert_ne!(h, hash_token("tc_abd"));
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn constant_time_eq_matches_string_equality() {
        assert!(constant_time_eq("abcd", "abcd"));
        assert!(!constant_time_eq("abcd", "abce"));
        assert!(!constant_time_eq("abcd", "abc"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn validate_accepts_the_right_plaintext() {
        let cache = TokenCache::new();
        let plaintext = generate_token();
        let prefix = plaintext[..TOKEN_PREFIX_LEN].to_string();
        cache.insert(
            prefix.clone(),
            stored("user-1", "ci", &plaintext, &far_future()),
        );

        let (got_prefix, validated) = cache.validate(&plaintext).expect("token validates");
        assert_eq!(got_prefix, prefix);
        assert_eq!(validated.user_sub, "user-1");
        assert_eq!(validated.groups, vec!["platform".to_string()]);
    }

    #[test]
    fn validate_rejects_a_hash_mismatch_on_a_known_prefix() {
        let cache = TokenCache::new();
        let plaintext = generate_token();
        let prefix = plaintext[..TOKEN_PREFIX_LEN].to_string();
        cache.insert(prefix.clone(), stored("u", "n", &plaintext, &far_future()));

        // Same prefix, different remainder.
        let forged = format!("{}{}", prefix, "0".repeat(56));
        assert!(cache.validate(&forged).is_none());
    }

    #[test]
    fn validate_rejects_unknown_prefixes_and_short_input() {
        let cache = TokenCache::new();
        assert!(cache.validate(&generate_token()).is_none());
        assert!(cache.validate("tc_").is_none());
    }

    #[test]
    fn validate_rejects_an_expired_token() {
        let cache = TokenCache::new();
        let plaintext = generate_token();
        cache.insert(
            plaintext[..TOKEN_PREFIX_LEN].to_string(),
            stored("u", "n", &plaintext, "2020-01-01T00:00:00+00:00"),
        );
        assert!(cache.validate(&plaintext).is_none());
    }

    #[test]
    fn unparseable_expiry_does_not_lock_a_token_out() {
        let t = stored("u", "n", "tc_x", "not-a-timestamp");
        assert!(!t.is_expired(chrono::Utc::now()));
    }

    #[test]
    fn list_for_user_is_scoped_and_newest_first() {
        let cache = TokenCache::new();
        let mut older = stored("u1", "older", "tc_a", &far_future());
        older.created_at = "2026-01-01T00:00:00+00:00".into();
        let mut newer = stored("u1", "newer", "tc_b", &far_future());
        newer.created_at = "2026-06-01T00:00:00+00:00".into();
        cache.insert("tc_aaaaaaaa".into(), older);
        cache.insert("tc_bbbbbbbb".into(), newer);
        cache.insert(
            "tc_cccccccc".into(),
            stored("u2", "other", "tc_c", &far_future()),
        );

        let list = cache.list_for_user("u1");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "newer");
        assert_eq!(list[1].name, "older");
        assert_eq!(cache.list_for_user("u2").len(), 1);
        assert!(cache.list_for_user("nobody").is_empty());
    }

    #[test]
    fn user_has_name_is_per_user() {
        let cache = TokenCache::new();
        cache.insert(
            "tc_aaaaaaaa".into(),
            stored("u1", "ci", "tc_a", &far_future()),
        );
        assert!(cache.user_has_name("u1", "ci"));
        assert!(!cache.user_has_name("u2", "ci"));
        assert!(!cache.user_has_name("u1", "other"));
    }

    #[test]
    fn touch_coalesces_until_drained() {
        let cache = TokenCache::new();
        let plaintext = generate_token();
        let prefix = plaintext[..TOKEN_PREFIX_LEN].to_string();
        cache.insert(prefix.clone(), stored("u", "n", &plaintext, &far_future()));

        cache.touch(&prefix);
        cache.touch(&prefix);
        let pending = cache.take_pending_last_used();
        assert_eq!(pending.len(), 1);
        assert!(cache.get(&prefix).unwrap().last_used_at.is_some());
        // Drained: a second take sees nothing.
        assert!(cache.take_pending_last_used().is_empty());
    }

    #[test]
    fn absorb_reads_both_data_and_string_data_and_skips_garbage() {
        let cache = TokenCache::new();
        let t = stored("u", "n", "tc_a", &far_future());
        let mut data = std::collections::BTreeMap::new();
        data.insert(
            "tc_aaaaaaaa".to_string(),
            k8s_openapi::ByteString(serde_json::to_vec(&t).unwrap()),
        );
        data.insert(
            "tc_garbage_".to_string(),
            k8s_openapi::ByteString(b"{nope".to_vec()),
        );
        let mut string_data = std::collections::BTreeMap::new();
        string_data.insert(
            "tc_bbbbbbbb".to_string(),
            serde_json::to_string(&t).unwrap(),
        );

        let secret = Secret {
            data: Some(data),
            string_data: Some(string_data),
            ..Default::default()
        };
        cache.absorb(&secret);

        assert_eq!(cache.len(), 2);
        assert!(cache.get("tc_aaaaaaaa").is_some());
        assert!(cache.get("tc_bbbbbbbb").is_some());
    }

    #[test]
    fn absorb_keeps_unflushed_last_used_values() {
        let cache = TokenCache::new();
        let plaintext = generate_token();
        let prefix = plaintext[..TOKEN_PREFIX_LEN].to_string();
        let t = stored("u", "n", &plaintext, &far_future());
        cache.insert(prefix.clone(), t.clone());
        cache.touch(&prefix);

        // The API server still holds the pre-touch value.
        let mut data = std::collections::BTreeMap::new();
        data.insert(
            prefix.clone(),
            k8s_openapi::ByteString(serde_json::to_vec(&t).unwrap()),
        );
        cache.absorb(&Secret {
            data: Some(data),
            ..Default::default()
        });

        assert!(cache.get(&prefix).unwrap().last_used_at.is_some());
    }

    #[test]
    fn remove_drops_the_token_and_its_pending_touch() {
        let cache = TokenCache::new();
        let plaintext = generate_token();
        let prefix = plaintext[..TOKEN_PREFIX_LEN].to_string();
        cache.insert(prefix.clone(), stored("u", "n", &plaintext, &far_future()));
        cache.touch(&prefix);
        cache.remove(&prefix);

        assert!(cache.is_empty());
        assert!(cache.take_pending_last_used().is_empty());
    }

    #[test]
    fn stored_token_roundtrips_and_tolerates_missing_optionals() {
        let raw = r#"{"user_sub":"u","name":"n","token_hash":"h",
                      "created_at":"2026-01-01T00:00:00+00:00",
                      "expires_at":"2099-01-01T00:00:00+00:00"}"#;
        let t: StoredToken = serde_json::from_str(raw).unwrap();
        assert!(t.description.is_empty());
        assert!(t.groups.is_empty());
        assert!(t.last_used_at.is_none());

        let back: StoredToken = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn to_info_never_exposes_the_hash() {
        let t = stored("u", "ci", "tc_a", &far_future());
        let info = t.to_info("tc_aaaaaaaa");
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains(&t.token_hash));
        assert!(json.contains("tc_aaaaaaaa"));
    }
}
