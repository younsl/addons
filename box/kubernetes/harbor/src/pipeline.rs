//! The build pipeline: fetch the Harbor source, patch its amd64 hardcodes,
//! drive the upstream Makefile natively on arm64, verify, then publish.

pub mod make;
pub mod publish;
pub mod source;
pub mod verify;

use tracing::info;

use crate::config::BuildConfig;
use crate::error::{Error, Result};
use crate::runner::{Cmd, Runner};

pub struct Context<'a> {
    pub cfg: &'a BuildConfig,
    pub runner: &'a dyn Runner,
}

pub fn run(cfg: &BuildConfig, runner: &dyn Runner) -> Result<()> {
    let ctx = Context { cfg, runner };

    if !cfg.dry_run {
        check_host(runner)?;
    }
    source::fetch(&ctx)?;
    source::patch(&ctx)?;
    make::photon_base(&ctx)?;
    make::compile(&ctx)?;
    make::build_images(&ctx)?;
    if !cfg.dry_run {
        verify::verify(&ctx)?;
    }
    publish::publish(&ctx)?;

    info!(version = %cfg.version, registry = %cfg.registry, "Harbor arm64 images ready");
    Ok(())
}

/// The upstream Makefile runs `docker build` and `docker run` without a
/// platform, so the daemon itself must be arm64. Emulation is not an option.
pub fn check_host(runner: &dyn Runner) -> Result<()> {
    let arch = runner.output(
        &Cmd::new("docker")
            .arg("info")
            .arg("--format")
            .arg("{{.Architecture}}"),
    )?;
    match arch.as_str() {
        "aarch64" | "arm64" => Ok(()),
        _ => Err(Error::HostArch(arch)),
    }
}

#[cfg(test)]
pub mod tests {
    use std::fs;

    use super::*;
    use crate::config::HarborVersion;
    use crate::elf;
    use crate::runner::fake::FakeRunner;

    pub fn config(workdir: &std::path::Path, dry_run: bool) -> BuildConfig {
        BuildConfig {
            version: HarborVersion::parse("v2.15.2").unwrap(),
            registry: "ghcr.io/younsl/harbor".to_string(),
            workdir: workdir.to_path_buf(),
            repo: "https://github.com/goharbor/harbor.git".to_string(),
            push: true,
            dry_run,
        }
    }

    /// Lay out the files the pipeline patches and parses, as `git clone` would.
    pub fn fake_checkout(workdir: &std::path::Path) {
        let photon = workdir.join("make/photon");
        fs::create_dir_all(photon.join("exporter")).unwrap();
        fs::create_dir_all(photon.join("core")).unwrap();
        fs::write(
            photon.join("exporter/Dockerfile"),
            "ENV GOOS=linux\nENV GOARCH=amd64\n",
        )
        .unwrap();
        fs::write(
            workdir.join("Makefile"),
            "TRIVY_DOWNLOAD_URL=https://example.com/trivy_$(V)_Linux-64bit.tar.gz\n",
        )
        .unwrap();
        fs::write(
            photon.join("core/Dockerfile.base"),
            "FROM goharbor/photon:5.0-legacy\n",
        )
        .unwrap();
    }

    fn arm64_runner(workdir: std::path::PathBuf) -> FakeRunner {
        FakeRunner::default()
            .on(|cmd| {
                cmd.to_string()
                    .starts_with("docker info")
                    .then(|| Ok("aarch64".into()))
            })
            .on(move |cmd| {
                cmd.to_string()
                    .starts_with("git -c advice.detachedHead=false clone")
                    .then(|| {
                        fake_checkout(&workdir);
                        Ok(String::new())
                    })
            })
            .on(|cmd| {
                cmd.to_string()
                    .starts_with("docker image inspect")
                    .then(|| Ok("arm64".into()))
            })
            .on(|cmd| {
                (cmd.program == "docker" && cmd.args.first().is_some_and(|a| a == "cp")).then(
                    || {
                        fs::write(&cmd.args[2], elf::header(elf::EM_AARCH64)).unwrap();
                        Ok(String::new())
                    },
                )
            })
    }

    #[test]
    fn full_run_builds_verifies_and_pushes() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("harbor");
        let runner = arm64_runner(workdir.clone());

        run(&config(&workdir, false), &runner).unwrap();

        let calls = runner.calls();
        let pos = |prefix: &str| {
            calls
                .iter()
                .position(|c| c.starts_with(prefix))
                .unwrap_or_else(|| panic!("{prefix} not called"))
        };
        assert!(pos("docker info") < pos("git -c advice.detachedHead=false clone"));
        assert!(pos("git -c advice.detachedHead=false clone") < pos("docker build"));
        assert!(pos("docker build") < pos(&format!("make -C {}", workdir.display())));
        assert!(pos("docker image inspect") < pos("docker tag"));
        assert!(
            calls
                .iter()
                .any(|c| c == "docker push ghcr.io/younsl/harbor/harbor-core:v2.15.2")
        );
        let exporter = fs::read_to_string(workdir.join("make/photon/exporter/Dockerfile")).unwrap();
        assert!(exporter.contains("ENV GOARCH=arm64"));
    }

    #[test]
    fn dry_run_skips_host_check_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();

        run(&config(&tmp.path().join("harbor"), true), &runner).unwrap();

        let calls = runner.calls();
        assert!(!calls.iter().any(|c| c.starts_with("docker info")));
        assert!(!calls.iter().any(|c| c.starts_with("docker image inspect")));
        assert!(calls.iter().any(|c| c.starts_with("docker push")));
    }

    #[test]
    fn host_check_rejects_amd64() {
        let runner = FakeRunner::default().on(|cmd| {
            cmd.to_string()
                .starts_with("docker info")
                .then(|| Ok("x86_64".into()))
        });
        assert!(matches!(check_host(&runner), Err(Error::HostArch(a)) if a == "x86_64"));
    }

    #[test]
    fn host_check_accepts_arm64_spellings() {
        for arch in ["aarch64", "arm64"] {
            let runner = FakeRunner::default().on(move |_| Some(Ok(arch.into())));
            check_host(&runner).unwrap();
        }
    }
}
