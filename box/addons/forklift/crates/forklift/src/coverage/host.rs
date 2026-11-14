//! What may be used as the forklift host and as the GitLab base URL, and the
//! advisory checks the console runs against both.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A host label, per the DNS grammar. Anchored with a single bounded quantifier
/// so it cannot backtrack polynomially on adversarial input.
static LABEL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$").expect("label regex")
});

/// A top-level domain, per the DNS grammar.
static TLD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]{2,63}$").expect("tld regex"));

const MAX_HOST_LENGTH: usize = 253;
const MAX_PORT: i64 = 65535;

/// reserved_tlds never resolve on the public internet. They are rejected because
/// forklift is reached at an external domain: a name ending in one of these is
/// not an address a build outside the cluster could resolve, so an alias using
/// one is a mistake rather than a deployment forklift actually answers on.
static RESERVED_TLDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "local",
        "localhost",
        "internal",
        "svc",
        "cluster",
        "arpa",
        "home",
        "lan",
        "intranet",
        "test",
        "invalid",
        "example",
        "onion",
    ])
});

/// NormalizeHost strips a scheme and any trailing slashes, leaving a bare
/// "host" or "host:port".
pub fn normalize_host(raw: &str) -> String {
    let mut host = raw.trim();
    for prefix in ["https://", "http://"] {
        if host.len() >= prefix.len() && host[..prefix.len()].eq_ignore_ascii_case(prefix) {
            host = &host[prefix.len()..];
            break;
        }
    }
    host.trim_end_matches('/').to_string()
}

/// HostFromURL extracts the bare host from an absolute URL. This is how the
/// forklift host is derived from FORKLIFT_EXTERNAL_URL: the server already knows
/// what it is called, so nobody has to retype it.
pub fn host_from_url(raw: &str) -> String {
    let host = normalize_host(raw);
    match host.find(['/', '?', '#']) {
        Some(i) => host[..i].to_string(),
        None => host,
    }
}

/// ValidateMatchHost returns a message describing why `raw` is not usable as a
/// forklift host, or the empty string when it is.
///
/// A forklift is reached at an external host domain, which is what makes the
/// rule strict: at least two labels with a letters-only last one, so IP
/// literals, single-label names and the reserved suffixes above are all refused.
/// Those names cannot be the address a build resolves through from outside the
/// cluster, so accepting one would only ever add a pattern nothing matches while
/// reading like the alias had been configured.
pub fn validate_match_host(raw: &str) -> String {
    let host = normalize_host(raw);
    if host.is_empty() {
        return "host is required".to_string();
    }
    if host.contains(['/', '@', '?', '#', '*', ',', ' ', '\t']) {
        return "enter a bare host such as forklift.example.com, with no scheme, path or wildcard"
            .to_string();
    }
    let (name, port, has_port) = match host.split_once(':') {
        Some((a, b)) => (a, b, true),
        None => (host.as_str(), "", false),
    };
    if has_port {
        match port.parse::<i64>() {
            Ok(n) if (1..=MAX_PORT).contains(&n) => {}
            _ => return "port is out of range".to_string(),
        }
    }
    if name.is_empty() || name.len() > MAX_HOST_LENGTH {
        return "host is too long".to_string();
    }
    if !is_external_domain(name) {
        return format!(
            "'{raw}' is not an external domain name; forklift is reached at one, such as forklift.example.com"
        );
    }
    String::new()
}

/// IsExternalDomain reports whether `name` is a public domain name: two or more
/// DNS labels ending in a letters-only, non-reserved top-level domain. It takes
/// a name without a port.
pub fn is_external_domain(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let tld = labels[labels.len() - 1];
    if !TLD_RE.is_match(tld) || RESERVED_TLDS.contains(tld.to_lowercase().as_str()) {
        return false;
    }
    labels.iter().all(|label| LABEL_RE.is_match(label))
}

/// HostCheck is the outcome of checking a forklift host: whether it is shaped
/// like one, and whether it resolves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCheck {
    pub host: String,
    /// SyntaxError is empty when the name could be a forklift address.
    pub syntax_error: String,
    /// Resolved reports whether DNS returned at least one address. It is
    /// advisory only: see [`lookup_host`].
    pub resolved: bool,
    /// Addresses are the resolved IPs, capped for display.
    pub addresses: Vec<String>,
    /// ResolveError explains a failed lookup in the resolver's own words.
    pub resolve_error: String,
    pub latency_ms: i64,
}

/// dns_timeout bounds one lookup. The console calls this as an administrator
/// types, so a resolver that is not answering must fail fast rather than hold
/// the request open.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// max_resolved_addresses caps what is reported back. A name behind a large pool
/// answers with dozens of records, and the console only needs enough to show the
/// lookup worked.
const MAX_RESOLVED_ADDRESSES: usize = 4;

/// LookupHost resolves the name in a forklift host and reports what happened.
///
/// A DNS lookup is the strongest check available here that does not involve
/// talking to the host. Nothing is sent to it: only the resolver is asked, so
/// this cannot be turned into a request forwarder the way an HTTP probe could.
///
/// The result is deliberately advisory. Forklift resolves names from inside the
/// cluster, and the builds this measures resolve them from wherever they run, so
/// a name that does not resolve here may resolve perfectly well for them under
/// split-horizon DNS. A failed lookup is worth showing an administrator and is
/// not grounds for refusing the setting.
pub async fn lookup_host(raw: &str) -> HostCheck {
    let host = normalize_host(raw);
    let mut out = HostCheck {
        host: host.clone(),
        addresses: Vec::new(),
        ..HostCheck::default()
    };
    let msg = validate_match_host(&host);
    if !msg.is_empty() {
        out.syntax_error = msg;
        return out;
    }
    let name = match host.split_once(':') {
        Some((a, _)) => a.to_string(),
        None => host.clone(),
    };

    let started = Instant::now();
    // Port 0 keeps the lookup a name resolution rather than a service lookup;
    // only the addresses are read back.
    let resolved =
        tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((name.clone(), 0u16))).await;
    out.latency_ms = started.elapsed().as_millis() as i64;
    let addrs = match resolved {
        Err(_) => {
            out.resolve_error = format!("lookup {name}: i/o timeout");
            return out;
        }
        Ok(Err(e)) => {
            out.resolve_error = format!("lookup {name}: {e}");
            return out;
        }
        Ok(Ok(iter)) => iter,
    };

    let mut addresses: Vec<String> = Vec::new();
    for addr in addrs {
        let ip = addr.ip().to_string();
        if !addresses.contains(&ip) {
            addresses.push(ip);
        }
    }
    if addresses.len() > MAX_RESOLVED_ADDRESSES {
        addresses.truncate(MAX_RESOLVED_ADDRESSES);
    }
    out.resolved = !addresses.is_empty();
    out.addresses = addresses;
    out
}

/// GitLabCheck is the outcome of asking the configured GitLab instance who it
/// is.
///
/// It verifies the URL and the token together, which is what an operator
/// actually needs to know: a reachable instance with a rejected token looks
/// exactly like a working setup until the first scan returns nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLabCheck {
    /// Configured is false when no URL or token is set, in which case nothing
    /// was attempted.
    pub configured: bool,
    pub url: String,
    /// Reachable means the API answered and accepted the token.
    pub reachable: bool,
    /// Status is the HTTP status the API returned, zero when it never answered.
    pub status: i64,
    /// Version is the GitLab version string, when the API reported one.
    pub version: String,
    pub error: String,
    pub latency_ms: i64,
}

/// ValidateGitLabURL returns a message describing why `raw` is not usable as the
/// GitLab base URL, or the empty string when it is.
///
/// The rule is looser than the forklift host's on purpose. This is a URL forklift
/// itself calls, not a name that has to appear in a repository's build config, so
/// a self-hosted instance on an internal name is entirely legitimate. What is
/// refused is what would be unsafe or meaningless: a non-http scheme, credentials
/// embedded in the URL (they would be logged and stored where a token never
/// should be), and a query or fragment, since the base is only ever concatenated
/// with an API path.
pub fn validate_gitlab_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "URL is required".to_string();
    }
    let parsed = match url::Url::parse(trimmed) {
        Ok(u) => u,
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            return "URL must start with http:// or https://".to_string();
        }
        Err(url::ParseError::EmptyHost) => return "URL is missing a host".to_string(),
        Err(_) => return format!("'{raw}' is not a valid URL"),
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return "URL must start with http:// or https://".to_string();
    }
    if parsed.host_str().unwrap_or("").is_empty() {
        return "URL is missing a host".to_string();
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return "remove the credentials from the URL; the access token comes from the environment"
            .to_string();
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return "URL must not carry a query or fragment".to_string();
    }
    String::new()
}

/// NormalizeGitLabURL trims the trailing slash so the base can be concatenated
/// with an API path without doubling it.
pub fn normalize_gitlab_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::coverage::{host_from_url, normalize_host, validate_match_host};

    #[test]
    fn host_from_url_test() {
        for (input, want) in [
            ("https://forklift.example.com", "forklift.example.com"),
            ("https://forklift.example.com/", "forklift.example.com"),
            (
                "https://forklift.example.com/npm/npmjs/",
                "forklift.example.com",
            ),
            (
                "http://forklift.example.com:8443/",
                "forklift.example.com:8443",
            ),
            ("  https://forklift.example.com  ", "forklift.example.com"),
            ("", ""),
        ] {
            assert_eq!(host_from_url(input), want, "host_from_url({input:?})");
        }
    }

    #[test]
    fn normalize_host_test() {
        for (input, want) in [
            ("https://forklift.example.com", "forklift.example.com"),
            ("HTTP://forklift.example.com//", "forklift.example.com"),
            ("  forklift.example.com  ", "forklift.example.com"),
            ("forklift.example.com:8443", "forklift.example.com:8443"),
            // A long run of trailing slashes is stripped without a backtracking
            // pattern.
            (
                "forklift.example.com////////////////////////////////",
                "forklift.example.com",
            ),
        ] {
            assert_eq!(normalize_host(input), want, "normalize_host({input:?})");
        }
    }

    /// A forklift is reached at an external host domain, so that is what an alias
    /// has to be.
    #[test]
    fn validate_match_host_accepts_external_domains() {
        for host in [
            "forklift.example.org",
            "forklift.example.org:8443",
            "artifacts.corp.example.net",
            "10-0-0-5.example.org",
            "a.io",
        ] {
            let msg = validate_match_host(host);
            assert!(
                msg.is_empty(),
                "validate_match_host({host:?}) rejected it: {msg}"
            );
        }
    }

    #[test]
    fn validate_match_host_rejects_what_cannot_be_a_forklift_address() {
        for host in [
            "",
            "forklift.example.org/npm",   // a path, not a host
            "*.example.org",              // a wildcard cannot appear in a registry URL
            "user@forklift.example.org",  // userinfo
            "forklift.example.org:99999", // port out of range
            "forklift.example.org:abc",
            "forklift example.org",
            "-leading-hyphen.example.org",
            "forklift.example.org,other.example.org", // one host per entry
            // None of these is an address a build resolves through from outside the
            // cluster, so an alias naming one would match nothing while reading as
            // though it had been configured.
            "forklift",
            "localhost",
            "localhost:8080",
            "forklift.svc",
            "forklift.forklift.svc.cluster.local",
            "db.internal",
            "127.0.0.1",
            "10.0.0.5",
            "169.254.169.254",
        ] {
            assert!(
                !validate_match_host(host).is_empty(),
                "validate_match_host({host:?}) accepted it"
            );
        }
    }
}
