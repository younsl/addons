//! Harbor source checkout and the patches that make it build for arm64.

use std::fs;
use std::path::Path;

use tracing::info;

use super::Context;
use crate::error::{Error, Result};
use crate::runner::Cmd;

/// Where upstream hardcodes amd64, as (file, from, to). Everything else
/// compiles for the host architecture.
pub const PATCHES: &[(&str, &str, &str)] = &[
    // The exporter build stage pins GOARCH.
    (
        "make/photon/exporter/Dockerfile",
        "ENV GOARCH=amd64",
        "ENV GOARCH=arm64",
    ),
    // The Trivy download URL names the x86_64 release asset.
    ("Makefile", "_Linux-64bit.tar.gz", "_Linux-ARM64.tar.gz"),
];

pub const DEFAULT_PHOTON_BASE: &str = "goharbor/photon:5.0-legacy";

/// Clone a fresh checkout, replacing any previous one.
pub fn fetch(ctx: &Context) -> Result<()> {
    let workdir = &ctx.cfg.workdir;
    if workdir.exists() && !ctx.cfg.dry_run {
        info!(path = %workdir.display(), "Removing previous checkout");
        // Compile containers run as root, so a checkout from an earlier run on
        // Linux can hold root-owned files.
        fs::remove_dir_all(workdir).map_err(|e| {
            Error::io(
                format!(
                    "cannot remove {} (root-owned build output?), remove it manually",
                    workdir.display()
                ),
                e,
            )
        })?;
    }
    info!(version = %ctx.cfg.version, "Cloning Harbor source");
    ctx.runner.run(
        &Cmd::new("git")
            .args([
                "-c",
                "advice.detachedHead=false",
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--branch",
            ])
            .arg(ctx.cfg.version.as_str())
            .arg(&ctx.cfg.repo)
            .arg(workdir.display().to_string()),
    )
}

pub fn patch(ctx: &Context) -> Result<()> {
    for (file, from, to) in PATCHES {
        let path = ctx.cfg.workdir.join(file);
        if ctx.cfg.dry_run {
            info!("+ replace '{from}' with '{to}' in {}", path.display());
            continue;
        }
        replace_all(&path, from, to)?;
        info!(file, from, to, "Patched");
    }
    Ok(())
}

/// Replace every `from` with `to`, failing when `from` is absent so an upstream
/// change surfaces instead of silently producing amd64 artifacts.
pub fn replace_all(path: &Path, from: &str, to: &str) -> Result<()> {
    let content =
        fs::read_to_string(path).map_err(|e| Error::io(format!("read {}", path.display()), e))?;
    if !content.contains(from) {
        return Err(Error::PatchTarget {
            file: path.to_path_buf(),
            needle: from.to_string(),
        });
    }
    fs::write(path, content.replace(from, to))
        .map_err(|e| Error::io(format!("write {}", path.display()), e))
}

/// The photon image every `Dockerfile.base` starts from, read from the core one.
pub fn photon_base_ref(workdir: &Path) -> Result<String> {
    let path = workdir.join("make/photon/core/Dockerfile.base");
    let content =
        fs::read_to_string(&path).map_err(|e| Error::io(format!("read {}", path.display()), e))?;
    content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("FROM "))
        .filter_map(|rest| rest.split_whitespace().next())
        .find(|image| image.starts_with("goharbor/photon:"))
        .map(str::to_string)
        .ok_or(Error::BaseImageNotFound(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::tests::{config, fake_checkout};
    use crate::runner::fake::FakeRunner;

    #[test]
    fn replace_all_rewrites_every_match() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, "a amd64 b amd64\n").unwrap();
        replace_all(&path, "amd64", "arm64").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a arm64 b arm64\n");
    }

    #[test]
    fn replace_all_fails_on_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        fs::write(&path, "nothing here\n").unwrap();
        assert!(matches!(
            replace_all(&path, "amd64", "arm64"),
            Err(Error::PatchTarget { .. })
        ));
    }

    #[test]
    fn replace_all_fails_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            replace_all(&tmp.path().join("missing"), "a", "b"),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn patch_applies_every_patch() {
        let tmp = tempfile::tempdir().unwrap();
        fake_checkout(tmp.path());
        let runner = FakeRunner::default();
        let cfg = config(tmp.path(), false);
        patch(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
        let makefile = fs::read_to_string(tmp.path().join("Makefile")).unwrap();
        assert!(makefile.contains("_Linux-ARM64.tar.gz"));
        assert!(!makefile.contains("_Linux-64bit.tar.gz"));
    }

    #[test]
    fn patch_in_dry_run_touches_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();
        let cfg = config(&tmp.path().join("absent"), true);
        patch(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
    }

    #[test]
    fn fetch_replaces_previous_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("harbor");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(workdir.join("stale"), "x").unwrap();
        let runner = FakeRunner::default();
        let cfg = config(&workdir, false);

        fetch(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();

        assert!(!workdir.exists());
        assert_eq!(
            runner.calls(),
            vec![format!(
                "git -c advice.detachedHead=false clone --quiet --depth 1 --branch v2.15.2 https://github.com/goharbor/harbor.git {}",
                workdir.display()
            )]
        );
    }

    #[test]
    fn photon_base_ref_reads_core_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        fake_checkout(tmp.path());
        assert_eq!(photon_base_ref(tmp.path()).unwrap(), DEFAULT_PHOTON_BASE);
    }

    #[test]
    fn photon_base_ref_fails_without_photon() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("make/photon/core")).unwrap();
        fs::write(
            tmp.path().join("make/photon/core/Dockerfile.base"),
            "FROM alpine:3\n",
        )
        .unwrap();
        assert!(matches!(
            photon_base_ref(tmp.path()),
            Err(Error::BaseImageNotFound(_))
        ));
    }
}
