//! Resolves gateway Name tags. `DescribeVpnConnections` returns their IDs only,
//! and an approver reading a card at 02:00 recognizes "prod-tgw", not
//! "tgw-0abcdef1234567890".

use std::collections::HashMap;
use std::sync::Mutex;

use aws_sdk_ec2::types::Tag;
use tracing::warn;

use super::client::{ApiError, Client};
use super::types::Connection;

/// Caches gateway ID to Name tag.
///
/// Every ID that was looked up is cached, including the ones that resolved to
/// nothing, so a gateway without a Name tag and a missing IAM permission both
/// cost one call per process rather than one per reconcile pass. The price is
/// that renaming a gateway takes effect on the next restart.
#[derive(Debug, Default)]
pub struct GatewayNames {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    by_id: HashMap<String, String>,
    warned: bool,
}

impl GatewayNames {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn known(&self, id: &str) -> bool {
        self.lock().by_id.contains_key(id)
    }

    fn get(&self, id: &str) -> String {
        if id.is_empty() {
            return String::new();
        }
        self.lock().by_id.get(id).cloned().unwrap_or_default()
    }

    fn set(&self, id: &str, name: String) {
        if !id.is_empty() {
            self.lock().by_id.insert(id.to_string(), name);
        }
    }

    /// Marks every requested ID as looked up, so a gateway the call did not
    /// return, or one with no Name tag, is not asked for again.
    fn fill(&self, ids: &[String]) {
        let mut g = self.lock();
        for id in ids {
            g.by_id.entry(id.clone()).or_default();
        }
    }

    /// Records a failed lookup and warns once. Warning per pass would turn a
    /// missing optional permission into a log flood.
    fn miss(&self, action: &str, ids: &[String], err: &ApiError) {
        let warn_now = {
            let mut g = self.lock();
            let first = !g.warned;
            g.warned = true;
            for id in ids {
                g.by_id.insert(id.clone(), String::new());
            }
            first
        };
        if warn_now {
            warn!(
                action,
                error = %err,
                hint = format!("grant {action} to name the gateways on the approval card"),
                "could not read gateway Name tags; notifications will show gateway IDs only"
            );
        }
    }

    /// Adds an ID that is worth a lookup: non-empty, not already cached, and
    /// not already queued in this batch.
    fn queue(&self, ids: &mut Vec<String>, id: &str) {
        if id.is_empty() || self.known(id) || ids.iter().any(|x| x == id) {
            return;
        }
        ids.push(id.to_string());
    }
}

impl Client {
    /// Fills in the Name tags of every gateway the connections reference. Names
    /// are cosmetic, so a failure degrades to the bare ID rather than failing
    /// discovery.
    pub(super) async fn resolve_gateway_names(&self, conns: &mut [Connection]) {
        let Some(api) = &self.gw_api else {
            return;
        };
        let cache = &self.gateways;

        let (mut transit, mut vpn, mut customer) = (Vec::new(), Vec::new(), Vec::new());
        for conn in conns.iter() {
            cache.queue(&mut transit, &conn.transit_gateway_id);
            cache.queue(&mut vpn, &conn.vpn_gateway_id);
            cache.queue(&mut customer, &conn.customer_gateway_id);
        }

        if !transit.is_empty() {
            match api.describe_transit_gateways(transit.clone()).await {
                Err(err) => cache.miss("ec2:DescribeTransitGateways", &transit, &err),
                Ok(out) => {
                    for tgw in out.transit_gateways() {
                        cache.set(
                            tgw.transit_gateway_id().unwrap_or_default(),
                            name_tag(tgw.tags()),
                        );
                    }
                    cache.fill(&transit);
                }
            }
        }
        if !vpn.is_empty() {
            match api.describe_vpn_gateways(vpn.clone()).await {
                Err(err) => cache.miss("ec2:DescribeVpnGateways", &vpn, &err),
                Ok(out) => {
                    for vgw in out.vpn_gateways() {
                        cache.set(
                            vgw.vpn_gateway_id().unwrap_or_default(),
                            name_tag(vgw.tags()),
                        );
                    }
                    cache.fill(&vpn);
                }
            }
        }
        if !customer.is_empty() {
            match api.describe_customer_gateways(customer.clone()).await {
                Err(err) => cache.miss("ec2:DescribeCustomerGateways", &customer, &err),
                Ok(out) => {
                    for cgw in out.customer_gateways() {
                        cache.set(
                            cgw.customer_gateway_id().unwrap_or_default(),
                            name_tag(cgw.tags()),
                        );
                    }
                    cache.fill(&customer);
                }
            }
        }

        for conn in conns.iter_mut() {
            conn.transit_gateway_name = cache.get(&conn.transit_gateway_id);
            conn.vpn_gateway_name = cache.get(&conn.vpn_gateway_id);
            conn.customer_gateway_name = cache.get(&conn.customer_gateway_id);
        }
    }
}

fn name_tag(tags: &[Tag]) -> String {
    tags.iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(|t| t.value())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::client::fake::*;
    use super::*;
    use crate::aws::DiscoverInput;

    fn client(gw: FakeGateways) -> (Client, Arc<FakeGateways>) {
        let ec2 = Arc::new(FakeEc2::default());
        let mut vgw = vpn_connection("vpn-2", "", [true, true]);
        vgw.transit_gateway_id = None;
        vgw.vpn_gateway_id = Some("vgw-1".into());
        *ec2.connections.lock().unwrap() = vec![vpn_connection("vpn-1", "prod", [true, true]), vgw];
        let gw = Arc::new(gw);
        let c = Client::from_parts(Box::new(ec2), None, Some(Box::new(gw.clone())));
        (c, gw)
    }

    #[tokio::test]
    async fn resolves_names_once_and_caches_misses() {
        let (client, gw) = client(FakeGateways {
            transit: vec![("tgw-1".into(), "prod-tgw".into())],
            vpn: vec![("vgw-1".into(), String::new())],
            customer: vec![],
            ..FakeGateways::default()
        });
        let conns = client.discover(&DiscoverInput::default()).await.unwrap();
        assert_eq!(conns[0].transit_gateway_name, "prod-tgw");
        assert_eq!(conns[0].customer_gateway_name, "", "cgw-1 not returned");
        assert_eq!(conns[1].vpn_gateway_name, "", "no Name tag");
        assert_eq!(*gw.calls.lock().unwrap(), 3);

        // Second pass: everything is cached, no further calls.
        let conns = client.discover(&DiscoverInput::default()).await.unwrap();
        assert_eq!(conns[0].transit_gateway_name, "prod-tgw");
        assert_eq!(*gw.calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn failed_lookup_degrades_to_ids() {
        let (client, gw) = client(FakeGateways {
            fail: true,
            ..FakeGateways::default()
        });
        let conns = client.discover(&DiscoverInput::default()).await.unwrap();
        assert_eq!(conns[0].transit_gateway_id, "tgw-1");
        assert!(conns[0].transit_gateway_name.is_empty());
        assert_eq!(*gw.calls.lock().unwrap(), 3);
        client.discover(&DiscoverInput::default()).await.unwrap();
        assert_eq!(*gw.calls.lock().unwrap(), 3, "misses are cached too");
    }

    #[tokio::test]
    async fn no_gateway_api_means_no_names() {
        let ec2 = Arc::new(FakeEc2::default());
        *ec2.connections.lock().unwrap() = vec![vpn_connection("vpn-1", "prod", [true, true])];
        let client = Client::from_parts(Box::new(ec2), None, None);
        let conns = client.discover(&DiscoverInput::default()).await.unwrap();
        assert!(conns[0].transit_gateway_name.is_empty());
    }
}
