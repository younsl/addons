//! Images the harbor-helm chart deploys. `prepare` and `harbor-log` belong to
//! the docker-compose installer only and are not built.

/// An image built from the Harbor source and the chart value that selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartImage {
    pub name: &'static str,
    pub values_key: &'static str,
}

pub const IMAGES: &[ChartImage] = &[
    ChartImage {
        name: "nginx-photon",
        values_key: "nginx.image",
    },
    ChartImage {
        name: "harbor-portal",
        values_key: "portal.image",
    },
    ChartImage {
        name: "harbor-core",
        values_key: "core.image",
    },
    ChartImage {
        name: "harbor-jobservice",
        values_key: "jobservice.image",
    },
    ChartImage {
        name: "registry-photon",
        values_key: "registry.registry.image",
    },
    ChartImage {
        name: "harbor-registryctl",
        values_key: "registry.controller.image",
    },
    ChartImage {
        name: "trivy-adapter-photon",
        values_key: "trivy.image",
    },
    ChartImage {
        name: "harbor-db",
        values_key: "database.internal.image",
    },
    ChartImage {
        name: "valkey-photon",
        values_key: "redis.internal.image",
    },
    ChartImage {
        name: "harbor-exporter",
        values_key: "exporter.image",
    },
];

/// Render harbor-helm values overriding every image `repository` and `tag`.
/// Keys sharing a parent (`registry.registry`, `registry.controller`) are
/// adjacent in [`IMAGES`], so each parent is written once.
pub fn values_override(registry: &str, tag: &str) -> String {
    let registry = registry.trim_end_matches('/');
    let mut lines = Vec::new();
    let mut previous: Vec<&str> = Vec::new();
    for image in IMAGES {
        let path: Vec<&str> = image.values_key.split('.').collect();
        let shared = previous
            .iter()
            .zip(&path)
            .take_while(|(a, b)| a == b)
            .count();
        for (depth, key) in path.iter().enumerate().skip(shared) {
            lines.push(format!("{}{key}:", "  ".repeat(depth)));
        }
        let indent = "  ".repeat(path.len());
        lines.push(format!("{indent}repository: {registry}/{}", image.name));
        lines.push(format!("{indent}tag: {tag}"));
        previous = path;
    }
    lines.push(String::new());
    lines.join("\n")
}

/// A binary the build compiles or downloads. OS packages come from the photon
/// aarch64 repository and need no check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryCheck {
    pub image: &'static str,
    pub path: &'static str,
}

pub const BINARY_CHECKS: &[BinaryCheck] = &[
    BinaryCheck {
        image: "harbor-core",
        path: "/harbor/harbor_core",
    },
    BinaryCheck {
        image: "harbor-jobservice",
        path: "/harbor/harbor_jobservice",
    },
    BinaryCheck {
        image: "harbor-registryctl",
        path: "/home/harbor/harbor_registryctl",
    },
    BinaryCheck {
        image: "harbor-registryctl",
        path: "/usr/bin/registry_DO_NOT_USE_GC",
    },
    BinaryCheck {
        image: "registry-photon",
        path: "/usr/bin/registry_DO_NOT_USE_GC",
    },
    BinaryCheck {
        image: "trivy-adapter-photon",
        path: "/usr/local/bin/trivy",
    },
    BinaryCheck {
        image: "trivy-adapter-photon",
        path: "/home/scanner/bin/scanner-trivy",
    },
    BinaryCheck {
        image: "harbor-exporter",
        path: "/harbor/harbor_exporter",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names_are_unique() {
        let mut names: Vec<_> = IMAGES.iter().map(|i| i.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), IMAGES.len());
    }

    #[test]
    fn every_binary_check_targets_a_built_image() {
        for check in BINARY_CHECKS {
            assert!(
                IMAGES.iter().any(|i| i.name == check.image),
                "{} is not built",
                check.image
            );
        }
    }

    #[test]
    fn values_override_nests_shared_parents_once() {
        let yaml = values_override("ghcr.io/younsl/harbor/", "v2.15.2");
        assert!(yaml.starts_with(
            "nginx:\n  image:\n    repository: ghcr.io/younsl/harbor/nginx-photon\n    tag: v2.15.2\n"
        ));
        assert!(yaml.contains(
            "registry:\n  registry:\n    image:\n      repository: ghcr.io/younsl/harbor/registry-photon\n      tag: v2.15.2\n  controller:\n    image:\n"
        ));
        assert_eq!(yaml.matches("\nregistry:\n").count(), 1);
        assert!(yaml.contains("redis:\n  internal:\n    image:\n      repository: ghcr.io/younsl/harbor/valkey-photon\n"));
        assert_eq!(yaml.matches("repository:").count(), IMAGES.len());
    }

    #[test]
    fn installer_only_images_are_excluded() {
        assert!(
            IMAGES
                .iter()
                .all(|i| i.name != "prepare" && i.name != "harbor-log")
        );
    }
}
