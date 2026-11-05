use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use aws_sdk_cloudwatch::error::DisplayErrorContext;
use aws_sdk_cloudwatch::primitives::DateTime;
use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat, ScanBy};

use super::error::AwsError;
use crate::source::MetricsSource;
use crate::types::NodeMetrics;

const NAMESPACE: &str = "AWS/ElastiCache";
const WINDOW: Duration = Duration::from_mins(15);
const PERIOD: i32 = 60;
const MAX_QUERIES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Items,
    Connections,
    Cpu,
    Memory,
}

impl Kind {
    const ALL: [Self; 4] = [Self::Items, Self::Connections, Self::Cpu, Self::Memory];

    const fn metric(self) -> &'static str {
        match self {
            Self::Items => "CurrItems",
            Self::Connections => "CurrConnections",
            Self::Cpu => "EngineCPUUtilization",
            Self::Memory => "DatabaseMemoryUsagePercentage",
        }
    }

    const fn apply(self, m: &mut NodeMetrics, v: f64) {
        match self {
            Self::Items => m.items = Some(v),
            Self::Connections => m.connections = Some(v),
            Self::Cpu => m.cpu = Some(v),
            Self::Memory => m.memory = Some(v),
        }
    }
}

pub struct CloudWatchMetrics {
    client: aws_sdk_cloudwatch::Client,
}

impl CloudWatchMetrics {
    pub const fn new(client: aws_sdk_cloudwatch::Client) -> Self {
        Self { client }
    }
}

/// Query ids must start with a lowercase letter, so they encode positions, not node ids.
fn query_id(node: usize, kind: usize) -> String {
    format!("n{node}k{kind}")
}

fn parse_query_id(id: &str) -> Option<(usize, usize)> {
    let (node, kind) = id.strip_prefix('n')?.split_once('k')?;
    Some((node.parse().ok()?, kind.parse().ok()?))
}

fn queries(nodes: &[String]) -> Vec<MetricDataQuery> {
    nodes
        .iter()
        .enumerate()
        .flat_map(|(n, node)| {
            Kind::ALL.iter().enumerate().map(move |(k, kind)| {
                let metric = Metric::builder()
                    .namespace(NAMESPACE)
                    .metric_name(kind.metric())
                    .dimensions(
                        Dimension::builder()
                            .name("CacheClusterId")
                            .value(node)
                            .build(),
                    )
                    .build();
                MetricDataQuery::builder()
                    .id(query_id(n, k))
                    .metric_stat(
                        MetricStat::builder()
                            .metric(metric)
                            .period(PERIOD)
                            .stat("Average")
                            .build(),
                    )
                    .return_data(true)
                    .build()
            })
        })
        .collect()
}

impl MetricsSource for CloudWatchMetrics {
    type Error = AwsError;

    async fn latest(&self, nodes: Vec<String>) -> Result<HashMap<String, NodeMetrics>, AwsError> {
        let now = SystemTime::now();
        let start = DateTime::from(now - WINDOW);
        let end = DateTime::from(now);
        let mut out: HashMap<String, NodeMetrics> = HashMap::new();
        for chunk in queries(&nodes).chunks(MAX_QUERIES) {
            let mut pages = self
                .client
                .get_metric_data()
                .set_metric_data_queries(Some(chunk.to_vec()))
                .start_time(start)
                .end_time(end)
                .scan_by(ScanBy::TimestampDescending)
                .into_paginator()
                .send();
            while let Some(page) = pages.next().await {
                let page =
                    page.map_err(|e| AwsError::Metrics(DisplayErrorContext(e).to_string()))?;
                for r in page.metric_data_results() {
                    let (Some((n, k)), Some(v)) =
                        (r.id().and_then(parse_query_id), r.values().first())
                    else {
                        continue;
                    };
                    if let (Some(node), Some(kind)) = (nodes.get(n), Kind::ALL.get(k)) {
                        kind.apply(out.entry(node.clone()).or_default(), *v);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_ids_round_trip() {
        assert_eq!(parse_query_id(&query_id(12, 3)), Some((12, 3)));
        assert_eq!(parse_query_id("x1k2"), None);
        assert_eq!(parse_query_id("n1"), None);
    }

    #[test]
    fn four_queries_per_node() {
        let q = queries(&["a-001".into(), "a-002".into()]);
        assert_eq!(q.len(), 8);
        assert_eq!(q[5].id(), Some("n1k1"));
        let names: Vec<_> = Kind::ALL.iter().map(|k| k.metric()).collect();
        assert_eq!(
            names,
            [
                "CurrItems",
                "CurrConnections",
                "EngineCPUUtilization",
                "DatabaseMemoryUsagePercentage"
            ]
        );
    }

    #[test]
    fn kinds_fill_their_field() {
        let mut m = NodeMetrics::default();
        for (i, k) in Kind::ALL.iter().enumerate() {
            k.apply(&mut m, f64::from(u8::try_from(i).unwrap()));
        }
        assert_eq!(
            (m.items, m.connections, m.cpu, m.memory),
            (Some(0.0), Some(1.0), Some(2.0), Some(3.0))
        );
    }
}
