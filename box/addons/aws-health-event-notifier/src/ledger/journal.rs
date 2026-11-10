//! The ledger plus where it is kept: process memory for a single replica, or a
//! `ConfigMap` shared by every replica under leader election.
//!
//! Every change goes through [`Journal::commit`], which persists before it
//! applies. With the `ConfigMap` backend that write is a compare-and-swap, so a
//! replica that lost the ledger to a newer leader gets [`JournalError::Fenced`]
//! and can never record, and therefore never post, another send.

use thiserror::Error;

use super::{Abandoned, Ledger};
use crate::k8s::error::K8sError;
use crate::k8s::ledger_store::LedgerStore;

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("ledger was claimed by another replica")]
    Fenced,

    #[error("ledger store: {0}")]
    Store(K8sError),

    #[error("ledger encode: {0}")]
    Encode(#[from] serde_json::Error),
}

impl From<K8sError> for JournalError {
    fn from(e: K8sError) -> Self {
        match e {
            K8sError::Conflict(_) => Self::Fenced,
            other @ K8sError::Api(_) => Self::Store(other),
        }
    }
}

enum Backend {
    Memory,
    ConfigMap {
        store: LedgerStore,
        resource_version: Option<String>,
    },
}

pub struct Journal {
    ledger: Ledger,
    backend: Backend,
}

impl Journal {
    pub fn memory() -> Self {
        Self {
            ledger: Ledger::default(),
            backend: Backend::Memory,
        }
    }

    pub fn configmap(store: LedgerStore) -> Self {
        Self {
            ledger: Ledger::default(),
            backend: Backend::ConfigMap {
                store,
                resource_version: None,
            },
        }
    }

    pub const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Applies `f` to a copy, persists it, then adopts it.
    pub async fn commit<F: FnOnce(&mut Ledger)>(&mut self, f: F) -> Result<(), JournalError> {
        let mut next = self.ledger.clone();
        f(&mut next);
        self.persist(&next).await?;
        self.ledger = next;
        Ok(())
    }

    /// Loads the stored ledger and claims it for `holder` under a new term,
    /// marking the previous holder's in-flight sends `Unknown`.
    pub async fn claim(&mut self, holder: &str) -> Result<Vec<Abandoned>, JournalError> {
        self.reload().await?;
        let mut next = self.ledger.clone();
        next.term += 1;
        next.holder = Some(holder.to_string());
        let abandoned = next.abandon_pending();
        self.persist(&next).await?;
        self.ledger = next;
        Ok(abandoned)
    }

    /// After a conflict, reloads and reports whether the ledger is still held
    /// by `holder` under the same term. If so, writes can continue.
    pub async fn resync(&mut self, holder: &str) -> Result<bool, JournalError> {
        let term = self.ledger.term;
        self.reload().await?;
        Ok(self.ledger.holder.as_deref() == Some(holder) && self.ledger.term == term)
    }

    async fn reload(&mut self) -> Result<(), JournalError> {
        let Backend::ConfigMap {
            store,
            resource_version,
        } = &mut self.backend
        else {
            return Ok(());
        };
        let Some(stored) = store.load().await.map_err(JournalError::Store)? else {
            self.ledger = Ledger::default();
            *resource_version = None;
            return Ok(());
        };
        self.ledger = if stored.payload.is_empty() {
            Ledger::default()
        } else {
            serde_json::from_str(&stored.payload).unwrap_or_else(|e| {
                tracing::error!(error = %e, "stored ledger is unreadable; starting from an empty ledger");
                Ledger::default()
            })
        };
        *resource_version = Some(stored.resource_version);
        Ok(())
    }

    async fn persist(&mut self, next: &Ledger) -> Result<(), JournalError> {
        let Backend::ConfigMap {
            store,
            resource_version,
        } = &mut self.backend
        else {
            return Ok(());
        };
        let payload = serde_json::to_string(next)?;
        let rv = match resource_version.as_deref() {
            Some(rv) => store.replace(&payload, rv).await?,
            None => store.create(&payload).await?,
        };
        *resource_version = Some(rv);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;
    use crate::k8s::fake::FakeApiServer;
    use crate::ledger::DeliveryState;

    const PATH: &str = "/api/v1/namespaces/ns/configmaps/ledger";

    fn journal(server: &FakeApiServer) -> Journal {
        Journal::configmap(LedgerStore::new(server.client(), "ns", "ledger"))
    }

    #[tokio::test]
    async fn memory_commit_and_claim() {
        let mut j = Journal::memory();
        j.commit(|l| l.set_event("arn:1", Some(Utc::now()), DeliveryState::Pending))
            .await
            .unwrap();
        let abandoned = j.claim("pod-a").await.unwrap();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(j.ledger().term, 1);
        assert!(j.resync("pod-a").await.unwrap());
    }

    #[tokio::test]
    async fn first_claim_creates_ledger() {
        let server = FakeApiServer::start().await;
        let mut j = journal(&server);
        assert_eq!(j.claim("pod-a").await.unwrap(), Vec::<Abandoned>::new());
        assert_eq!(j.ledger().term, 1);
        assert!(server.object(PATH).is_some());
    }

    #[tokio::test]
    async fn new_leader_fences_old_one() {
        let server = FakeApiServer::start().await;
        let mut a = journal(&server);
        let mut b = journal(&server);
        a.claim("pod-a").await.unwrap();
        a.commit(|l| l.set_event("arn:1", Some(Utc::now()), DeliveryState::Pending))
            .await
            .unwrap();

        let abandoned = b.claim("pod-b").await.unwrap();
        assert_eq!(abandoned[0].arn, "arn:1");
        assert_eq!(b.ledger().term, 2);
        assert_eq!(
            b.ledger().event("arn:1").unwrap().state,
            DeliveryState::Unknown
        );

        let err = a
            .commit(|l| l.set_event("arn:2", Some(Utc::now()), DeliveryState::Pending))
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::Fenced));
        assert!(!a.resync("pod-a").await.unwrap());
        assert!(a.ledger().event("arn:2").is_none());
    }

    #[tokio::test]
    async fn resync_keeps_ownership_after_foreign_edit() {
        let server = FakeApiServer::start().await;
        let mut a = journal(&server);
        a.claim("pod-a").await.unwrap();
        let mut stored = server.object(PATH).unwrap();
        stored["metadata"]["labels"]["touched"] = json!("yes");
        server.put_object(PATH, stored);

        assert!(matches!(
            a.commit(|l| l.cursor = Some(Utc::now())).await.unwrap_err(),
            JournalError::Fenced
        ));
        assert!(a.resync("pod-a").await.unwrap());
        a.commit(|l| l.cursor = Some(Utc::now())).await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_payload_starts_fresh() {
        let server = FakeApiServer::start().await;
        server.put_object(
            PATH,
            json!({"metadata": {"name": "ledger"}, "data": {"ledger.json": "not json"}}),
        );
        let mut j = journal(&server);
        j.claim("pod-a").await.unwrap();
        assert_eq!(j.ledger().term, 1);
        assert!(j.ledger().cursor.is_none());
    }

    #[tokio::test]
    async fn store_failure_is_not_fencing() {
        let server = FakeApiServer::start().await;
        server.fail_all();
        let err = journal(&server).claim("pod-a").await.unwrap_err();
        assert!(matches!(err, JournalError::Store(_)));
    }
}
