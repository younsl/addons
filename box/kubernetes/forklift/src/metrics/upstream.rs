//! Reachability of every proxy repository's upstream.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{FuturesUnordered, StreamExt};
use http::HeaderMap;
use parking_lot::Mutex;
use prometheus::Registry;
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use tokio_util::sync::CancellationToken;

use super::{desc, gauge_family};
use crate::repoconfig::UpstreamAuthConfig;

/// How often every proxy upstream is re-probed. One GET per upstream per
/// minute stays far below public registry rate limits while keeping
/// forklift_upstream_up fresh enough to alert on.
const PROBE_INTERVAL: Duration = Duration::from_secs(60);
/// Matches the console health check's probe client budget.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds parallel probes so a deployment with many proxy repositories cannot
/// fan a cycle out into an unbounded connection burst.
const PROBE_CONCURRENCY: usize = 4;

/// The subset of the metadata store the prober queries each cycle.
#[async_trait]
pub trait RepoLister: Send + Sync {
    /// All repositories, ordered by name.
    async fn list_repositories(&self) -> crate::meta::Result<Vec<crate::meta::Repository>>;
}

#[async_trait]
impl<T: super::Reader> RepoLister for T {
    async fn list_repositories(&self) -> crate::meta::Result<Vec<crate::meta::Repository>> {
        super::Reader::list_repositories(self).await
    }
}

/// Periodically probes every proxy repository's upstream URL and exposes the
/// last result as the forklift_upstream_up gauge. It runs on every pod, not
/// only the leader: reachability is a per-pod network property, and the
/// standby's view is exactly what an operator needs to judge a failover.
///
/// Probe semantics mirror the console's upstream health check: any HTTP
/// response, even a 4xx, means the upstream is reachable; only a transport
/// error (DNS, connect, TLS, timeout) counts as down.
pub struct UpstreamProber {
    store: Arc<dyn RepoLister>,
    client: reqwest::Client,

    /// Guards the last cycle's results, written by probe cycles and read on
    /// scrape. Each cycle replaces the whole map, so repositories deleted
    /// since the previous cycle drop out of the scrape instead of reporting a
    /// stale series.
    state: Mutex<HashMap<String, f64>>,

    up: Desc,
}

impl UpstreamProber {
    /// Builds the prober and registers it as a collector.
    pub fn new(store: Arc<dyn RepoLister>, registry: &Registry) -> Arc<UpstreamProber> {
        crate::server::install_crypto_provider();
        let p = Arc::new(UpstreamProber {
            store,
            // Probes carry the repository's upstream credentials, so redirects
            // are never followed: a redirecting upstream must not bounce them
            // elsewhere, and any HTTP response (a 3xx included) already proves
            // reachability.
            client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build upstream probe client"),
            state: Mutex::new(HashMap::new()),
            up: desc(
                "forklift_upstream_up",
                "1 if the proxy repository's upstream answered the last probe with any HTTP response, else 0.",
                &["repo"],
            ),
        });
        registry
            .register(Box::new(ProberCollector(Arc::clone(&p))))
            .expect("register forklift_upstream_up");
        p
    }

    /// Probes all upstreams immediately and then on every `PROBE_INTERVAL`
    /// tick until `cancel` fires.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        self.probe_all().await;
        let mut ticker = tokio::time::interval(PROBE_INTERVAL);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => self.probe_all().await,
            }
        }
    }

    /// Runs one probe cycle over every proxy repository and swaps in the fresh
    /// result set.
    pub(crate) async fn probe_all(&self) {
        let repos = match tokio::time::timeout(PROBE_TIMEOUT, self.store.list_repositories()).await
        {
            Ok(Ok(repos)) => repos,
            Ok(Err(e)) => {
                tracing::warn!(err = %e, "upstream probe: list repositories failed");
                return;
            }
            Err(_) => {
                tracing::warn!(
                    err = "context deadline exceeded",
                    "upstream probe: list repositories failed"
                );
                return;
            }
        };

        let mut targets = Vec::new();
        for r in repos {
            if r.r#type != crate::meta::TYPE_PROXY || r.upstream_url.is_empty() {
                continue;
            }
            // Probe with the repository's stored upstream credentials so an
            // auth-required upstream does not read as down; any HTTP response
            // (including 401 on a bad credential) still counts as reachable.
            let auth = crate::repoconfig::parse(&r.config_json)
                .map(|cfg| cfg.upstream_auth)
                .unwrap_or_default();
            targets.push((r.name, r.upstream_url, auth));
        }

        let mut next = HashMap::with_capacity(targets.len());
        let mut inflight = FuturesUnordered::new();
        let mut pending = targets.into_iter();
        loop {
            while inflight.len() < PROBE_CONCURRENCY {
                match pending.next() {
                    Some((name, url, auth)) => inflight.push(async move {
                        let v = self.probe(&url, &auth).await;
                        (name, v)
                    }),
                    None => break,
                }
            }
            match inflight.next().await {
                Some((name, v)) => {
                    next.insert(name, v);
                }
                None => break,
            }
        }

        *self.state.lock() = next;
    }

    /// Issues one GET against `raw_url` and reports 1 for any HTTP response.
    async fn probe(&self, raw_url: &str, auth: &UpstreamAuthConfig) -> f64 {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            match http::HeaderValue::from_str(&format!("forklift/{}", crate::version::VERSION)) {
                Ok(v) => v,
                Err(_) => return 0.0,
            },
        );
        auth.apply_headers(&mut headers);
        match self.client.get(raw_url).headers(headers).send().await {
            Ok(_) => 1.0,
            Err(_) => 0.0,
        }
    }

    /// Replaces the last cycle's results, for tests that need a starting
    /// state.
    #[cfg(test)]
    pub(crate) fn set_state(&self, state: HashMap<String, f64>) {
        *self.state.lock() = state;
    }
}

/// The collector face of the prober. The prober itself is shared as an `Arc`
/// (the probe loop and the registry both hold it), and `Registry::register`
/// takes an owned `Box<dyn Collector>`, so registration goes through this thin
/// wrapper instead of duplicating the state.
pub(crate) struct ProberCollector(pub(crate) Arc<UpstreamProber>);

impl Collector for ProberCollector {
    fn desc(&self) -> Vec<&Desc> {
        vec![&self.0.up]
    }

    /// Emits the last probe results.
    fn collect(&self) -> Vec<MetricFamily> {
        let state = self.0.state.lock();
        vec![gauge_family(
            &self.0.up,
            state
                .iter()
                .map(|(repo, v)| (vec![repo.clone()], *v))
                .collect(),
        )]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use prometheus::Registry;

    use crate::metrics::tests::{FakeReader, collect_and_count, encode, repo};
    use crate::metrics::upstream::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_prober() {
        // Reachable upstream: any HTTP response counts, even a 4xx.
        let up = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(403))
            .mount(&up)
            .await;
        let up_url = up.uri();
        let down = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let down_url = format!("http://{}", down.local_addr().unwrap());
        drop(down);

        let mut proxy_no_url = repo(4, "proxy-no-url", "npm", crate::meta::TYPE_PROXY);
        proxy_no_url.upstream_url = String::new();
        let mut npm_proxy = repo(1, "npm-proxy", "npm", crate::meta::TYPE_PROXY);
        npm_proxy.upstream_url = up_url;
        let mut maven_proxy = repo(2, "maven-proxy", "maven", crate::meta::TYPE_PROXY);
        maven_proxy.upstream_url = down_url;

        let reg = Registry::new();
        let p = UpstreamProber::new(
            Arc::new(FakeReader {
                repos: vec![
                    npm_proxy,
                    maven_proxy,
                    repo(3, "npm-hosted", "npm", crate::meta::TYPE_HOSTED),
                    proxy_no_url,
                ],
                ..Default::default()
            }),
            &reg,
        );
        p.probe_all().await;

        let want = r#"# HELP forklift_upstream_up 1 if the proxy repository's upstream answered the last probe with any HTTP response, else 0.
# TYPE forklift_upstream_up gauge
forklift_upstream_up{repo="maven-proxy"} 0
forklift_upstream_up{repo="npm-proxy"} 1
"#;
        assert_eq!(encode(&reg), want);
    }

    /// A failing repository listing must keep the previous cycle's results instead
    /// of wiping the gauge.
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_prober_list_error_keeps_state() {
        let reg = Registry::new();
        let p = UpstreamProber::new(
            Arc::new(FakeReader {
                err: true,
                ..Default::default()
            }),
            &reg,
        );
        p.set_state(HashMap::from([("npm-proxy".to_string(), 1.0)]));
        p.probe_all().await;
        assert_eq!(
            collect_and_count(&ProberCollector(Arc::clone(&p))),
            1,
            "expected previous state to survive a list error"
        );
    }
}
