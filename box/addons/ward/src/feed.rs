//! The signature database feed: building it from public sources, checking
//! it against a false-positive corpus, and pulling it from an OCI registry.

pub mod build;
pub mod oci;

use crate::content::{ContentError, FeedDb, check_digest, verify_signature};
use crate::crd::FeedSpec;
use crate::rollout::ContentRef;

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error(transparent)]
    Oci(#[from] oci::OciError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("feed has no signature layer and insecureSkipVerify is false")]
    Unsigned,
}

/// A database pulled and checked.
pub struct Pulled {
    pub content: ContentRef,
    pub db: FeedDb,
}

/// Checks the digest and the signature of downloaded bytes.
pub fn verify(
    content: &ContentRef,
    db_bytes: &[u8],
    sig_bytes: &[u8],
    public_key: Option<&str>,
    insecure: bool,
) -> Result<FeedDb, FeedError> {
    check_digest(db_bytes, &content.digest)?;
    if !insecure {
        let key = public_key.ok_or(FeedError::Unsigned)?;
        if sig_bytes.is_empty() {
            return Err(FeedError::Unsigned);
        }
        verify_signature(db_bytes, sig_bytes, key)?;
    }
    Ok(FeedDb::decode(db_bytes)?)
}

/// Resolves the feed reference, downloads both layers, and verifies them.
pub async fn pull(registry: &oci::Registry, spec: &FeedSpec) -> Result<Pulled, FeedError> {
    let reference = oci::Reference::parse(&spec.image)?;
    let resolved = registry.resolve(&reference).await?;
    let db_bytes = registry
        .blob(
            &reference,
            &resolved.db.digest,
            crate::content::MAX_DB_BYTES,
        )
        .await?;
    let sig_bytes = match &resolved.signature {
        Some(s) => registry.blob(&reference, &s.digest, 64 * 1024).await?,
        None => Vec::new(),
    };
    let mut content = ContentRef {
        revision: String::new(),
        digest: resolved.db.digest.clone(),
        signature_digest: resolved
            .signature
            .as_ref()
            .map(|s| s.digest.clone())
            .unwrap_or_default(),
        repository: reference.repository_url(),
        size: resolved.db.size,
    };
    let db = verify(
        &content,
        &db_bytes,
        &sig_bytes,
        spec.public_key.as_deref(),
        spec.insecure_skip_verify,
    )?;
    content.revision.clone_from(&db.manifest.revision);
    Ok(Pulled { content, db })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::oci_digest;
    use crate::content::tests::sample_db;

    #[test]
    fn verify_requires_a_signature_unless_insecure() {
        let bytes = sample_db().encode();
        let content = ContentRef {
            digest: oci_digest(&bytes),
            ..ContentRef::default()
        };
        assert!(verify(&content, &bytes, &[], None, true).is_ok());
        assert!(matches!(
            verify(&content, &bytes, &[], None, false),
            Err(FeedError::Unsigned)
        ));
        assert!(matches!(
            verify(&content, &bytes, &[], Some("k"), false),
            Err(FeedError::Unsigned)
        ));
        assert!(matches!(
            verify(&content, &bytes, b"sig", Some("k"), false),
            Err(FeedError::Content(_))
        ));
        let wrong = ContentRef {
            digest: "sha256:00".into(),
            ..ContentRef::default()
        };
        assert!(verify(&wrong, &bytes, &[], None, true).is_err());
    }
}
