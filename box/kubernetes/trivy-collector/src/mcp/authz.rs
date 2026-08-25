//! Per-tool RBAC for the embedded MCP server.
//!
//! The HTTP layer already authenticated the caller and inserted an
//! [`AuthSession`] into the request extensions. rmcp forwards the request's
//! [`Parts`] into the MCP request context, so a tool can recover the session
//! and evaluate the same [`RbacPolicy`] the REST handlers use. Resource and
//! action names are identical to the REST mapping in `docs/rbac.md`, which
//! keeps a single policy CSV valid for both surfaces.

use axum::http::request::Parts;
use rmcp::ErrorData as McpError;
use rmcp::model::Extensions;

use crate::auth::rbac::RbacPolicy;
use crate::auth::session::AuthSession;

/// Recover the authenticated session, if any, from the MCP request context.
///
/// Absent when `auth_mode=none` (middleware not installed) or in unit tests
/// that call tools without an HTTP transport.
pub fn session_from(extensions: &Extensions) -> Option<AuthSession> {
    extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<AuthSession>().cloned())
}

/// Deny unless the caller's groups are allowed `resource:action`.
///
/// A missing session evaluates with no groups, so the policy's default role
/// decides. That matches REST behaviour for self-issued `tc_` API tokens.
pub fn require(
    extensions: &Extensions,
    rbac: &RbacPolicy,
    resource: &str,
    action: &str,
) -> Result<(), McpError> {
    let groups = session_from(extensions)
        .map(|s| s.groups)
        .unwrap_or_default();
    if rbac.is_allowed(&groups, resource, action) {
        Ok(())
    } else {
        Err(McpError::invalid_request(
            format!("RBAC denied: {resource}:{action}"),
            Some(serde_json::json!({ "resource": resource, "action": action })),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(default_policy: &str) -> RbacPolicy {
        RbacPolicy::from_csv(
            "p, role:readonly, reports, get, allow\n\
             p, role:readonly, clusters, get, allow\n\
             p, role:admin, *, *, allow\n\
             g, sec, role:readonly\n\
             g, ops, role:admin\n",
            default_policy,
        )
        .unwrap()
    }

    fn extensions_with_groups(groups: &[&str]) -> Extensions {
        let (mut parts, _) = axum::http::Request::new(()).into_parts();
        parts.extensions.insert(AuthSession {
            sub: "u".into(),
            email: None,
            name: None,
            preferred_username: None,
            groups: groups.iter().map(|g| g.to_string()).collect(),
            expires_at: i64::MAX,
        });
        let mut ext = Extensions::new();
        ext.insert(parts);
        ext
    }

    #[test]
    fn missing_parts_yields_no_session() {
        assert!(session_from(&Extensions::new()).is_none());
    }

    #[test]
    fn session_recovered_from_parts() {
        let ext = extensions_with_groups(&["sec"]);
        let s = session_from(&ext).unwrap();
        assert_eq!(s.groups, vec!["sec".to_string()]);
    }

    #[test]
    fn readonly_group_scoped() {
        let rbac = policy("");
        let ext = extensions_with_groups(&["sec"]);
        assert!(require(&ext, &rbac, "reports", "get").is_ok());
        assert!(require(&ext, &rbac, "clusters", "get").is_ok());
        let err = require(&ext, &rbac, "stats", "get").unwrap_err();
        assert!(err.message.contains("stats:get"));
    }

    #[test]
    fn admin_wildcard() {
        let rbac = policy("");
        let ext = extensions_with_groups(&["ops"]);
        assert!(require(&ext, &rbac, "admin", "delete").is_ok());
    }

    #[test]
    fn no_session_uses_default_policy() {
        let rbac = policy("role:readonly");
        assert!(require(&Extensions::new(), &rbac, "reports", "get").is_ok());
        assert!(require(&Extensions::new(), &rbac, "stats", "get").is_err());

        let strict = policy("");
        assert!(require(&Extensions::new(), &strict, "reports", "get").is_err());
    }
}
