//! HA fencing relies on failed `If-Match` / `If-None-Match` writes being
//! rejected with 412. Some S3-compatible stores (Garage) accept and ignore the
//! headers, so enforcement is measured at startup rather than assumed.

use std::collections::HashMap;

use super::metasync::{Error, ObjectApi, PutBody, PutObjectInput, Result};

const MISMATCHED_ETAG: &str = "\"forklift-conditional-write-probe\"";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionalWrites {
    Enforced,
    Ignored { reason: String },
}

impl ConditionalWrites {
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Enforced)
    }
}

/// `key` must be unique per caller: two pods probing the same key would see
/// each other's object.
pub async fn probe_conditional_writes(
    api: &dyn ObjectApi,
    bucket: &str,
    key: &str,
) -> Result<ConditionalWrites> {
    let result = run_probe(api, bucket, key).await;
    if let Err(err) = api.delete_object(bucket, key).await {
        tracing::warn!(key, err = %err, "remove conditional write probe object");
    }
    result
}

async fn run_probe(api: &dyn ObjectApi, bucket: &str, key: &str) -> Result<ConditionalWrites> {
    let put = |if_match: Option<&str>, if_none_match: Option<&str>| PutObjectInput {
        bucket: bucket.to_string(),
        key: key.to_string(),
        body: PutBody::Bytes(b"probe".to_vec()),
        content_length: 5,
        metadata: HashMap::new(),
        if_match: if_match.map(str::to_owned),
        if_none_match: if_none_match.map(str::to_owned),
    };

    api.put_object(put(None, Some("*")))
        .await
        .map_err(|e| e.context("conditional write probe: create"))?;

    match api.put_object(put(None, Some("*"))).await {
        Err(Error::PreconditionFailed(_)) => {}
        Ok(_) => {
            return Ok(ConditionalWrites::Ignored {
                reason: "If-None-Match: * overwrote an existing object".into(),
            });
        }
        Err(e) => return Err(e.context("conditional write probe: If-None-Match")),
    }

    match api.put_object(put(Some(MISMATCHED_ETAG), None)).await {
        Err(Error::PreconditionFailed(_)) => Ok(ConditionalWrites::Enforced),
        Ok(_) => Ok(ConditionalWrites::Ignored {
            reason: "If-Match with a stale ETag overwrote the object".into(),
        }),
        Err(e) => Err(e.context("conditional write probe: If-Match")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;

    use crate::objstore::condprobe::*;
    use crate::objstore::{GetObjectOutput, HeadObjectOutput, PutObjectOutput};

    #[derive(Default)]
    struct Store {
        enforce_none_match: bool,
        enforce_match: bool,
        fail_create: bool,
        objects: parking_lot::Mutex<HashMap<String, String>>,
        puts: parking_lot::Mutex<usize>,
    }

    #[async_trait]
    impl ObjectApi for Store {
        async fn put_object(&self, input: PutObjectInput) -> Result<PutObjectOutput> {
            if self.fail_create {
                return Err(Error::ObjectStore("501 NotImplemented".into()));
            }
            let mut objects = self.objects.lock();
            let current = objects.get(&input.key).cloned();
            if self.enforce_none_match && input.if_none_match.is_some() && current.is_some() {
                return Err(Error::PreconditionFailed("exists".into()));
            }
            if self.enforce_match
                && let Some(want) = &input.if_match
                && current.as_ref() != Some(want)
            {
                return Err(Error::PreconditionFailed("etag".into()));
            }
            let mut n = self.puts.lock();
            *n += 1;
            let etag = format!("\"etag-{n}\"");
            objects.insert(input.key, etag.clone());
            Ok(PutObjectOutput { e_tag: Some(etag) })
        }

        async fn get_object(&self, _bucket: &str, _key: &str) -> Result<GetObjectOutput> {
            Err(Error::NotFound)
        }

        async fn head_object(&self, _bucket: &str, _key: &str) -> Result<HeadObjectOutput> {
            Err(Error::NotFound)
        }

        async fn delete_object(&self, _bucket: &str, key: &str) -> Result<()> {
            self.objects.lock().remove(key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn enforced_store_passes_and_is_cleaned_up() {
        let store = Store {
            enforce_none_match: true,
            enforce_match: true,
            ..Store::default()
        };
        let got = probe_conditional_writes(&store, "b", "probe")
            .await
            .unwrap();
        assert_eq!(got, ConditionalWrites::Enforced);
        assert!(got.is_enforced());
        assert!(store.objects.lock().is_empty(), "probe object left behind");
    }

    #[tokio::test]
    async fn ignored_if_none_match_is_reported() {
        let store = Store::default();
        let got = probe_conditional_writes(&store, "b", "probe")
            .await
            .unwrap();
        assert!(
            matches!(&got, ConditionalWrites::Ignored { reason } if reason.contains("If-None-Match")),
            "got {got:?}"
        );
        assert!(!got.is_enforced());
        assert!(store.objects.lock().is_empty(), "probe object left behind");
    }

    #[tokio::test]
    async fn ignored_if_match_is_reported() {
        let store = Store {
            enforce_none_match: true,
            ..Store::default()
        };
        let got = probe_conditional_writes(&store, "b", "probe")
            .await
            .unwrap();
        assert!(
            matches!(&got, ConditionalWrites::Ignored { reason } if reason.contains("If-Match")),
            "got {got:?}"
        );
    }

    #[tokio::test]
    async fn transport_failure_is_an_error() {
        let store = Store {
            fail_create: true,
            ..Store::default()
        };
        let err = probe_conditional_writes(&store, "b", "probe")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("create"), "err = {err}");
    }
}
