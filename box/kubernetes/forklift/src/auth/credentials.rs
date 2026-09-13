//! Password hashing, personal access token generation and the credential
//! sentinels the login paths return.

use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::Digest as _;

/// Identifies forklift personal access tokens in client configs.
const PAT_PREFIX: &str = "flpat_";

/// bcrypt's work factor.
const DEFAULT_HASH_COST: u32 = 10;

/// The lowest cost the bcrypt crate accepts, used by tests.
const MIN_HASH_COST: u32 = 4;

static HASH_COST: AtomicU32 = AtomicU32::new(DEFAULT_HASH_COST);

/// Lowers the bcrypt cost for tests. Never call in production code paths.
pub fn set_test_hash_cost() {
    HASH_COST.store(MIN_HASH_COST, Ordering::Relaxed);
}

/// Returns a bcrypt hash of a plaintext password.
pub fn hash_password(plain: &str) -> Result<String, super::Error> {
    bcrypt::hash(plain, HASH_COST.load(Ordering::Relaxed))
        .map_err(|e| super::Error::Other(format!("hash password: {e}")))
}

/// Checks a plaintext password against a bcrypt hash.
pub fn verify_password(hash: &str, plain: &str) -> bool {
    bcrypt::verify(plain, hash).unwrap_or(false)
}

/// Creates a new personal access token and its storage hash. The plaintext is
/// returned once to the caller and never persisted.
pub fn generate_token() -> Result<(String, String), super::Error> {
    let mut buf = [0u8; 24];
    rand::fill(&mut buf[..]);
    let plaintext = format!("{PAT_PREFIX}{}", hex::encode(buf));
    let hash = hash_token(&plaintext);
    Ok((plaintext, hash))
}

/// Returns the SHA-256 hex hash used to look up a token. PATs are high entropy,
/// so a fast hash (not bcrypt) is appropriate and keeps lookups cheap.
pub fn hash_token(plaintext: &str) -> String {
    let sum = sha2::Sha256::digest(plaintext.as_bytes());
    hex::encode(sum)
}

/// Generates a URL-safe random password (used to seed the initial admin when no
/// password is configured).
pub fn random_password() -> Result<String, super::Error> {
    let mut b = [0u8; 18];
    rand::fill(&mut b[..]);
    Ok(URL_SAFE_NO_PAD.encode(b))
}

/// Reports whether a credential string looks like a forklift PAT.
pub fn is_pat(s: &str) -> bool {
    s.len() > PAT_PREFIX.len() && s.starts_with(PAT_PREFIX)
}

/// The consecutive failed-password threshold that locks a lockout-enabled
/// account.
pub const MAX_FAILED_LOGINS: i64 = 5;
