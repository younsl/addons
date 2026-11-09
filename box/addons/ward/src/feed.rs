//! The signature database feed: building it from public sources, checking
//! it against a false-positive corpus, and pulling it from an OCI registry.

pub mod build;
pub mod oci;
mod platform;
pub mod sigstore;

use crate::content::{ContentError, FeedDb, check_digest, verify_signature};
use crate::crd::FeedSpec;
use crate::rollout::ContentRef;

/// How a feed revision proves where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// A cosign signature that verifies with this PEM public key.
    Key(String),
    /// A cosign keyless bundle signed by this OIDC identity.
    Keyless(sigstore::Identity),
    /// No check, for development only.
    Insecure,
    /// Nothing configured, so every revision is refused.
    Missing,
}

impl Trust {
    pub fn of(spec: Option<&FeedSpec>) -> Self {
        let Some(s) = spec else {
            return Self::Missing;
        };
        if s.insecure_skip_verify {
            return Self::Insecure;
        }
        if let Some(k) = &s.keyless {
            return Self::Keyless(sigstore::Identity {
                issuer: k.issuer.clone(),
                subject: k.identity.clone(),
            });
        }
        s.public_key.clone().map_or(Self::Missing, Self::Key)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error(transparent)]
    Oci(#[from] oci::OciError),
    #[error(transparent)]
    Content(#[from] ContentError),
    #[error("feed has no signature layer and insecureSkipVerify is false")]
    Unsigned,
    #[error("keyless signature: {0}")]
    Keyless(#[from] sigstore::SigstoreError),
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
    trust: &Trust,
) -> Result<FeedDb, FeedError> {
    check_digest(db_bytes, &content.digest)?;
    match trust {
        Trust::Insecure => {}
        Trust::Missing => return Err(FeedError::Unsigned),
        _ if sig_bytes.is_empty() => return Err(FeedError::Unsigned),
        Trust::Key(key) => verify_signature(db_bytes, sig_bytes, key)?,
        Trust::Keyless(id) => {
            let v = sigstore::verify(db_bytes, sig_bytes, id)?;
            tracing::debug!(subject = %v.subject, log_index = v.log_index, "keyless signature verified");
        }
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
    let db = verify(&content, &db_bytes, &sig_bytes, &Trust::of(Some(spec)))?;
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
        assert!(verify(&content, &bytes, &[], &Trust::Insecure).is_ok());
        assert!(matches!(
            verify(&content, &bytes, &[], &Trust::Missing),
            Err(FeedError::Unsigned)
        ));
        assert!(matches!(
            verify(&content, &bytes, &[], &Trust::Key("k".into())),
            Err(FeedError::Unsigned)
        ));
        assert!(matches!(
            verify(&content, &bytes, b"sig", &Trust::Key("k".into())),
            Err(FeedError::Content(_))
        ));
        let keyless = Trust::Keyless(sigstore::Identity {
            issuer: "https://token.actions.githubusercontent.com".into(),
            subject:
                "https://github.com/younsl/addons/.github/workflows/ward-db.yml@refs/heads/main"
                    .into(),
        });
        assert!(matches!(
            verify(&content, &bytes, b"{}", &keyless),
            Err(FeedError::Keyless(_))
        ));
        let wrong = ContentRef {
            digest: "sha256:00".into(),
            ..ContentRef::default()
        };
        assert!(verify(&wrong, &bytes, &[], &Trust::Insecure).is_err());

        let mut spec: FeedSpec = serde_json::from_value(serde_json::json!({
            "image": "ghcr.io/younsl/ward-db:latest",
            "keyless": {"identity": "https://github.com/younsl/addons/.github/workflows/ward-db.yml@refs/heads/main"},
        }))
        .unwrap();
        assert_eq!(Trust::of(Some(&spec)), keyless);
        spec.keyless = None;
        assert_eq!(Trust::of(Some(&spec)), Trust::Missing);
        spec.public_key = Some("pem".into());
        assert_eq!(Trust::of(Some(&spec)), Trust::Key("pem".into()));
        spec.insecure_skip_verify = true;
        assert_eq!(Trust::of(Some(&spec)), Trust::Insecure);
        assert_eq!(Trust::of(None), Trust::Missing);
    }
}
