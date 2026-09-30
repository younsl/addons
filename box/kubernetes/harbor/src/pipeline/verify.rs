//! Post-build checks. Image metadata alone would pass an amd64 binary copied
//! into an arm64 base, so compiled and downloaded binaries are read directly.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use tracing::info;

use super::Context;
use crate::elf;
use crate::error::{Error, Result};
use crate::images::{BINARY_CHECKS, IMAGES};
use crate::runner::{Cmd, Runner};

pub fn verify(ctx: &Context) -> Result<()> {
    info!("Verifying architecture");
    for image in IMAGES {
        let reference = ctx.cfg.upstream_ref(image.name);
        let arch = ctx.runner.output(
            &Cmd::new("docker")
                .args(["image", "inspect", "--format", "{{.Architecture}}"])
                .arg(&reference),
        )?;
        if arch != "arm64" {
            return Err(Error::ImageArch {
                image: reference,
                arch,
            });
        }
    }

    let scratch = tempfile::tempdir().map_err(|e| Error::io("create scratch directory", e))?;
    let out = scratch.path().join("bin");
    for check in BINARY_CHECKS {
        let reference = ctx.cfg.upstream_ref(check.image);
        let machine = binary_machine(ctx.runner, &reference, check.path, &out)?;
        if machine != elf::EM_AARCH64 {
            return Err(Error::BinaryArch {
                image: reference,
                path: check.path.to_string(),
                machine,
            });
        }
    }

    info!(
        images = IMAGES.len(),
        binaries = BINARY_CHECKS.len(),
        "All images and binaries are arm64"
    );
    Ok(())
}

/// Copy `path` out of a stopped container and read its ELF machine.
fn binary_machine(runner: &dyn Runner, reference: &str, path: &str, out: &Path) -> Result<u16> {
    let cid = runner.output(&Cmd::new("docker").arg("create").arg(reference))?;
    let copied = runner.run(
        &Cmd::new("docker")
            .arg("cp")
            .arg(format!("{cid}:{path}"))
            .arg(out.display().to_string()),
    );
    // Remove the container even when the copy failed.
    let removed = runner.output(&Cmd::new("docker").args(["rm", &cid]));
    copied?;
    removed?;

    let mut header = [0u8; 20];
    File::open(out)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| Error::io(format!("read {reference}:{path}"), e))?;
    elf::machine(&header, &format!("{reference}:{path}"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::pipeline::tests::config;
    use crate::runner::fake::FakeRunner;

    fn runner(image_arch: &'static str, machine: u16) -> FakeRunner {
        FakeRunner::default()
            .on(move |cmd| {
                cmd.to_string()
                    .starts_with("docker image inspect")
                    .then(|| Ok(image_arch.into()))
            })
            .on(|cmd| {
                cmd.to_string()
                    .starts_with("docker create")
                    .then(|| Ok("cid123".into()))
            })
            .on(move |cmd| {
                cmd.to_string().starts_with("docker cp").then(|| {
                    fs::write(&cmd.args[2], elf::header(machine)).unwrap();
                    Ok(String::new())
                })
            })
    }

    fn run_verify(runner: &FakeRunner) -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path(), false);
        verify(&Context { cfg: &cfg, runner })
    }

    #[test]
    fn passes_when_everything_is_arm64() {
        let r = runner("arm64", elf::EM_AARCH64);
        run_verify(&r).unwrap();
        let removed = r
            .calls()
            .iter()
            .filter(|c| *c == "docker rm cid123")
            .count();
        assert_eq!(removed, BINARY_CHECKS.len());
    }

    #[test]
    fn fails_on_amd64_image() {
        let r = runner("amd64", elf::EM_AARCH64);
        assert!(matches!(run_verify(&r), Err(Error::ImageArch { arch, .. }) if arch == "amd64"));
    }

    #[test]
    fn fails_on_amd64_binary() {
        let r = runner("arm64", elf::EM_X86_64);
        assert!(matches!(
            run_verify(&r),
            Err(Error::BinaryArch { machine, .. }) if machine == elf::EM_X86_64
        ));
    }

    #[test]
    fn removes_container_when_copy_fails() {
        let r = FakeRunner::default()
            .on(|cmd| {
                cmd.to_string()
                    .starts_with("docker image inspect")
                    .then(|| Ok("arm64".into()))
            })
            .on(|cmd| {
                cmd.to_string()
                    .starts_with("docker create")
                    .then(|| Ok("cid9".into()))
            })
            .fail_on("docker cp");
        assert!(matches!(run_verify(&r), Err(Error::CommandFailed { .. })));
        assert!(r.calls().iter().any(|c| c == "docker rm cid9"));
    }
}
