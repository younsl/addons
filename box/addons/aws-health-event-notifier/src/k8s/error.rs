use thiserror::Error;

#[derive(Debug, Error)]
pub enum K8sError {
    /// The object changed or already exists. Another writer won the race.
    #[error("conflict on {0}")]
    Conflict(String),

    #[error("kubernetes api: {0}")]
    Api(#[from] kube::Error),
}

pub type K8sResult<T> = Result<T, K8sError>;

/// Maps both `AlreadyExists` and `Conflict` (HTTP 409) onto [`K8sError::Conflict`].
pub fn conflict_or(e: kube::Error, what: &str) -> K8sError {
    match &e {
        kube::Error::Api(status) if status.code == 409 => K8sError::Conflict(what.to_string()),
        _ => K8sError::Api(e),
    }
}
