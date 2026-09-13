//! Proves at startup that this Pod can actually do its job.

use thiserror::Error;

use super::client::{ApiError, Client};
use super::types::{Connection, DiscoverInput};

/// What the startup check learned about this Pod's AWS access.
#[derive(Debug, Clone, Default)]
pub struct Access {
    /// The assumed role's ARN, which is what tells IRSA apart from EKS Pod
    /// Identity apart from a node role picked up by accident.
    pub identity: String,
    pub account: String,
    /// The managed connections the read call returned. Empty is a tag-filter
    /// problem or an empty account, not a permission problem.
    pub connections: Vec<Connection>,
}

/// A startup check that failed.
#[derive(Debug, Error)]
pub enum AccessError {
    #[error("STS client is not configured, so AWS credentials cannot be verified")]
    NoSts,
    /// The default chain produced nothing usable. In a Pod that means neither
    /// IRSA nor EKS Pod Identity is wired up.
    #[error(
        "no usable AWS credentials: sts:GetCallerIdentity failed, so neither IRSA nor EKS Pod Identity is providing credentials to this Pod: {0}"
    )]
    NoCredentials(ApiError),
    #[error(
        "ec2:DescribeVpnConnections failed as {identity}, so managed connections cannot be discovered: {source}"
    )]
    Discover { identity: String, source: ApiError },
    #[error(
        "ec2:GetVpnTunnelReplacementStatus failed for {connection} as {identity}, so pending maintenance cannot be read: {source}"
    )]
    Status {
        connection: String,
        identity: String,
        source: ApiError,
    },
}

impl Client {
    /// Verifies access at startup instead of discovering a missing permission
    /// when maintenance is first queued.
    ///
    /// The three calls mirror the three the controller depends on: credentials
    /// resolve at all, the managed connections are readable, and their
    /// maintenance status is readable. `ReplaceVpnTunnel` is deliberately not
    /// probed: there is no way to test it that does not either change
    /// something or depend on maintenance being queued right now, which is
    /// why `dryRun` exists.
    pub async fn verify_access(&self, input: &DiscoverInput) -> Result<Access, AccessError> {
        let sts = self.sts.as_ref().ok_or(AccessError::NoSts)?;
        let (identity, account) = sts
            .caller_identity()
            .await
            .map_err(AccessError::NoCredentials)?;
        let mut access = Access {
            identity,
            account,
            connections: Vec::new(),
        };

        access.connections =
            self.discover(input)
                .await
                .map_err(|source| AccessError::Discover {
                    identity: access.identity.clone(),
                    source,
                })?;
        // Readable but empty is not a permission failure, and not this
        // function's call to make fatal.
        let Some(first) = access.connections.first() else {
            return Ok(access);
        };
        // One connection is enough to prove the permission.
        self.statuses(first)
            .await
            .map_err(|source| AccessError::Status {
                connection: first.id.clone(),
                identity: access.identity.clone(),
                source,
            })?;
        Ok(access)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::client::fake::*;
    use super::*;

    fn identity() -> FakeSts {
        FakeSts(Ok((
            "arn:aws:sts::123456789012:assumed-role/x".into(),
            "123456789012".into(),
        )))
    }

    #[tokio::test]
    async fn verifies_all_three_calls() {
        let ec2 = Arc::new(FakeEc2::default());
        *ec2.connections.lock().unwrap() = vec![vpn_connection("vpn-1", "prod", [true, true])];
        let client = Client::from_parts(Box::new(ec2.clone()), Some(Box::new(identity())), None);
        let access = client
            .verify_access(&DiscoverInput::default())
            .await
            .unwrap();
        assert_eq!(access.account, "123456789012");
        assert!(access.identity.starts_with("arn:aws:sts"));
        assert_eq!(access.connections.len(), 1);

        *ec2.status_error.lock().unwrap() = Some("denied".into());
        let err = client
            .verify_access(&DiscoverInput::default())
            .await
            .unwrap_err();
        assert!(matches!(err, AccessError::Status { .. }), "{err}");
        assert!(
            err.to_string().contains("for vpn-1 as arn:aws:sts"),
            "{err}"
        );

        *ec2.describe_error.lock().unwrap() = Some("denied".into());
        let err = client
            .verify_access(&DiscoverInput::default())
            .await
            .unwrap_err();
        assert!(matches!(err, AccessError::Discover { .. }), "{err}");
    }

    #[tokio::test]
    async fn empty_account_is_not_an_error() {
        let ec2 = Arc::new(FakeEc2::default());
        let client = Client::from_parts(Box::new(ec2), Some(Box::new(identity())), None);
        let access = client
            .verify_access(&DiscoverInput::default())
            .await
            .unwrap();
        assert!(access.connections.is_empty());
    }

    #[tokio::test]
    async fn missing_credentials_and_sts() {
        let ec2 = Arc::new(FakeEc2::default());
        let client = Client::from_parts(Box::new(ec2.clone()), None, None);
        assert!(matches!(
            client.verify_access(&DiscoverInput::default()).await,
            Err(AccessError::NoSts)
        ));
        let client = Client::from_parts(
            Box::new(ec2),
            Some(Box::new(FakeSts(Err("no providers".into())))),
            None,
        );
        let err = client
            .verify_access(&DiscoverInput::default())
            .await
            .unwrap_err();
        assert!(
            err.to_string().starts_with("no usable AWS credentials"),
            "{err}"
        );
    }
}
