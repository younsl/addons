//! Retag the upstream-named images under the target prefix and push them.

use tracing::info;

use super::Context;
use crate::error::Result;
use crate::images::IMAGES;
use crate::runner::Cmd;

pub fn publish(ctx: &Context) -> Result<()> {
    for image in IMAGES {
        let source = ctx.cfg.upstream_ref(image.name);
        let target = ctx.cfg.target_ref(image.name);
        ctx.runner
            .run(&Cmd::new("docker").arg("tag").arg(&source).arg(&target))?;
        if ctx.cfg.push {
            info!(image = %target, "Pushing");
            ctx.runner
                .run(&Cmd::new("docker").arg("push").arg(&target))?;
        } else {
            info!(image = %target, "Tagged, not pushed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::pipeline::tests::config;
    use crate::runner::fake::FakeRunner;

    #[test]
    fn tags_and_pushes_every_image() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();
        let cfg = config(tmp.path(), false);
        publish(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), IMAGES.len() * 2);
        assert_eq!(
            calls[0],
            "docker tag goharbor/nginx-photon:v2.15.2 ghcr.io/younsl/harbor/nginx-photon:v2.15.2"
        );
        assert_eq!(
            calls[1],
            "docker push ghcr.io/younsl/harbor/nginx-photon:v2.15.2"
        );
    }

    #[test]
    fn tags_only_without_push() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default();
        let mut cfg = config(tmp.path(), false);
        cfg.push = false;
        publish(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap();
        assert!(runner.calls().iter().all(|c| c.starts_with("docker tag")));
    }

    #[test]
    fn stops_on_push_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = FakeRunner::default().fail_on("docker push");
        let cfg = config(tmp.path(), false);
        let err = publish(&Context {
            cfg: &cfg,
            runner: &runner,
        })
        .unwrap_err();
        assert!(matches!(err, Error::CommandFailed { .. }));
        assert_eq!(runner.calls().len(), 2);
    }
}
