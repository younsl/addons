use thiserror::Error;

#[derive(Debug, Error)]
pub enum AwsError {
    #[error("elasticache:DescribeReplicationGroups failed: {0}")]
    DescribeGroups(String),

    #[error("ec2:DescribeNetworkInterfaces failed: {0}")]
    DescribeEnis(String),

    #[error("cloudwatch:GetMetricData failed: {0}")]
    Metrics(String),
}
