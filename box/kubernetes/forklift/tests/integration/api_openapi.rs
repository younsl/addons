//!
//! The invariant under test is unchanged -- the document and the registrations must name the
//! same operations -- and the source is the same single place the router is built.

use std::collections::BTreeSet;

/// The document served at `/openapi.yaml` and rendered by `/api-docs`. It is
/// also what the console's client is generated from, so a path that drifts out
/// of it stops being callable from the UI without any Rust code changing.
const SPEC_PATH: &str = "src/openapi/openapi.yaml";
const SPEC: &str = include_str!("../../src/openapi/openapi.yaml");
const ROUTER_SOURCE: &str = include_str!("../../src/api.rs");

/// Pins the OpenAPI document to the routes the management API actually mounts,
/// in both directions: an undocumented route is invisible to the generated
/// client, and a documented route that no longer exists generates a client
/// method that 404s at runtime.
///
/// It compares paths and methods only. Response bodies are a separate concern --
/// see the schema definitions in the document itself.
#[test]
fn spec_covers_mounted_routes() {
    let mounted = mounted_api_routes();
    let documented = documented_api_routes();

    for r in &mounted {
        assert!(
            documented.contains(r),
            "{r} is mounted but absent from {SPEC_PATH}; the generated console client cannot reach it"
        );
    }
    for r in &documented {
        assert!(
            mounted.contains(r),
            "{r} is documented in {SPEC_PATH} but not mounted; the generated client would 404"
        );
    }
}

/// Reports each mounted route as "METHOD /path", with path parameters
/// normalised so the router's `{id}` and the document's `{id}` compare equal
/// regardless of the name chosen.
fn mounted_api_routes() -> BTreeSet<String> {
    let mut routes = BTreeSet::new();
    let mut rest = ROUTER_SOURCE;
    while let Some(at) = rest.find(".route(") {
        let after = &rest[at + ".route(".len()..];
        let Some(call) = balanced(after) else { break };
        rest = &after[call.len()..];
        let Some(path) = first_string_literal(call) else {
            continue;
        };
        for method in verbs(call) {
            routes.insert(normalise_route(&method, &path));
        }
    }
    assert!(
        !routes.is_empty(),
        "no routes found; the extraction is not seeing `routes`' registrations"
    );
    routes
}

/// Returns the text up to (excluding) the paren that closes the call whose body
/// `s` starts, ignoring parens inside string literals.
fn balanced(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let (mut depth, mut in_string) = (0usize, false);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_string = !in_string,
            b'\\' if in_string => i += 1,
            b'(' if !in_string => depth += 1,
            b')' if !in_string => {
                if depth == 0 {
                    return Some(&s[..i]);
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn first_string_literal(s: &str) -> Option<String> {
    let start = s.find('"')? + 1;
    let end = start + s[start..].find('"')?;
    Some(s[start..end].to_string())
}

/// The HTTP verbs a route registration carries: the `routing::<verb>(`
/// constructor plus any `.<verb>(` chained onto it.
fn verbs(call: &str) -> Vec<String> {
    const VERBS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
    let mut out = Vec::new();
    for verb in VERBS {
        if call.contains(&format!("routing::{verb}(")) || call.contains(&format!(".{verb}(")) {
            out.push(verb.to_uppercase());
        }
    }
    out
}

/// Extracts the `/api/v1` operations from the OpenAPI document. It reads the
/// YAML structurally rather than parsing it, so the test carries no dependency
/// and stays readable next to the document.
fn documented_api_routes() -> BTreeSet<String> {
    let mut routes = BTreeSet::new();
    let mut path = String::new();
    for line in SPEC.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        // Paths are the only two-space-indented keys under "paths:".
        if line.starts_with("  /") && line.ends_with(':') {
            path = line.trim().trim_end_matches(':').to_string();
            continue;
        }
        if !path.starts_with("/api/v1") {
            continue;
        }
        for verb in ["get", "post", "put", "patch", "delete"] {
            if line == format!("    {verb}:") {
                routes.insert(normalise_route(
                    &verb.to_uppercase(),
                    path.trim_start_matches("/api/v1"),
                ));
            }
        }
    }
    assert!(
        !routes.is_empty(),
        "no /api/v1 operations found in {SPEC_PATH}; the extraction is broken, not the spec"
    );
    routes
}

fn normalise_route(method: &str, path: &str) -> String {
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = if path.is_empty() { "/" } else { path };
    // Replace every "{name}" with "{}" so the two sources compare equal
    // regardless of the parameter name chosen.
    let mut out = String::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                out.push_str("{}");
                rest = &rest[open + close + 1..];
            }
            None => {
                rest = &rest[open..];
                break;
            }
        }
    }
    out.push_str(rest);
    format!("{method} {out}")
}
