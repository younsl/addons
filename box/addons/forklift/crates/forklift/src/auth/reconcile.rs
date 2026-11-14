//! Applies the declarative RBAC policy (ConfigMap) and local accounts (Secret)
//! to the metadata store on startup.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;

use super::credentials::hash_password;
use super::policy::parse_policy;
use crate::meta;

/// Applies the declarative RBAC policy to the store. `policy_file` is the path
/// to an ArgoCD-style policy.csv (mounted from a ConfigMap); `accounts_dir`, if
/// set, is a directory of local-account password files (mounted from a Secret),
/// one file per account named after the username with the plaintext password as
/// its content. Reconciliation is authoritative for managed rows and idempotent.
///
/// When `policy_file` is empty, declarative RBAC is disabled and the store is
/// left untouched. A configured-but-missing policy file is treated as disabled
/// (a warning is logged) so a chart misconfiguration does not wipe managed rows.
pub async fn reconcile_rbac(
    store: Arc<meta::Store>,
    policy_file: &str,
    accounts_dir: &str,
) -> anyhow::Result<()> {
    if policy_file.is_empty() {
        return Ok(());
    }
    let body = match tokio::fs::read_to_string(policy_file).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                path = %policy_file,
                "RBAC policy file not found; skipping declarative reconciliation"
            );
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("read RBAC policy")),
    };

    let mut desired = parse_policy(&body).context("parse RBAC policy")?;

    desired.local_users = load_accounts(accounts_dir)
        .await
        .context("load local accounts")?;

    let counts = (
        desired.roles.len(),
        desired.group_roles.len(),
        desired.user_roles.len(),
        desired.local_users.len(),
    );
    store
        .apply_managed_rbac(desired)
        .await
        .context("apply RBAC policy")?;
    tracing::info!(
        roles = counts.0,
        group_mappings = counts.1,
        user_roles = counts.2,
        local_accounts = counts.3,
        "reconciled declarative RBAC"
    );
    Ok(())
}

/// Reads local-account passwords from a mounted Secret directory. Each regular
/// file's name is the username and its content is the plaintext password, which
/// is hashed before storage. Dotfiles (e.g. Kubernetes `..data` symlinks) are
/// skipped.
pub(crate) async fn load_accounts(dir: &str) -> anyhow::Result<Vec<meta::ManagedLocalUser>> {
    if dir.is_empty() {
        return Ok(Vec::new());
    }
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    let mut names: Vec<String> = Vec::new();
    while let Some(e) = entries.next_entry().await? {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || e.file_type().await?.is_dir() {
            continue;
        }
        names.push(name);
    }
    names.sort();
    for name in names {
        let raw = tokio::fs::read_to_string(Path::new(dir).join(&name)).await?;
        let password = raw.trim();
        if password.is_empty() {
            continue;
        }
        let hash = hash_password(password)?;
        out.push(meta::ManagedLocalUser {
            username: name,
            password_hash: hash,
            email: String::new(),
        });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use crate::auth::*;
    use crate::testing::auth::{new_test_service, new_test_service_with};

    #[tokio::test]
    async fn reconcile_rbac_disabled() {
        let t = new_test_service().await;
        // Empty policy file path is a no-op (declarative RBAC disabled).
        reconcile_rbac(Arc::clone(&t.store), "", "")
            .await
            .expect("disabled reconcile should succeed");
        let roles = t.store.list_roles().await.unwrap();
        assert!(roles.is_empty(), "no roles expected, got {}", roles.len());
    }

    #[tokio::test]
    async fn reconcile_rbac_missing_file() {
        let t = new_test_service().await;
        let dir = tempfile::tempdir().unwrap();
        // A configured-but-missing file is tolerated (warn + skip), not an error.
        reconcile_rbac(
            Arc::clone(&t.store),
            dir.path().join("nope.csv").to_str().unwrap(),
            "",
        )
        .await
        .expect("missing file should not error");
    }

    #[tokio::test]
    async fn reconcile_rbac_end_to_end() {
        let t = new_test_service().await;

        let dir = tempfile::tempdir().unwrap();
        let policy_path = dir.path().join("policy.csv");
        let policy = "p, readonly, repo, read, *, allow\np, dev, repo, write, team-*, allow\ng, user:ci-bot, dev\n";
        std::fs::write(&policy_path, policy).unwrap();

        let accounts_dir = dir.path().join("accounts");
        std::fs::create_dir_all(&accounts_dir).unwrap();
        std::fs::write(accounts_dir.join("ci-bot"), "s3cr3t\n").unwrap();
        // A Kubernetes-style dotfile must be ignored.
        std::fs::write(accounts_dir.join(".hidden"), "x").unwrap();

        reconcile_rbac(
            Arc::clone(&t.store),
            policy_path.to_str().unwrap(),
            accounts_dir.to_str().unwrap(),
        )
        .await
        .expect("reconcile");

        let bot = t
            .store
            .get_user_by_username("ci-bot")
            .await
            .expect("ci-bot not provisioned");
        // Local account: password hashed and stored, source local.
        assert_eq!(bot.source, "local", "ci-bot source");
        assert!(
            verify_password(&bot.password_hash, "s3cr3t"),
            "ci-bot password not set correctly"
        );
        assert!(
            t.store.get_user_by_username(".hidden").await.is_err(),
            "dotfile should not create a user"
        );
    }

    #[tokio::test]
    async fn reconcile_rbac_invalid_policy() {
        let t = new_test_service().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.csv");
        std::fs::write(&path, "p, r, repo, bogus, *, allow").unwrap();
        assert!(
            reconcile_rbac(Arc::clone(&t.store), path.to_str().unwrap(), "")
                .await
                .is_err(),
            "invalid policy should error"
        );
    }

    #[tokio::test]
    async fn default_role_grants_permissions() {
        let t = new_test_service_with(Options {
            session_secret: b"test-secret-test-secret-test-secret".to_vec(),
            default_role: "readonly".into(),
            ..Default::default()
        })
        .await;

        // Reconcile a readonly role and configure it as the default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.csv");
        std::fs::write(&path, "p, readonly, repo, read, *, allow\n").unwrap();
        reconcile_rbac(Arc::clone(&t.store), path.to_str().unwrap(), "")
            .await
            .expect("reconcile");

        // A brand-new local user with no explicit roles inherits readonly.
        let hash = hash_password("pw").unwrap();
        t.store
            .create_user(crate::meta::User {
                username: "nobody".into(),
                password_hash: hash,
                source: crate::meta::SOURCE_LOCAL.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let p = t
            .svc
            .principal_from_password("nobody", "pw")
            .await
            .expect("resolve")
            .expect("resolve");
        assert!(
            p.can("anyrepo", ACTION_READ),
            "default role should grant read"
        );
        assert!(
            !p.can("anyrepo", ACTION_WRITE),
            "default role must not grant write"
        );
    }
}
