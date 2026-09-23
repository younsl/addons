//! First-run repository seeding.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use crate::meta::{self, Repository, Store};
use crate::repoconfig;

/// Describes a repository to preconfigure on first run.
pub struct DefaultRepo {
    pub name: &'static str,
    pub format: &'static str,
    pub r#type: &'static str,
    /// Proxy only.
    pub upstream: &'static str,
    /// Group only, in lookup order (hosted before proxy).
    pub members: &'static [&'static str],
    /// The English operator-facing text. The console localizes seeded
    /// descriptions client-side (`web/src/lib/seed-descriptions.ts`), so the
    /// stored value is what API consumers with no locale see.
    pub description: &'static str,
}

/// Seeded when `SeedDefaultRepos` is enabled, mirroring the repositories a Nexus
/// install ships with: one proxy of each public registry, one local (hosted)
/// repository per format for internal artifacts, and one group per format
/// combining both behind a single client URL (the Nexus `maven-public`
/// pattern). Groups are listed last so their members exist when they are
/// created.
pub static DEFAULT_REPOSITORIES: LazyLock<Vec<DefaultRepo>> = LazyLock::new(|| {
    vec![
        // Proxies of public upstreams.
        DefaultRepo {
            name: "maven-central",
            format: meta::FORMAT_MAVEN,
            r#type: meta::TYPE_PROXY,
            upstream: "https://repo1.maven.org/maven2",
            members: &[],
            description: "Caching proxy of Maven Central. Serves Maven and Gradle dependencies and keeps a local copy of every downloaded artifact.",
        },
        DefaultRepo {
            name: "npmjs",
            format: meta::FORMAT_NPM,
            r#type: meta::TYPE_PROXY,
            upstream: "https://registry.npmjs.org",
            members: &[],
            description: "Caching proxy of the public npm registry. Package metadata and tarballs are cached locally after the first download.",
        },
        DefaultRepo {
            name: "crates-io",
            format: meta::FORMAT_CARGO,
            r#type: meta::TYPE_PROXY,
            upstream: "https://index.crates.io",
            members: &[],
            description: "Caching proxy of the crates.io sparse index. Rust crates are cached locally so repeated builds avoid the upstream.",
        },
        DefaultRepo {
            name: "goproxy",
            format: meta::FORMAT_GO,
            r#type: meta::TYPE_PROXY,
            upstream: "https://proxy.golang.org",
            members: &[],
            description: "Caching proxy of proxy.golang.org. Go modules and checksums are cached locally after the first fetch.",
        },
        DefaultRepo {
            name: "pypi",
            format: meta::FORMAT_PYPI,
            r#type: meta::TYPE_PROXY,
            upstream: "https://pypi.org/simple",
            members: &[],
            description: "Caching proxy of PyPI. Python packages are cached locally so repeated installs avoid the upstream.",
        },
        DefaultRepo {
            name: "docker.io-proxy",
            format: meta::FORMAT_OCI,
            r#type: meta::TYPE_PROXY,
            upstream: "https://registry-1.docker.io",
            members: &[],
            description: "Caching proxy of Docker Hub. Container images pulled through this registry are cached layer by layer.",
        },
        DefaultRepo {
            name: "ghcr.io-proxy",
            format: meta::FORMAT_OCI,
            r#type: meta::TYPE_PROXY,
            upstream: "https://ghcr.io",
            members: &[],
            description: "Caching proxy of the GitHub Container Registry. Container images and Helm charts pulled through this registry are cached layer by layer.",
        },
        // Hosted repositories for internal artifacts.
        DefaultRepo {
            name: "maven-hosted",
            format: meta::FORMAT_MAVEN,
            r#type: meta::TYPE_HOSTED,
            upstream: "",
            members: &[],
            description: "Hosted repository for internal Maven and Gradle artifacts. Publish with mvn deploy or the console upload.",
        },
        DefaultRepo {
            name: "npm-hosted",
            format: meta::FORMAT_NPM,
            r#type: meta::TYPE_HOSTED,
            upstream: "",
            members: &[],
            description: "Hosted repository for internal npm packages. Publish with npm publish or the console upload.",
        },
        DefaultRepo {
            name: "cargo-hosted",
            format: meta::FORMAT_CARGO,
            r#type: meta::TYPE_HOSTED,
            upstream: "",
            members: &[],
            description: "Hosted repository for internal Rust crates. Publish with cargo publish or the console upload.",
        },
        DefaultRepo {
            name: "go-hosted",
            format: meta::FORMAT_GO,
            r#type: meta::TYPE_HOSTED,
            upstream: "",
            members: &[],
            description: "Hosted repository for internal Go modules. Publish through the console upload.",
        },
        DefaultRepo {
            name: "pypi-hosted",
            format: meta::FORMAT_PYPI,
            r#type: meta::TYPE_HOSTED,
            upstream: "",
            members: &[],
            description: "Hosted repository for internal Python packages. Publish with twine or the console upload.",
        },
        DefaultRepo {
            name: "oci-hosted",
            format: meta::FORMAT_OCI,
            r#type: meta::TYPE_HOSTED,
            upstream: "",
            members: &[],
            description: "Hosted OCI registry for internal container images and Helm charts. Push with standard clients such as docker and helm and oras.",
        },
        // Groups: hosted first so internal artifacts shadow public ones.
        DefaultRepo {
            name: "maven-public",
            format: meta::FORMAT_MAVEN,
            r#type: meta::TYPE_GROUP,
            upstream: "",
            members: &["maven-hosted", "maven-central"],
            description: "Group repository combining maven-hosted and maven-central behind one URL. Internal artifacts are looked up before the public proxy.",
        },
        DefaultRepo {
            name: "npm-public",
            format: meta::FORMAT_NPM,
            r#type: meta::TYPE_GROUP,
            upstream: "",
            members: &["npm-hosted", "npmjs"],
            description: "Group repository combining npm-hosted and npmjs behind one URL. Internal packages are looked up before the public proxy.",
        },
        DefaultRepo {
            name: "cargo-public",
            format: meta::FORMAT_CARGO,
            r#type: meta::TYPE_GROUP,
            upstream: "",
            members: &["cargo-hosted", "crates-io"],
            description: "Group repository combining cargo-hosted and crates-io behind one URL. Internal crates are looked up before the public proxy.",
        },
        DefaultRepo {
            name: "go-public",
            format: meta::FORMAT_GO,
            r#type: meta::TYPE_GROUP,
            upstream: "",
            members: &["go-hosted", "goproxy"],
            description: "Group repository combining go-hosted and goproxy behind one URL. Internal modules are looked up before the public proxy.",
        },
        DefaultRepo {
            name: "pypi-public",
            format: meta::FORMAT_PYPI,
            r#type: meta::TYPE_GROUP,
            upstream: "",
            members: &["pypi-hosted", "pypi"],
            description: "Group repository combining pypi-hosted and pypi behind one URL. Internal packages are looked up before the public proxy.",
        },
        DefaultRepo {
            name: "oci-public",
            format: meta::FORMAT_OCI,
            r#type: meta::TYPE_GROUP,
            upstream: "",
            members: &["oci-hosted", "docker.io-proxy", "ghcr.io-proxy"],
            description: "Group registry combining oci-hosted with the docker.io and ghcr.io proxies behind one image prefix. Internal images are looked up before the public registries.",
        },
    ]
});

/// The set of names in [`DEFAULT_REPOSITORIES`], for O(1) lookup.
static DEFAULT_REPO_NAMES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| DEFAULT_REPOSITORIES.iter().map(|r| r.name).collect());

/// Maps earlier seeded names to their current ones (the OCI proxies moved to
/// the `{host}-proxy` convention). [`seed_defaults`] renames a row still
/// carrying the old name so an install seeded before the change follows the
/// definition instead of accumulating a duplicate.
const LEGACY_SEED_RENAMES: &[(&str, &str)] =
    &[("docker-hub", "docker.io-proxy"), ("ghcr", "ghcr.io-proxy")];

/// Reports whether `name` is one of the predefined seed repositories. These are
/// protected from deletion (even by admins): a group repository depends on its
/// members existing, and a deleted default would silently reappear on the next
/// startup when seeding is enabled, so deletion is at best confusing and at
/// worst breaks a group. Repositories cannot be renamed, so the name is a stable
/// identity for this check.
pub fn is_default_repo(name: &str) -> bool {
    DEFAULT_REPO_NAMES.contains(name)
}

/// Renames a legacy seeded repository to its current name and rewrites seeded
/// group member lists that referenced the old name. A no-op when the old name is
/// absent or the new name already exists (the operator created it themselves).
pub(crate) async fn rename_seeded_repo(
    store: &Arc<Store>,
    old_name: &str,
    new_name: &str,
) -> Result<(), meta::Error> {
    let old = match store.get_repository_by_name(old_name).await {
        Ok(r) => r,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e) => return Err(e),
    };
    match store.get_repository_by_name(new_name).await {
        Ok(_) => return Ok(()),
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(e),
    }
    store.rename_repository(old.id, new_name).await?;
    tracing::info!(
        from = old_name,
        to = new_name,
        "renamed legacy seeded repository"
    );
    // Fix seeded groups whose member list still names the old repository.
    for repo in store.list_repositories().await? {
        if repo.r#type != meta::TYPE_GROUP || !is_default_repo(&repo.name) {
            continue;
        }
        let Ok(mut cfg) = repoconfig::parse(&repo.config_json) else {
            continue;
        };
        let mut changed = false;
        for member in cfg.group.members.iter_mut() {
            if member == old_name {
                *member = new_name.to_string();
                changed = true;
            }
        }
        if !changed {
            continue;
        }
        let cfg_json = cfg.json().map_err(|e| meta::Error::Other(e.to_string()))?;
        store
            .update_repository_config(repo.id, &repo.upstream_url, &cfg_json)
            .await?;
    }
    Ok(())
}

/// Creates any missing default repositories. It is idempotent: existing names
/// are skipped, and a create lost to a concurrent replica (UNIQUE conflict) is
/// ignored.
pub async fn seed_defaults(store: &Arc<Store>) -> Result<(), meta::Error> {
    for (old_name, new_name) in LEGACY_SEED_RENAMES {
        rename_seeded_repo(store, old_name, new_name).await?;
    }
    for r in DEFAULT_REPOSITORIES.iter() {
        match store.get_repository_by_name(r.name).await {
            Ok(existing) => {
                // Seeded repositories are definition-owned: their description is
                // part of the seed definition and is kept in sync on every
                // startup (like their delete protection). Operator edits to
                // seeded descriptions are therefore reset here; custom
                // repositories are never touched.
                if existing.description != r.description {
                    store
                        .update_repository_description(existing.id, r.description)
                        .await?;
                }
                // When the seed definition gains a group member (e.g. the ghcr
                // proxy joining oci-public), installs seeded before the addition
                // are extended too — but only when the stored list is a strict
                // prefix of the new default, so an operator's reordering or
                // removals are never overwritten.
                if r.r#type == meta::TYPE_GROUP
                    && let Ok(mut cfg) = repoconfig::parse(&existing.config_json)
                    && cfg.group.members.len() < r.members.len()
                    && cfg.group.members
                        == r.members[..cfg.group.members.len()]
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                {
                    cfg.group.members = r.members.iter().map(|s| s.to_string()).collect();
                    if let Ok(cfg_json) = cfg.json() {
                        store
                            .update_repository_config(
                                existing.id,
                                &existing.upstream_url,
                                &cfg_json,
                            )
                            .await?;
                        tracing::info!(
                            name = r.name, members = ?r.members,
                            "extended seeded group members"
                        );
                    }
                }
                continue;
            }
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
        let mut cfg = repoconfig::Config::default();
        cfg.group.members = r.members.iter().map(|s| s.to_string()).collect();
        let cfg_json = cfg.json().map_err(|e| meta::Error::Other(e.to_string()))?;
        match store
            .create_repository(Repository {
                name: r.name.to_string(),
                format: r.format.to_string(),
                r#type: r.r#type.to_string(),
                upstream_url: r.upstream.to_string(),
                config_json: cfg_json,
                description: r.description.to_string(),
                ..Default::default()
            })
            .await
        {
            Ok(_) => {}
            // A UNIQUE violation means a concurrent replica won the race.
            Err(meta::Error::Conflict) => continue,
            Err(e) => return Err(e),
        }
        tracing::info!(
            name = r.name,
            format = r.format,
            r#type = r.r#type,
            "seeded default repository"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use crate::meta::{self, Store};
    use crate::repoconfig;

    use crate::repo::group::validate_group_members;
    use crate::repo::{DEFAULT_REPOSITORIES, seed_defaults};

    #[tokio::test]
    async fn seed_defaults_is_idempotent_and_complete() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(
            Store::open(dir.path().join("seed.db"))
                .await
                .expect("open store"),
        );

        seed_defaults(&store).await.expect("seed");
        let repos = store.list_repositories().await.expect("list");
        assert_eq!(repos.len(), DEFAULT_REPOSITORIES.len(), "seeded count");
        // Idempotent: a second run creates nothing new.
        seed_defaults(&store).await.expect("reseed");
        assert_eq!(
            store.list_repositories().await.expect("list").len(),
            DEFAULT_REPOSITORIES.len(),
            "count after reseed"
        );

        // Expect proxy, local and group defaults, with proxies carrying an upstream
        // and every group member valid.
        let (mut proxies, mut locals, mut groups) = (0, 0, 0);
        for r in &repos {
            match r.r#type.as_str() {
                meta::TYPE_PROXY => {
                    proxies += 1;
                    assert!(
                        !r.upstream_url.is_empty(),
                        "proxy {} missing upstream",
                        r.name
                    );
                }
                meta::TYPE_HOSTED => locals += 1,
                meta::TYPE_GROUP => {
                    groups += 1;
                    let cfg = repoconfig::parse(&r.config_json)
                        .unwrap_or_else(|e| panic!("group {} config: {e}", r.name));
                    validate_group_members(&store, &r.format, &cfg.group.members)
                        .await
                        .unwrap_or_else(|e| panic!("group {} members invalid: {e}", r.name));
                }
                _ => {}
            }
        }
        assert!(
            proxies > 0 && locals > 0 && groups > 0,
            "expected proxy, local and group defaults, got proxies={proxies} locals={locals} groups={groups}"
        );
    }

    mod seed_rename {
        use std::sync::Arc;

        use crate::meta::{self, Repository, Store};
        use crate::repoconfig::{self, Config};

        use crate::repo::seed::rename_seeded_repo;
        use crate::repo::{DEFAULT_REPOSITORIES, is_default_repo};
        use crate::testing::repo::new_test_manager;

        async fn members(store: &Arc<Store>, id: i64) -> Vec<String> {
            let repo = store.get_repository(id).await.expect("get repository");
            repoconfig::parse(&repo.config_json)
                .expect("parse config")
                .group
                .members
        }

        /// An install seeded before the OCI proxies were renamed must follow the current
        /// definition rather than accumulate a duplicate, and the rename has to carry
        /// the seeded groups with it: a group whose member list still names the old
        /// repository would resolve nothing after the rename.
        #[tokio::test]
        async fn rename_seeded_repo_rewrites_group_members() {
            let tm = new_test_manager().await;

            let legacy = tm
                .store
                .create_repository(Repository {
                    name: "docker-hub".into(),
                    format: meta::FORMAT_OCI.into(),
                    r#type: meta::TYPE_PROXY.into(),
                    upstream_url: "https://registry-1.docker.io".into(),
                    ..Default::default()
                })
                .await
                .expect("create legacy repo");
            let mut group_config = Config::default();
            group_config.group.members = vec!["oci-hosted".into(), "docker-hub".into()];
            let group_json = group_config.json().expect("config json");
            let group = tm
                .store
                .create_repository(Repository {
                    name: "oci-public".into(),
                    format: meta::FORMAT_OCI.into(),
                    r#type: meta::TYPE_GROUP.into(),
                    config_json: group_json.clone(),
                    ..Default::default()
                })
                .await
                .expect("create group");
            // A group the operator created themselves must not be rewritten: only
            // seeded definitions are owned by the seed.
            let custom = tm
                .store
                .create_repository(Repository {
                    name: "oci-team".into(),
                    format: meta::FORMAT_OCI.into(),
                    r#type: meta::TYPE_GROUP.into(),
                    config_json: group_json,
                    ..Default::default()
                })
                .await
                .expect("create custom group");

            rename_seeded_repo(&tm.store, "docker-hub", "docker.io-proxy")
                .await
                .expect("rename");

            let renamed = tm
                .store
                .get_repository(legacy.id)
                .await
                .expect("get renamed");
            assert_eq!(renamed.name, "docker.io-proxy", "repository name");
            let got = members(&tm.store, group.id).await;
            assert_eq!(got.len(), 2);
            assert_eq!(got[1], "docker.io-proxy", "seeded group members");
            assert_eq!(
                members(&tm.store, custom.id).await[1],
                "docker-hub",
                "operator-created group was rewritten"
            );
        }

        /// The rename is a no-op in the two states where it would do damage: nothing to
        /// rename, and a repository already sitting on the target name (which the
        /// operator may have created themselves, and which renaming onto would collide
        /// with).
        #[tokio::test]
        async fn rename_seeded_repo_no_ops() {
            let tm = new_test_manager().await;

            rename_seeded_repo(&tm.store, "docker-hub", "docker.io-proxy")
                .await
                .expect("absent legacy repository");

            for name in ["docker-hub", "docker.io-proxy"] {
                tm.store
                    .create_repository(Repository {
                        name: name.into(),
                        format: meta::FORMAT_OCI.into(),
                        r#type: meta::TYPE_PROXY.into(),
                        ..Default::default()
                    })
                    .await
                    .expect("create repo");
            }
            rename_seeded_repo(&tm.store, "docker-hub", "docker.io-proxy")
                .await
                .expect("target already present");
            // Both survive: the legacy row is left alone rather than renamed onto a name
            // that is taken.
            tm.store
                .get_repository_by_name("docker-hub")
                .await
                .expect("legacy repository was touched");
        }

        /// Seeded repositories are protected from deletion, so the membership test has
        /// to be exact: a name that merely looks seeded must not inherit the protection,
        /// and every seeded name must have it.
        #[test]
        fn is_default_repo_membership() {
            for definition in DEFAULT_REPOSITORIES.iter() {
                assert!(
                    is_default_repo(definition.name),
                    "seeded repository {:?} is not protected",
                    definition.name
                );
            }
            for name in [
                "",
                "maven-hosted-2",
                "docker-hub",
                "team-maven",
                "MAVEN-HOSTED",
            ] {
                assert!(
                    !is_default_repo(name),
                    "{name:?} is treated as a seeded repository"
                );
            }
        }
    }
}
