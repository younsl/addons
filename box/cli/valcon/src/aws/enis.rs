use std::collections::HashMap;
use std::net::IpAddr;

use aws_sdk_ec2::error::DisplayErrorContext;
use aws_sdk_ec2::types::Filter;

use super::error::AwsError;
use crate::source::AddressSource;

const ENI_PREFIX: &str = "ElastiCache ";

/// `ElastiCache` creates one ENI per node with the description `ElastiCache <node-id>`.
pub struct EniAddresses {
    client: aws_sdk_ec2::Client,
}

impl EniAddresses {
    pub const fn new(client: aws_sdk_ec2::Client) -> Self {
        Self { client }
    }
}

impl AddressSource for EniAddresses {
    type Error = AwsError;

    async fn node_ips(&self) -> Result<HashMap<String, IpAddr>, AwsError> {
        let filter = Filter::builder()
            .name("description")
            .values(format!("{ENI_PREFIX}*"))
            .build();
        let mut stream = self
            .client
            .describe_network_interfaces()
            .filters(filter)
            .into_paginator()
            .items()
            .send();
        let mut pairs = Vec::new();
        while let Some(item) = stream.next().await {
            let eni =
                item.map_err(|e| AwsError::DescribeEnis(DisplayErrorContext(e).to_string()))?;
            if let (Some(d), Some(ip)) = (eni.description(), eni.private_ip_address()) {
                pairs.push((d.to_owned(), ip.to_owned()));
            }
        }
        Ok(addresses(&pairs))
    }
}

fn addresses(pairs: &[(String, String)]) -> HashMap<String, IpAddr> {
    pairs
        .iter()
        .filter_map(|(desc, ip)| {
            Some((desc.strip_prefix(ENI_PREFIX)?.to_owned(), ip.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_cache_enis_with_valid_ips() {
        let pairs = [
            ("ElastiCache orders-001".to_owned(), "10.0.0.1".to_owned()),
            ("RDSNetworkInterface".to_owned(), "10.0.0.2".to_owned()),
            ("ElastiCache bad-001".to_owned(), "nope".to_owned()),
        ];
        let map = addresses(&pairs);
        assert_eq!(map.len(), 1);
        assert_eq!(map["orders-001"], "10.0.0.1".parse::<IpAddr>().unwrap());
    }
}
