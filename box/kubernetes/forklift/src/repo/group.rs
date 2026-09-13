//! Fans group repository requests out to their members.

use std::sync::Arc;

use axum::extract::Request;
use axum::response::Response;
use http::request::Parts;
use http::{Method, StatusCode, Uri};

use crate::meta::{self, Store};
use crate::server::http_error;
use crate::{auth, repoconfig};

use super::Manager;
use super::router::{HandlerFn, route_params};

/// Marks a request that was authorized at the group level, so the member
/// handler's own authorization is skipped (Nexus semantics: privileges on the
/// group govern access through it, member privileges are not required).
///
#[derive(Clone, Copy)]
pub(crate) struct ViaGroup;

/// Carries the group repository's own name through fan-out so a member handler
/// can build client-facing URLs (e.g. npm tarball links) that point back at the
/// group the client is actually talking to, not the member.
#[derive(Clone)]
pub(crate) struct GroupName(pub(crate) String);

/// Returns the group name a request is being served under during group fan-out,
/// falling back to the member's own name for direct requests. Client-facing URLs
/// use this so a package installed through a group links its tarballs back to
/// the group — a single entry point and credential, matching Nexus/Artifactory
/// group semantics.
pub(crate) fn serving_repo_name(parts: &Parts, fallback: &str) -> String {
    match parts.extensions.get::<GroupName>() {
        Some(GroupName(name)) if !name.is_empty() => name.clone(),
        _ => fallback.to_string(),
    }
}

/// Reports whether the request was already authorized at group level.
pub(crate) fn via_group(parts: &Parts) -> bool {
    parts.extensions.get::<ViaGroup>().is_some()
}

/// Intercepts requests to group repositories and fans them out to the member
/// repositories in order, serving the first hit. It is format-agnostic: member
/// attempts re-enter the wrapped format handler with the repository segment of
/// the request path rewritten, so every protocol gets group support for free.
/// Non-group repositories pass through untouched.
pub(crate) async fn grouped(m: Arc<Manager>, req: Request, h: HandlerFn) -> Response {
    let (name, _) = route_params(req.uri().path());
    let repo = match m.store.get_repository_by_name(&name).await {
        Ok(repo) if repo.r#type == meta::TYPE_GROUP => repo,
        // Unknown repos fall through so the format handler emits its usual 404.
        _ => return h(m, req).await,
    };

    if req.method() != Method::GET && req.method() != Method::HEAD {
        return http_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "group repositories are read-only",
        );
    }
    let cfg = match repoconfig::parse(&repo.config_json) {
        Ok(cfg) => cfg,
        Err(_) => {
            return http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid repository config",
            );
        }
    };
    let (mut parts, body) = req.into_parts();
    if let Err(resp) = m.authorize(&parts, &name, auth::ACTION_READ, cfg.public) {
        return resp;
    }
    // The group's own ACL governs access through it; member ACLs are skipped
    // during fan-out (`via_group`), mirroring `authorize`.
    if let Err(resp) = m.ip_allowed(&parts, &name, &cfg) {
        return resp;
    }

    parts.extensions.insert(ViaGroup);
    parts.extensions.insert(GroupName(name.clone()));
    if let Some(resp) = m
        .serve_group_metadata(&parts, &repo, &cfg.group.members, h)
        .await
    {
        return resp;
    }
    drop(body);
    for member in &cfg.group.members {
        let attempt = request_with_repo(&parts, member);
        let resp = h(Arc::clone(&m), attempt).await;
        if resp.status() != StatusCode::NOT_FOUND {
            return resp;
        }
    }
    super::not_found()
}

/// Clones the request with the repository segment of the path replaced, so the
/// wrapped handler resolves the member repository instead.
///
/// The Rust router reads the repository out of the raw path (see [`route_params`]), so the same
/// substitution is done on the path itself.
fn request_with_repo(parts: &Parts, member: &str) -> Request {
    let mut req = Request::new(axum::body::Body::empty());
    *req.method_mut() = parts.method.clone();
    *req.version_mut() = parts.version;
    *req.headers_mut() = parts.headers.clone();
    *req.extensions_mut() = parts.extensions.clone();
    *req.uri_mut() = uri_with_repo(&parts.uri, member);
    req
}

/// Replaces the repository segment (the second path segment, after the format
/// prefix) of `uri` with `member`, preserving the query string.
fn uri_with_repo(uri: &Uri, member: &str) -> Uri {
    let path = uri.path();
    let mut segments: Vec<&str> = path.split('/').collect();
    // `path` starts with '/', so segments[0] is empty, [1] is the format prefix
    // and [2] is the repository name.
    if segments.len() > 2 {
        segments[2] = member;
    }
    let mut rebuilt = segments.join("/");
    if let Some(q) = uri.query() {
        rebuilt.push('?');
        rebuilt.push_str(q);
    }
    rebuilt.parse().unwrap_or_else(|_| uri.clone())
}

/// Enforces group membership invariants that need store access: at least one
/// member, every member exists, matches the group's format, is not itself a
/// group, and appears only once. Shared by the create and update API handlers.
pub async fn validate_group_members(
    store: &Arc<Store>,
    format: &str,
    members: &[String],
) -> Result<(), meta::Error> {
    if members.is_empty() {
        return Err(meta::Error::Other(
            "group repository requires at least one member".to_string(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for name in members {
        if !seen.insert(name.as_str()) {
            return Err(meta::Error::Other(format!(
                "duplicate group member: {name}"
            )));
        }
        let member = match store.get_repository_by_name(name).await {
            Ok(member) => member,
            Err(meta::Error::NotFound) => {
                return Err(meta::Error::Other(format!(
                    "group member not found: {name}"
                )));
            }
            Err(err) => return Err(err),
        };
        if member.format != format {
            return Err(meta::Error::Other(format!(
                "group member format mismatch: {name}"
            )));
        }
        if member.r#type == meta::TYPE_GROUP {
            return Err(meta::Error::Other(format!(
                "nested group repositories are not allowed: {name}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use http::{Method, StatusCode};

    use crate::meta::{self, Repository, Store};
    use crate::repoconfig;

    use crate::repo::group::validate_group_members;
    use crate::testing::repo::{call, mk_repo, mux, new_test_manager};

    /// Creates a group repository over the given members.
    pub(crate) async fn mk_group(store: &Arc<Store>, name: &str, members: &[&str]) -> Repository {
        let mut cfg = repoconfig::default();
        cfg.group.members = members.iter().map(|s| s.to_string()).collect();
        mk_repo(store, name, meta::TYPE_GROUP, "", cfg).await
    }

    #[tokio::test]
    async fn group_serves_first_hit_in_order() {
        let tm = new_test_manager().await;
        for name in ["hosted-a", "hosted-b"] {
            mk_repo(
                &tm.store,
                name,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
        }
        mk_group(&tm.store, "mvn-public", &["hosted-a", "hosted-b"]).await;
        let h = mux(&tm.manager);

        // Artifact only in the second member.
        let resp = call(
            &h,
            Method::PUT,
            "/maven/hosted-b/com/acme/lib/1.0/lib-1.0.jar",
            "FROM-B",
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "put b");

        let resp = call(
            &h,
            Method::GET,
            "/maven/mvn-public/com/acme/lib/1.0/lib-1.0.jar",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "group get");
        assert_eq!(resp.text(), "FROM-B");

        // First member shadows the second for the same path.
        let resp = call(
            &h,
            Method::PUT,
            "/maven/hosted-a/com/acme/lib/1.0/lib-1.0.jar",
            "FROM-A",
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "put a");
        let resp = call(
            &h,
            Method::GET,
            "/maven/mvn-public/com/acme/lib/1.0/lib-1.0.jar",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "shadowed get");
        assert_eq!(resp.text(), "FROM-A");

        // Miss in every member -> 404.
        let resp = call(&h, Method::GET, "/maven/mvn-public/missing.jar", "").await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "miss");
    }

    #[tokio::test]
    async fn group_is_read_only() {
        let tm = new_test_manager().await;
        mk_repo(
            &tm.store,
            "hosted-a",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        mk_group(&tm.store, "mvn-public", &["hosted-a"]).await;
        let h = mux(&tm.manager);

        let resp = call(
            &h,
            Method::PUT,
            "/maven/mvn-public/com/acme/lib/1.0/lib-1.0.jar",
            "X",
        )
        .await;
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "group put");
    }

    #[tokio::test]
    async fn group_skips_deleted_member() {
        let tm = new_test_manager().await;
        for name in ["hosted-a", "hosted-b"] {
            mk_repo(
                &tm.store,
                name,
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
        }
        mk_group(&tm.store, "mvn-public", &["hosted-a", "hosted-b"]).await;
        let h = mux(&tm.manager);

        let resp = call(
            &h,
            Method::PUT,
            "/maven/hosted-b/com/acme/lib/1.0/lib-1.0.jar",
            "FROM-B",
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "put");

        // Delete the first member; the group must skip the dangling name.
        let a = tm
            .store
            .get_repository_by_name("hosted-a")
            .await
            .expect("repository");
        tm.store
            .delete_repository(a.id)
            .await
            .expect("delete repository");

        let resp = call(
            &h,
            Method::GET,
            "/maven/mvn-public/com/acme/lib/1.0/lib-1.0.jar",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "get with dangling member");
        assert_eq!(resp.text(), "FROM-B");
    }

    #[tokio::test]
    async fn validate_group_members_cases() {
        let tm = new_test_manager().await;
        mk_repo(
            &tm.store,
            "hosted-a",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        mk_group(&tm.store, "existing-group", &["hosted-a"]).await;

        let cases: &[(&str, &str, &[&str], &str)] = &[
            ("valid", meta::FORMAT_MAVEN, &["hosted-a"], ""),
            ("empty", meta::FORMAT_MAVEN, &[], "at least one member"),
            ("missing", meta::FORMAT_MAVEN, &["nope"], "not found"),
            (
                "format mismatch",
                meta::FORMAT_NPM,
                &["hosted-a"],
                "format mismatch",
            ),
            (
                "nested group",
                meta::FORMAT_MAVEN,
                &["existing-group"],
                "nested group",
            ),
            (
                "duplicate",
                meta::FORMAT_MAVEN,
                &["hosted-a", "hosted-a"],
                "duplicate",
            ),
        ];
        for (name, format, members, want_err) in cases {
            let members: Vec<String> = members.iter().map(|s| s.to_string()).collect();
            let err = validate_group_members(&tm.store, format, &members).await;
            if want_err.is_empty() {
                assert!(err.is_ok(), "{name}: unexpected error {err:?}");
            } else {
                let message = err.expect_err(name).to_string();
                assert!(
                    message.contains(want_err),
                    "{name}: err = {message:?}, want containing {want_err:?}"
                );
            }
        }
    }

    mod authz {
        //!
        //! `TestSweeperReclaimsUnreferencedBlobs` and `TestMaybeEvictTrimsToCap` live in
        //! `sweeper_test.rs`, which already owns the engine-level sweeper/eviction
        //! cases.

        use std::sync::Arc;

        use axum::Router;
        use axum::body::Body;
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;
        use http::{Method, Request, StatusCode};

        use crate::auth::{self, Options, Service};
        use crate::meta::{self, Permission, Role, Store, User};
        use crate::repoconfig;

        use crate::repo::Manager;
        use crate::repo::group::tests::mk_group;
        use crate::testing::repo::{
            TestManager, TestResponse, call, mk_repo, mux, new_test_manager, send,
        };

        /// Wires a manager with a real `auth::Service` so the RBAC path in
        /// [`Manager::authorize`] is exercised end to end.
        async fn new_authz_manager(anonymous_read: bool) -> (TestManager, Arc<Service>) {
            crate::testing::repo::init();
            let tm = new_test_manager().await;
            let svc = Service::new(
                Arc::clone(&tm.store),
                Options {
                    session_secret: b"test-secret-test-secret-test-secret".to_vec(),
                    anonymous_read,
                    ..Default::default()
                },
            );
            svc.bootstrap_admin("admin", "adminpw")
                .await
                .expect("bootstrap admin");
            let manager = Manager::new(
                Arc::clone(&tm.engine),
                Arc::clone(&tm.store),
                Some(Arc::clone(&svc)),
                None,
                None,
            );
            (TestManager { manager, ..tm }, svc)
        }

        fn authz_mux(m: &Arc<Manager>, svc: Arc<Service>) -> Router {
            mux(m).layer(axum::middleware::from_fn_with_state(svc, auth::middleware))
        }

        /// Creates a user with one role granting `actions` on `pattern`.
        async fn grant_role(
            store: &Arc<Store>,
            username: &str,
            password: &str,
            pattern: &str,
            actions: &str,
        ) {
            let hash = auth::hash_password(password).expect("hash password");
            let u = store
                .create_user(User {
                    username: username.to_string(),
                    password_hash: hash,
                    source: meta::SOURCE_LOCAL.to_string(),
                    ..Default::default()
                })
                .await
                .expect("create user");
            let role = store
                .create_role(Role {
                    name: format!("{username}-role"),
                    ..Default::default()
                })
                .await
                .expect("create role");
            store
                .add_permission(Permission {
                    role_id: role.id,
                    repo_pattern: pattern.to_string(),
                    actions: actions.to_string(),
                    ..Default::default()
                })
                .await
                .expect("add permission");
            store.assign_role(u.id, role.id).await.expect("assign role");
        }

        /// Drives one request with optional HTTP Basic credentials.
        async fn do_authz(
            h: &Router,
            method: Method,
            path: &str,
            user: &str,
            pass: &str,
            body: &str,
        ) -> TestResponse {
            let mut builder = Request::builder().method(method).uri(path);
            if !user.is_empty() {
                let raw = BASE64.encode(format!("{user}:{pass}"));
                builder = builder.header("Authorization", format!("Basic {raw}"));
            }
            let request = builder
                .body(if body.is_empty() {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .expect("build request");
            send(h, request).await
        }

        #[tokio::test]
        async fn authorize_rbac_matrix() {
            let (tm, svc) = new_authz_manager(false).await;
            mk_repo(
                &tm.store,
                "mvn-hosted",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            grant_role(&tm.store, "reader", "readerpw", "mvn-*", "read").await;
            let h = authz_mux(&tm.manager, svc);
            let path = "/maven/mvn-hosted/com/acme/a/1.0/a-1.0.jar";

            // Anonymous is rejected with a Basic challenge when anonymous read is off.
            let resp = do_authz(&h, Method::GET, path, "", "", "").await;
            assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "anonymous get");
            assert!(
                !resp.header("WWW-Authenticate").is_empty(),
                "missing Basic challenge"
            );

            // Admin can write; the reader cannot.
            let resp = do_authz(&h, Method::PUT, path, "admin", "adminpw", "JAR").await;
            assert_eq!(resp.status, StatusCode::CREATED, "admin put");
            let resp = do_authz(&h, Method::PUT, path, "reader", "readerpw", "JAR").await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "reader put");

            // The reader can read; a repo outside their pattern is forbidden.
            let resp = do_authz(&h, Method::GET, path, "reader", "readerpw", "").await;
            assert_eq!(resp.status, StatusCode::OK, "reader get");
            mk_repo(
                &tm.store,
                "other",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let resp = do_authz(
                &h,
                Method::GET,
                "/maven/other/x.jar",
                "reader",
                "readerpw",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "reader get other");
        }

        #[tokio::test]
        async fn authorize_anonymous_read() {
            let (tm, svc) = new_authz_manager(true).await;
            mk_repo(
                &tm.store,
                "mvn-hosted",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            let h = authz_mux(&tm.manager, svc);
            let path = "/maven/mvn-hosted/com/acme/a/1.0/a-1.0.jar";

            let resp = do_authz(&h, Method::PUT, path, "admin", "adminpw", "JAR").await;
            assert_eq!(resp.status, StatusCode::CREATED, "seed put");
            // Anonymous read is allowed, anonymous write is not.
            let resp = do_authz(&h, Method::GET, path, "", "", "").await;
            assert_eq!(resp.status, StatusCode::OK, "anonymous get");
            let resp = do_authz(&h, Method::PUT, path, "", "", "JAR").await;
            assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "anonymous put");
        }

        /// A repository with `config.public` serves anonymous reads while writes still require
        /// authentication, independent of the instance-wide anonymous-read switch.
        #[tokio::test]
        async fn public_repository_anonymous_read() {
            // authz with anonymous read OFF: only the per-repo public flag opens reads.
            let (tm, svc) = new_authz_manager(false).await;
            let mut public = repoconfig::default();
            public.public = true;
            mk_repo(&tm.store, "mvn-public-flag", meta::TYPE_HOSTED, "", public).await;
            mk_repo(
                &tm.store,
                "mvn-private",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;

            for name in ["mvn-public-flag", "mvn-private"] {
                let repository = tm
                    .store
                    .get_repository_by_name(name)
                    .await
                    .expect("repository");
                tm.engine
                    .put(
                        &repository,
                        "com/x/a/1/a-1.jar",
                        "1",
                        "application/java-archive",
                        None,
                        crate::testing::repo::body("J"),
                        "",
                    )
                    .await
                    .expect("seed artifact");
            }
            let h = authz_mux(&tm.manager, svc);

            // Anonymous read on the public repo succeeds.
            let resp = call(
                &h,
                Method::GET,
                "/maven/mvn-public-flag/com/x/a/1/a-1.jar",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "anonymous public read");
            // Anonymous read on the private repo is challenged.
            let resp = call(&h, Method::GET, "/maven/mvn-private/com/x/a/1/a-1.jar", "").await;
            assert_eq!(
                resp.status,
                StatusCode::UNAUTHORIZED,
                "anonymous private read"
            );
            // Anonymous write on the public repo is still challenged.
            let resp = call(
                &h,
                Method::PUT,
                "/maven/mvn-public-flag/com/x/b/1/b-1.jar",
                "J",
            )
            .await;
            assert_eq!(
                resp.status,
                StatusCode::UNAUTHORIZED,
                "anonymous public write"
            );
        }

        #[tokio::test]
        async fn group_authorization_bypasses_members() {
            let (tm, svc) = new_authz_manager(false).await;
            mk_repo(
                &tm.store,
                "mvn-hosted",
                meta::TYPE_HOSTED,
                "",
                repoconfig::default(),
            )
            .await;
            mk_group(&tm.store, "mvn-public", &["mvn-hosted"]).await;
            // The reader may only access the group, not the member.
            grant_role(&tm.store, "reader", "readerpw", "mvn-public", "read").await;
            let h = authz_mux(&tm.manager, svc);

            let resp = do_authz(
                &h,
                Method::PUT,
                "/maven/mvn-hosted/a/b/1.0/b-1.0.jar",
                "admin",
                "adminpw",
                "JAR",
            )
            .await;
            assert_eq!(resp.status, StatusCode::CREATED, "seed put");

            // Direct member access is forbidden, but the same artifact is readable
            // through the group (member authorization is bypassed via the group grant).
            let resp = do_authz(
                &h,
                Method::GET,
                "/maven/mvn-hosted/a/b/1.0/b-1.0.jar",
                "reader",
                "readerpw",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "direct member get");
            let resp = do_authz(
                &h,
                Method::GET,
                "/maven/mvn-public/a/b/1.0/b-1.0.jar",
                "reader",
                "readerpw",
                "",
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "group get");
            // Group write is rejected before member fan-out.
            let resp = do_authz(
                &h,
                Method::PUT,
                "/maven/mvn-public/a/b/1.0/b-1.0.jar",
                "admin",
                "adminpw",
                "X",
            )
            .await;
            assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED, "group put");
        }
    }
}
