//! SSRF guard for client-supplied upstream URLs.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use url::Url;

/// Builds an HTTP client for URLs that originate from clients (e.g. the PyPI
/// base64url file refs) rather than from admin configuration.
///
pub(crate) fn new_public_only_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        // Redirects are followed by hand so credential headers can be dropped
        // when the host changes and every hop can be screened.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

/// The reason a destination address was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum DialError {
    #[error("refusing dial to unparseable address")]
    Unparseable,
    #[error("refusing dial to non-public address")]
    NonPublic,
}

/// Refuses loopback, private, unspecified, link-local and multicast
/// destinations. `address` is a `host:port` pair as it would reach a dialer.
pub(crate) fn public_only_dial_control(_network: &str, address: &str) -> Result<(), DialError> {
    let host = match address.parse::<SocketAddr>() {
        Ok(sa) => sa.ip(),
        Err(_) => {
            let host = match address.rsplit_once(':') {
                Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
                None => return Err(DialError::Unparseable),
            };
            host.parse::<IpAddr>().map_err(|_| DialError::Unparseable)?
        }
    };
    if is_public(host) {
        Ok(())
    } else {
        Err(DialError::NonPublic)
    }
}

fn is_public(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => !(v4.is_private() || v4.is_link_local()),
        IpAddr::V6(v6) => !(is_unique_local(v6) || is_link_local_v6(v6)),
    }
}

fn is_unique_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xfe00 == 0xfc00
}

/// `fe80::/10`, IPv6 link-local unicast.
fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xffc0 == 0xfe80
}

/// Resolves `url`'s host and refuses the request when any resolved address is
/// non-public. Called before every untrusted fetch and before every redirect hop
/// of one.
pub(crate) async fn guard_public_url(url: &Url) -> Result<(), DialError> {
    let host = url.host_str().ok_or(DialError::Unparseable)?;
    let port = url.port_or_known_default().unwrap_or(443);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_public(ip) {
            Ok(())
        } else {
            Err(DialError::NonPublic)
        };
    }
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| DialError::Unparseable)?;
    let mut any = false;
    for addr in addrs {
        any = true;
        public_only_dial_control("tcp", &addr.to_string())?;
    }
    if any {
        Ok(())
    } else {
        Err(DialError::Unparseable)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::repo::pypi::same_host;
    use crate::repo::safedial::public_only_dial_control;

    #[test]
    fn public_only_dial_control_screens_destinations() {
        let blocked = [
            "127.0.0.1:80",
            "10.0.0.5:443",
            "172.16.1.1:80",
            "192.168.1.1:8080",
            "169.254.169.254:80",
            "0.0.0.0:80",
            "[::1]:80",
            "[fd00::1]:443",
            "[fe80::1]:80",
            "[::ffff:127.0.0.1]:80",
        ];
        for addr in blocked {
            assert!(
                public_only_dial_control("tcp", addr).is_err(),
                "dial to {addr} allowed, want blocked"
            );
        }
        let allowed = [
            "93.184.216.34:443",
            "[2606:2800:220:1:248:1893:25c8:1946]:443",
        ];
        for addr in allowed {
            assert!(
                public_only_dial_control("tcp", addr).is_ok(),
                "dial to {addr} blocked"
            );
        }
    }

    #[test]
    fn same_host_compares_case_insensitively() {
        let parse = |s: &str| url::Url::parse(s).unwrap_or_else(|e| panic!("parse {s}: {e}"));
        assert!(
            same_host(
                &parse("https://PyPI.org/packages/x.whl"),
                "https://pypi.org/simple"
            ),
            "case-insensitive same host rejected"
        );
        assert!(
            !same_host(
                &parse("https://files.pythonhosted.org/x.whl"),
                "https://pypi.org/simple"
            ),
            "different host accepted as same"
        );
        assert!(
            !same_host(&parse("https://pypi.org/x.whl"), "://bad"),
            "unparseable upstream accepted"
        );
    }
}
