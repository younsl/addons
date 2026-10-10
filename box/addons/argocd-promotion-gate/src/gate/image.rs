//! Image reference parsing, manifest walking, and the cross-environment
//! comparison.

use std::collections::BTreeMap;

use serde_json::Value;

use super::types::{ImageComparison, ImageRef};

/// Manifest keys whose entries carry an `image` field.
const CONTAINER_KEYS: [&str; 3] = ["containers", "initContainers", "ephemeralContainers"];

/// Parses a container image reference.
///
/// The digest is split off first, then a tag separator is only honoured after
/// the last `/` so a registry port (`registry:5000/app`) is not mistaken for a
/// tag.
#[must_use]
pub fn parse_image(raw: &str) -> ImageRef {
    let raw = raw.trim();

    if let Some((repo, digest)) = raw.split_once('@') {
        return ImageRef {
            raw: raw.to_string(),
            repository: repo.to_string(),
            basename: basename_of(repo).to_string(),
            tag: String::new(),
            digest: digest.to_string(),
        };
    }

    let last_colon = raw.rfind(':');
    let last_slash = raw.rfind('/');
    if let Some(idx) = last_colon
        && last_slash.is_none_or(|slash| idx > slash)
    {
        let repo = &raw[..idx];
        return ImageRef {
            raw: raw.to_string(),
            repository: repo.to_string(),
            basename: basename_of(repo).to_string(),
            tag: raw[idx + 1..].to_string(),
            digest: String::new(),
        };
    }

    ImageRef {
        raw: raw.to_string(),
        repository: raw.to_string(),
        basename: basename_of(raw).to_string(),
        tag: String::new(),
        digest: String::new(),
    }
}

fn basename_of(repository: &str) -> &str {
    repository
        .rfind('/')
        .map_or(repository, |idx| &repository[idx + 1..])
}

/// Matches a repository basename against the ignore list. A single trailing
/// `*` globs, so one entry covers a sidecar family.
#[must_use]
pub fn is_ignored(basename: &str, ignore: &[String]) -> bool {
    ignore.iter().any(|pattern| {
        pattern.strip_suffix('*').map_or_else(
            || pattern == basename,
            |prefix| basename.starts_with(prefix),
        )
    })
}

/// Indexes images by repository basename, dropping ignored repositories. The
/// last occurrence wins, which keeps the result stable when a manifest repeats
/// a repository across containers.
#[must_use]
pub fn index_by_basename(images: &[ImageRef], ignore: &[String]) -> BTreeMap<String, ImageRef> {
    images
        .iter()
        .filter(|img| !is_ignored(&img.basename, ignore))
        .map(|img| (img.basename.clone(), img.clone()))
        .collect()
}

/// Collects every container image from a rendered Kubernetes manifest.
///
/// It walks the decoded JSON rather than switching on kind, so Deployment,
/// `StatefulSet`, `DaemonSet`, `CronJob`, and Argo Rollouts all work with no
/// per-kind handling.
#[must_use]
pub fn extract_images(manifest: &Value) -> Vec<ImageRef> {
    let mut out = Vec::new();
    walk(manifest, &mut out);
    out
}

fn walk(node: &Value, out: &mut Vec<ImageRef>) {
    match node {
        Value::Object(map) => {
            // serde_json maps iterate in key order without preserve_order, so
            // repeated runs produce the same list.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let child = &map[key];
                if CONTAINER_KEYS.contains(&key.as_str()) {
                    collect_container_images(child, out);
                }
                walk(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, out);
            }
        }
        _ => {}
    }
}

fn collect_container_images(node: &Value, out: &mut Vec<ImageRef>) {
    let Some(containers) = node.as_array() else {
        return;
    };
    for container in containers {
        let Some(image) = container.get("image").and_then(Value::as_str) else {
            continue;
        };
        if image.trim().is_empty() {
            continue;
        }
        out.push(parse_image(image));
    }
}

/// Compares desired against upstream-live images by repository basename.
///
/// Only repositories present on both sides are comparable: a sidecar injected
/// in one environment only, or an image pulled from a different registry
/// account, must not register as a mismatch.
#[must_use]
pub fn compare_images(
    desired: &[ImageRef],
    upstream_live: &[ImageRef],
    ignore: &[String],
) -> Vec<ImageComparison> {
    let desired_idx = index_by_basename(desired, ignore);
    let upstream_idx = index_by_basename(upstream_live, ignore);

    desired_idx
        .iter()
        .filter_map(|(basename, desired_img)| {
            let upstream_img = upstream_idx.get(basename)?;
            // A digest-pinned reference carries no tag, so comparing empty tags
            // would report a false match. Comparing reference() falls back to
            // the digest and only matches when both sides pin the same one.
            let desired_ref = desired_img.reference();
            let matched = !desired_ref.is_empty() && desired_ref == upstream_img.reference();
            Some(ImageComparison {
                repository: basename.clone(),
                desired_tag: desired_ref.to_string(),
                upstream_tag: upstream_img.reference().to_string(),
                matched,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parse_image_handles_tags_digests_and_ports() {
        let tagged = parse_image(" ghcr.io/org/payment-api:1.2.3 ");
        assert_eq!(tagged.repository, "ghcr.io/org/payment-api");
        assert_eq!(tagged.basename, "payment-api");
        assert_eq!(tagged.tag, "1.2.3");
        assert!(tagged.digest.is_empty());

        let pinned = parse_image("ghcr.io/org/payment-api@sha256:abc");
        assert_eq!(pinned.repository, "ghcr.io/org/payment-api");
        assert_eq!(pinned.digest, "sha256:abc");
        assert!(pinned.tag.is_empty());

        let port = parse_image("registry:5000/app");
        assert_eq!(port.repository, "registry:5000/app");
        assert_eq!(port.basename, "app");
        assert!(port.tag.is_empty());

        let bare = parse_image("nginx");
        assert_eq!(bare.repository, "nginx");
        assert_eq!(bare.basename, "nginx");
        assert_eq!(bare.reference(), "");
    }

    #[test]
    fn ignore_list_supports_exact_and_trailing_glob() {
        let ignore = strings(&["autoinstrumentation-*", "envoy"]);
        assert!(is_ignored("autoinstrumentation-java", &ignore));
        assert!(is_ignored("envoy", &ignore));
        assert!(!is_ignored("envoy-proxy", &ignore));
        assert!(!is_ignored("payment-api", &ignore));
        assert!(!is_ignored("payment-api", &[]));
    }

    #[test]
    fn index_drops_ignored_and_keeps_last_occurrence() {
        let images = vec![
            parse_image("a/app:1"),
            parse_image("b/app:2"),
            parse_image("c/sidecar:9"),
        ];
        let idx = index_by_basename(&images, &strings(&["sidecar"]));
        assert_eq!(idx.len(), 1);
        assert_eq!(idx["app"].tag, "2");
    }

    #[test]
    fn extract_images_walks_every_container_list() {
        let manifest = json!({
            "kind": "Deployment",
            "spec": {"template": {"spec": {
                "initContainers": [{"name": "init", "image": "busybox:1"}],
                "containers": [
                    {"name": "app", "image": "ghcr.io/org/app:2"},
                    {"name": "blank", "image": "  "},
                    {"name": "none"}
                ],
                "ephemeralContainers": [{"image": "debug:3"}]
            }}}
        });
        let images = extract_images(&manifest);
        let refs: Vec<&str> = images.iter().map(|i| i.raw.as_str()).collect();
        assert_eq!(refs, vec!["ghcr.io/org/app:2", "debug:3", "busybox:1"]);
        assert!(extract_images(&json!("scalar")).is_empty());
        assert!(extract_images(&json!({"containers": "not a list"})).is_empty());
    }

    #[test]
    fn compare_only_shared_basenames() {
        let desired = vec![
            parse_image("a/app:2"),
            parse_image("a/worker:2"),
            parse_image("a/only-here:1"),
        ];
        let upstream = vec![parse_image("b/app:2"), parse_image("b/worker:1")];
        let out = compare_images(&desired, &upstream, &[]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].repository, "app");
        assert!(out[0].matched);
        assert_eq!(out[1].repository, "worker");
        assert!(!out[1].matched);
        assert_eq!(out[1].desired_tag, "2");
        assert_eq!(out[1].upstream_tag, "1");
    }

    #[test]
    fn compare_digests_and_untagged() {
        let same = compare_images(
            &[parse_image("a/app@sha256:x")],
            &[parse_image("b/app@sha256:x")],
            &[],
        );
        assert!(same[0].matched);
        let untagged = compare_images(&[parse_image("a/app")], &[parse_image("b/app")], &[]);
        assert!(!untagged[0].matched, "two empty references must not match");
        let ignored = compare_images(
            &[parse_image("a/app:1")],
            &[parse_image("b/app:1")],
            &strings(&["app"]),
        );
        assert!(ignored.is_empty());
    }
}
