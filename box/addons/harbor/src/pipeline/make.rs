//! Drives the upstream Makefile. Every container it starts runs on the host
//! architecture, so on an arm64 daemon the whole build is native.

use tracing::info;

use super::Context;
use super::source::{DEFAULT_PHOTON_BASE, photon_base_ref};
use crate::error::Result;
use crate::runner::Cmd;

/// Build tags upstream release CI compiles with.
const GOBUILDTAGS: &str = "include_oss include_gcs";

/// `goharbor/photon:*-legacy` on Docker Hub is amd64 only. Build it locally
/// from the Dockerfile upstream CI uses, so every `Dockerfile.base` resolves to
/// an arm64 image already in the local store.
pub fn photon_base(ctx: &Context) -> Result<()> {
    let workdir = &ctx.cfg.workdir;
    let base = if ctx.cfg.dry_run {
        DEFAULT_PHOTON_BASE.to_string()
    } else {
        photon_base_ref(workdir)?
    };
    info!(image = %base, "Building photon base");
    ctx.runner.run(
        &Cmd::new("docker")
            .args(["build", "--pull", "-f"])
            .arg(
                workdir
                    .join("make/photon/common/Dockerfile")
                    .display()
                    .to_string(),
            )
            .arg("-t")
            .arg(base)
            .arg(workdir.display().to_string()),
    )
}

/// `make compile` also runs `check_environment`, which demands a host Go and
/// docker-compose. These are the remaining targets.
pub fn compile(ctx: &Context) -> Result<()> {
    info!("Compiling core, jobservice and registryctl");
    ctx.runner.run(
        &make(ctx)
            .args([
                "versions_prepare",
                "compile_core",
                "compile_jobservice",
                "compile_registryctl",
            ])
            .arg(format!("VERSIONTAG={}", ctx.cfg.version))
            .arg(format!("GOBUILDTAGS={GOBUILDTAGS}")),
    )
}

pub fn build_images(ctx: &Context) -> Result<()> {
    info!("Building base and component images");
    ctx.runner.run(
        &make(ctx)
            .arg("build")
            .arg(format!("VERSIONTAG={}", ctx.cfg.version))
            .arg(format!("BASEIMAGETAG={}", ctx.cfg.version))
            .arg("BUILD_BASE=true")
            // Keep the arm64 base images built locally instead of pulling the
            // amd64 ones published upstream.
            .arg("PULL_BASE_FROM_DOCKERHUB=false")
            // Skip prepare and harbor-log, which the chart does not deploy.
            .arg("BUILD_INSTALLER=false")
            .arg("TRIVYFLAG=true"),
    )
}

fn make(ctx: &Context) -> Cmd {
    Cmd::new("make")
        .arg("-C")
        .arg(ctx.cfg.workdir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::tests::{config, fake_checkout};
    use crate::runner::fake::FakeRunner;

    #[test]
    fn photon_base_uses_the_ref_from_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        fake_checkout(tmp.path());
        let runner = FakeRunner::default();
        let cfg = config(tmp.path(), false);
        photon_base(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
        let w = tmp.path().display();
        assert_eq!(
            runner.calls(),
            vec![format!(
                "docker build --pull -f {w}/make/photon/common/Dockerfile -t goharbor/photon:5.0-legacy {w}"
            )]
        );
    }

    #[test]
    fn compile_passes_version_and_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();
        let cfg = config(tmp.path(), true);
        compile(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
        let call = &runner.calls()[0];
        assert!(call.contains("compile_core compile_jobservice compile_registryctl"));
        assert!(call.contains("VERSIONTAG=v2.15.2"));
        assert!(call.contains("'GOBUILDTAGS=include_oss include_gcs'"));
    }

    #[test]
    fn build_images_keeps_local_bases() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();
        let cfg = config(tmp.path(), true);
        build_images(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
        let call = &runner.calls()[0];
        for expected in [
            " build ",
            "BASEIMAGETAG=v2.15.2",
            "BUILD_BASE=true",
            "PULL_BASE_FROM_DOCKERHUB=false",
            "BUILD_INSTALLER=false",
            "TRIVYFLAG=true",
        ] {
            assert!(call.contains(expected), "{expected} missing from {call}");
        }
    }
}
