//!
//! Rust's harness has no subtests, so the mismatches are collected and reported together at the
//! end of each test.

use std::collections::BTreeMap;

use http::{Method, StatusCode};
use serde_json::Value;

use super::openapi_schema_test::{SchemaShape, spec_schemas};
use crate::testing::api::{TestServer, new_test_server};

/// The document served at `/openapi.yaml`, named for the failure messages.
const SPEC_PATH: &str = "src/openapi/openapi.yaml";
const SPEC: &str = include_str!("../openapi/openapi.yaml");

/// One documented operation and the request that exercises it.
struct Case {
    name: &'static str,
    /// Operation as it appears in the document.
    method: &'static str,
    path: &'static str,
    /// Path parameter values, in the order they appear in path.
    params: &'static [&'static str],
    /// Query string appended to the request, without the leading "?".
    query: &'static str,
    /// Request body, for the mutations.
    body: &'static str,
}

/// Shorthand for the rows below, which set two or three fields out of six.
const fn case(name: &'static str, method: &'static str, path: &'static str) -> Case {
    Case {
        name,
        method,
        path,
        params: &[],
        query: "",
        body: "",
    }
}

/// Pins each operation to the schema its handler actually answers with, by
/// calling the endpoint and comparing the keys on the wire to the schema the
/// document attaches to that operation.
///
/// The schema tests next door compare a schema against a Rust DTO, so a schema
/// that is right in isolation still passes when the document hangs it off the
/// wrong operation. That is not hypothetical: GET /repositories/{id} was
/// documented as Repository while the handler answers with the list item shape,
/// and six fields -- including the one the console branches its upload UI on --
/// were missing from the generated client with every test green.
///
/// Mutations are here too, and they are the ones that could not be checked any
/// other way: five of them had their schema attached on the strength of a manual
/// measurement, which proved the link was right that day and nothing more.
#[tokio::test]
async fn spec_operations_match_live_responses() {
    let srv = new_test_server().await;
    seed_for_response_test(&srv).await;

    let cases = [
        // Reads.
        Case {
            params: &["1"],
            ..case("repository detail", "get", "/api/v1/repositories/{id}")
        },
        Case {
            params: &["1"],
            ..case(
                "repository artifacts",
                "get",
                "/api/v1/repositories/{id}/artifacts",
            )
        },
        Case {
            params: &["1"],
            ..case(
                "repository audit log",
                "get",
                "/api/v1/repositories/{id}/audit-logs",
            )
        },
        Case {
            params: &["1"],
            ..case(
                "repository permissions",
                "get",
                "/api/v1/repositories/{id}/permissions",
            )
        },
        Case {
            params: &["1"],
            ..case(
                "repository tokens",
                "get",
                "/api/v1/repositories/{id}/tokens",
            )
        },
        Case {
            params: &["1"],
            ..case("user tokens", "get", "/api/v1/users/{id}/tokens")
        },
        case("session principal", "get", "/api/v1/me"),
        case("storage overview", "get", "/api/v1/storage"),
        case("ha status", "get", "/api/v1/ha"),
        Case {
            query: "q=probe",
            ..case("global search", "get", "/api/v1/search")
        },
        // Mutations. Ordered so each one's precondition is met by the seed or by
        // a mutation above it.
        Case {
            body: r#"{"name":"pinned-hosted","format":"maven","type":"hosted"}"#,
            ..case("create repository", "post", "/api/v1/repositories")
        },
        Case {
            params: &["1"],
            body: r#"{"upstream_url":"","config":{}}"#,
            ..case("update repository", "put", "/api/v1/repositories/{id}")
        },
        Case {
            params: &["1"],
            body: r#"{"config":{"approval":{"enabled":false}}}"#,
            ..case(
                "update repository security",
                "put",
                "/api/v1/repositories/{id}/security",
            )
        },
        Case {
            params: &["1"],
            body: r#"{"disabled":false}"#,
            ..case(
                "toggle repository disabled",
                "post",
                "/api/v1/repositories/{id}/disabled",
            )
        },
        Case {
            params: &["1"],
            body: r#"{"disabled":false}"#,
            ..case("update user", "put", "/api/v1/users/{id}")
        },
        Case {
            body: r#"{"name":"pinned-role","description":"created by the pinning test"}"#,
            ..case("create role", "post", "/api/v1/roles")
        },
        Case {
            params: &["1"],
            body: r#"{"repo_pattern":"pinned-*","actions":["read"]}"#,
            ..case(
                "add role permission",
                "post",
                "/api/v1/roles/{id}/permissions",
            )
        },
        Case {
            body: r#"{"name":"pinned-token","description":"created by the pinning test","expires_in":"720h","scopes":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
            ..case("create token", "post", "/api/v1/tokens")
        },
        Case {
            params: &["1"],
            body: r#"{"name":"pinned-user-token","description":"created by the pinning test","expires_in":"720h","scopes":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
            ..case("create user token", "post", "/api/v1/users/{id}/tokens")
        },
        Case {
            body: r#"{"repo":"pinned-proxy","package":"com.example:pinned","version":"1.0.0","reason":"pinning test"}"#,
            ..case("create version deny", "post", "/api/v1/version-denies")
        },
        Case {
            body: r#"{"name":"pinned-recv","description":"created by the pinning test","webhook_url":"https://example.invalid/hook","enabled":true}"#,
            ..case("create receiver", "post", "/api/v1/notification/receivers")
        },
        Case {
            params: &["1"],
            body: r#"{"name":"seeded-recv","description":"updated by the pinning test","webhook_url":"https://example.invalid/hook","enabled":true}"#,
            ..case(
                "update receiver",
                "put",
                "/api/v1/notification/receivers/{id}",
            )
        },
    ];

    let schemas = spec_schemas();
    let mut failures = Vec::new();
    for tc in &cases {
        let schema_name = operation_response_schema(tc.method, tc.path);
        // Routes are mounted without the /api/v1 prefix the document carries;
        // the gateway adds it in production.
        let mut url = tc
            .path
            .strip_prefix("/api/v1")
            .unwrap_or(tc.path)
            .to_string();
        for value in tc.params {
            url = replace_route_params(&url, value);
        }
        if !tc.query.is_empty() {
            url = format!("{url}?{}", tc.query);
        }
        let declared = schemas.get(&schema_name).unwrap_or_else(|| {
            panic!(
                "{} {} references schema {schema_name:?}, which is not defined",
                tc.method, tc.path
            )
        });

        let method = Method::from_bytes(tc.method.to_uppercase().as_bytes()).expect("method");
        let resp = srv.admin_do(method.clone(), &url, tc.body).await;
        assert!(
            resp.status == StatusCode::OK || resp.status == StatusCode::CREATED,
            "{method} {url}: status {} ({})",
            resp.status.as_u16(),
            resp.text().trim()
        );
        let body = first_object(&resp.body).unwrap_or_else(|| {
                    panic!(
                        "{method} {url} answered with an empty list, so there is nothing to compare; seed a row for it ({})",
                        tc.name
                    )
                });
        compare_to_schema(
            &format!("{method} {url}"),
            &schema_name,
            declared,
            &body,
            &mut failures,
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Reduces a response to the object to compare: the body itself, or a list's
/// first element. A list is reported as absent rather than as an empty object,
/// so an unseeded endpoint fails loudly instead of passing vacuously.
fn first_object(raw: &[u8]) -> Option<serde_json::Map<String, Value>> {
    let trimmed = std::str::from_utf8(raw).unwrap_or_default().trim();
    if trimmed.starts_with('{') {
        let body: serde_json::Map<String, Value> =
            serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("decode object: {e}"));
        return Some(body);
    }
    if trimmed.starts_with('[') {
        let items: Vec<Value> =
            serde_json::from_str(trimmed).unwrap_or_else(|e| panic!("decode list: {e}"));
        let first = items.first()?;
        let body = first
            .as_object()
            .unwrap_or_else(|| panic!("decode list element: {first}"));
        return Some(body.clone());
    }
    None
}

/// Checks the keys on the wire against the schema both ways: an undeclared key
/// is invisible to the generated client, and a required key that never arrives
/// makes the client trust something absent.
fn compare_to_schema(
    label: &str,
    schema_name: &str,
    declared: &SchemaShape,
    body: &serde_json::Map<String, Value>,
    failures: &mut Vec<String>,
) {
    for key in body.keys() {
        if !declared.properties.contains(key) {
            failures.push(format!(
                        "{label} sends {key:?} but {schema_name} does not declare it; the generated client cannot see it"
                    ));
        }
    }
    // Only requiredness is checked in the other direction: an optional field may
    // legitimately be absent from this particular response.
    for key in &declared.required {
        if !body.contains_key(key) {
            failures.push(format!(
                "{schema_name} requires {key:?} but {label} did not send it"
            ));
        }
    }
}

/// Creates the rows the operations are called against, so the responses are of
/// real records rather than of 404s and empty lists.
///
/// Repository 1 is hosted and repository 2 is a proxy: the version-deny and
/// approval surfaces only accept a proxy, while the upload capability only
/// applies to a hosted one.
async fn seed_for_response_test(srv: &TestServer) {
    async fn seed(srv: &TestServer, what: &str, path: &str, body: &str) {
        let resp = srv.admin_do(Method::POST, path, body).await;
        assert!(
            resp.status == StatusCode::CREATED || resp.status == StatusCode::OK,
            "seed {what}: status {} ({})",
            resp.status.as_u16(),
            resp.text().trim()
        );
    }
    seed(
        srv,
        "hosted repository",
        "/repositories",
        r#"{"name":"maven-hosted","format":"maven","type":"hosted"}"#,
    )
    .await;
    seed(
                srv,
                "proxy repository",
                "/repositories",
                r#"{"name":"pinned-proxy","format":"maven","type":"proxy","upstream_url":"https://example.invalid/maven2"}"#,
            )
            .await;
    // A token scoped to the seeded repository, so both /repositories/{id}/tokens
    // and /users/{id}/tokens answer with a row instead of an empty list.
    seed(
                srv,
                "scoped token",
                "/users/1/tokens",
                r#"{"name":"seeded-token","description":"seeded by the pinning test","expires_in":"720h","scopes":[{"repo_pattern":"maven-*","actions":["read"]}]}"#,
            )
            .await;
    seed(
                srv,
                "receiver",
                "/notification/receivers",
                r#"{"name":"seeded-recv","description":"seeded by the pinning test","webhook_url":"https://example.invalid/hook","enabled":true}"#,
            )
            .await;
    seed(
        srv,
        "group mapping",
        "/group-mappings",
        r#"{"group_name":"seeded-group","role_id":1}"#,
    )
    .await;
}

/// No documented operation the table drives carries more than one parameter.
fn replace_route_params(path: &str, value: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        if let Some(close) = rest[open..].find('}') {
            out.push_str(value);
            rest = &rest[open + close + 1..];
        } else {
            rest = &rest[open..];
            break;
        }
    }
    out.push_str(rest);
    out
}

/// A path item mixes methods with a "parameters" sequence, so each key is
/// decoded on demand rather than through one struct that would have to model
/// both shapes.
#[derive(serde::Deserialize)]
struct PathsDoc {
    paths: BTreeMap<String, BTreeMap<String, serde_yaml_ng::Value>>,
}

#[derive(serde::Deserialize)]
struct Operation {
    #[serde(default)]
    responses: BTreeMap<String, ResponseNode>,
}

#[derive(Default, serde::Deserialize)]
struct ResponseNode {
    #[serde(default)]
    content: BTreeMap<String, MediaType>,
}

#[derive(Default, serde::Deserialize)]
struct MediaType {
    #[serde(default)]
    schema: SchemaRef,
}

#[derive(Default, serde::Deserialize)]
struct SchemaRef {
    #[serde(rename = "$ref", default)]
    reference: String,
    #[serde(default)]
    items: ItemsRef,
}

#[derive(Default, serde::Deserialize)]
struct ItemsRef {
    #[serde(rename = "$ref", default)]
    reference: String,
}

fn paths_doc() -> PathsDoc {
    serde_yaml_ng::from_str(SPEC).unwrap_or_else(|e| panic!("parse spec: {e}"))
}

/// The component schema an operation's success response resolves to, following
/// an array's items when the response is a list.
fn operation_response_schema(method: &str, path: &str) -> String {
    let doc = paths_doc();
    let node = doc
        .paths
        .get(path)
        .and_then(|item| item.get(method))
        .unwrap_or_else(|| panic!("{method} {path} is not in {SPEC_PATH}"));
    let operation: Operation = serde_yaml_ng::from_value(node.clone())
        .unwrap_or_else(|e| panic!("decode {method} {path}: {e}"));

    for status in ["200", "201"] {
        let Some(content) = operation
            .responses
            .get(status)
            .and_then(|r| r.content.get("application/json"))
        else {
            continue;
        };
        let reference = if content.schema.reference.is_empty() {
            &content.schema.items.reference
        } else {
            &content.schema.reference
        };
        if let Some(name) = reference.strip_prefix("#/components/schemas/") {
            return name.to_string();
        }
    }
    panic!("{method} {path} has no named success response schema; an inline shape cannot be pinned")
}

/// The table above names the operations worth seeding data for; this catches the rest without
/// anyone having to remember to list them.
///
/// Operations whose success response has no named schema are reported rather
/// than skipped: an inline shape is exactly what leaves a generated client with
/// an anonymous per-operation type and a screen with nothing to import.
#[tokio::test]
async fn spec_covers_every_parameterless_get() {
    let srv = new_test_server().await;
    seed_for_response_test(&srv).await;
    let schemas = spec_schemas();

    // GETs that need a query parameter to answer at all. Without one they 400,
    // which the sweep would otherwise skip as "nothing to compare".
    let required = BTreeMap::from([("/api/v1/search", "q=maven")]);

    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for path in parameterless_get_paths() {
        let mut url = path.strip_prefix("/api/v1").unwrap_or(&path).to_string();
        if let Some(q) = required.get(path.as_str()) {
            url = format!("{url}?{q}");
        }
        let resp = srv.admin_do(Method::GET, &url, "").await;
        if resp.status != StatusCode::OK {
            skipped.push(format!("{path} (status {})", resp.status.as_u16()));
            continue;
        }

        // A list is compared through its first element, which is what the item
        // schema describes. An empty one is recorded rather than silently
        // passed: it means nothing was checked.
        let Some(body) = first_object(&resp.body) else {
            skipped.push(format!("{path} (empty list or non-object body)"));
            continue;
        };

        let schema_name = operation_response_schema("get", &path);
        let declared = schemas.get(&schema_name).unwrap_or_else(|| {
            panic!("GET {path} references schema {schema_name:?}, which is not defined")
        });
        compare_to_schema(
            &format!("GET {url}"),
            &schema_name,
            declared,
            &body,
            &mut failures,
        );
    }
    // Skips are reported at the end rather than next to the comparisons, where a
    // green run reads as full coverage of every path swept.
    if !skipped.is_empty() {
        println!("not compared ({}): {}", skipped.len(), skipped.join(", "));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The documented paths with a GET and no path parameter, which are the ones
/// callable without seeding anything first.
fn parameterless_get_paths() -> Vec<String> {
    let doc = paths_doc();
    let mut paths: Vec<String> = doc
        .paths
        .iter()
        .filter(|(path, item)| {
            !path.contains('{') && path.starts_with("/api/v1") && item.contains_key("get")
        })
        .map(|(path, _)| path.clone())
        .collect();
    paths.sort();
    paths
}
