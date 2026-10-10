//! The console and its API answer only to loopback names, so a page on
//! another site can neither read them through DNS rebinding nor post to
//! them across origins.

use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const LOOPBACK: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

pub async fn loopback_only(request: Request, next: Next) -> Response {
    if !host_allowed(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "tether answers only on localhost, 127.0.0.1, or [::1]",
        )
            .into_response();
    }
    let reads = matches!(*request.method(), Method::GET | Method::HEAD);
    if !reads && !origin_allowed(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "cross-origin requests are not accepted",
        )
            .into_response();
    }
    next.run(request).await
}

fn hostname(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority
            .find(']')
            .map_or(authority, |end| &authority[..=end]);
    }
    authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(authority, |(host, _)| host)
}

fn host_allowed(headers: &HeaderMap) -> bool {
    headers.get(header::HOST).is_none_or(|host| {
        host.to_str()
            .is_ok_and(|host| LOOPBACK.contains(&hostname(host).to_ascii_lowercase().as_str()))
    })
}

/// A browser sends Origin on every cross-site write. Tools such as curl send
/// none, and a same-origin page sends its own host.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Ok(origin), Some(Ok(host))) = (
        origin.to_str(),
        headers.get(header::HOST).map(|h| h.to_str()),
    ) else {
        return false;
    };
    origin
        .split_once("://")
        .is_some_and(|(scheme, authority)| scheme == "http" && authority.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(host: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut map = HeaderMap::new();
        if let Some(host) = host {
            map.insert(header::HOST, HeaderValue::from_str(host).expect("host"));
        }
        if let Some(origin) = origin {
            map.insert(
                header::ORIGIN,
                HeaderValue::from_str(origin).expect("origin"),
            );
        }
        map
    }

    #[test]
    fn hostname_strips_port() {
        assert_eq!(hostname("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(hostname("[::1]:8080"), "[::1]");
        assert_eq!(hostname("localhost"), "localhost");
        assert_eq!(hostname("evil.example.com:8080"), "evil.example.com");
    }

    #[test]
    fn only_loopback_hosts() {
        assert!(host_allowed(&headers(Some("127.0.0.1:8080"), None)));
        assert!(host_allowed(&headers(Some("LOCALHOST:8080"), None)));
        assert!(host_allowed(&headers(Some("[::1]:8080"), None)));
        assert!(host_allowed(&headers(None, None)));
        assert!(!host_allowed(&headers(
            Some("rebind.example.com:8080"),
            None
        )));
        assert!(!host_allowed(&headers(Some("127.0.0.1.example.com"), None)));
    }

    #[test]
    fn writes_need_same_origin() {
        assert!(origin_allowed(&headers(Some("127.0.0.1:8080"), None)));
        assert!(origin_allowed(&headers(
            Some("127.0.0.1:8080"),
            Some("http://127.0.0.1:8080")
        )));
        assert!(!origin_allowed(&headers(
            Some("127.0.0.1:8080"),
            Some("https://evil.example.com")
        )));
        assert!(!origin_allowed(&headers(
            Some("127.0.0.1:8080"),
            Some("null")
        )));
        assert!(!origin_allowed(&headers(
            None,
            Some("http://127.0.0.1:8080")
        )));
    }
}
