//! `forklift migrate-storage`: the source is the regular `FORKLIFT_STORAGE_S3_*`
//! configuration, the target `FORKLIFT_MIGRATE_TO_S3_*` or, with
//! `--interactive`, answers typed at a prompt.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config;
use crate::objstore::{self, S3Api};
use crate::storage;

use super::lease::{LeaseHold, in_cluster_client, wait_for_no_writers};
use super::preflight::render;
use super::{Check, Endpoint, Guard, Options, Report, has_meta, migrate};

pub const USAGE: &str = "Usage of forklift migrate-storage:
  -interactive, -i
        prompt for the target and confirm the plan before copying
  -dry-run
        run every preflight check and report the plan without copying
  -overwrite-meta
        replace a metadata snapshot already present in the target
  -concurrency int
        blobs copied in parallel (default 8)
  -require-conditional-writes
        fail when the target ignores S3 conditional writes (the target runs HA)
  -allow-missing-source-blobs
        carry over metadata whose blobs are already missing in the source
  -lease-name string
        hold this HA Lease while copying (in-cluster only)
  -writer-selector string
        label selector of forklift pods that must be gone before copying
  -wait duration
        how long to wait for writers to stop and the Lease to free (default 3m)
  -staging string
        directory for staged blobs and the snapshot (default the system temp dir)
  -verify string
        postflight blob re-hash: off, sample or full (default sample)
  -verify-sample-percent int
        share of named blobs re-hashed in sample mode, at least 20 (default 5)

The source is FORKLIFT_STORAGE_S3_*. The target is FORKLIFT_MIGRATE_TO_S3_BUCKET,
_PREFIX, _REGION, _ENDPOINT, _FORCE_PATH_STYLE, _ACCESS_KEY_ID, _SECRET_ACCESS_KEY,
and for the capacity check _PROVIDER, _ADMIN_ENDPOINT and _ADMIN_TOKEN.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub interactive: bool,
    pub dry_run: bool,
    pub overwrite_meta: bool,
    pub concurrency: usize,
    pub require_conditional_writes: bool,
    pub allow_missing_source_blobs: bool,
    pub lease_name: String,
    pub writer_selector: String,
    pub wait: Duration,
    pub staging: PathBuf,
    pub verify: super::Verify,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            interactive: false,
            dry_run: false,
            overwrite_meta: false,
            concurrency: 8,
            require_conditional_writes: false,
            allow_missing_source_blobs: false,
            lease_name: String::new(),
            writer_selector: String::new(),
            wait: Duration::from_secs(180),
            staging: std::env::temp_dir(),
            verify: super::Verify::Sample { percent: 5 },
        }
    }
}

pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args::default();
    let mut verify_mode = "sample".to_string();
    let mut verify_percent: u8 = 5;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = arg.trim_start_matches('-');
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (flag, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or(format!("flag needs an argument: -{name}"))
        };
        match name {
            "interactive" | "i" => out.interactive = true,
            "dry-run" => out.dry_run = true,
            "overwrite-meta" => out.overwrite_meta = true,
            "require-conditional-writes" => out.require_conditional_writes = true,
            "allow-missing-source-blobs" => out.allow_missing_source_blobs = true,
            "concurrency" => {
                let v = value()?;
                out.concurrency = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=256).contains(n))
                    .ok_or(format!("invalid value {v:?} for flag -concurrency (1-256)"))?;
            }
            "lease-name" => out.lease_name = value()?,
            "writer-selector" => out.writer_selector = value()?,
            "wait" => {
                let v = value()?;
                out.wait = humantime::parse_duration(&v)
                    .map_err(|e| format!("invalid value {v:?} for flag -wait: {e}"))?;
            }
            "staging" => out.staging = PathBuf::from(value()?),
            "verify" => verify_mode = value()?,
            "verify-sample-percent" => {
                let v = value()?;
                verify_percent = v
                    .parse::<u8>()
                    .ok()
                    .filter(|n| (1..=100).contains(n))
                    .ok_or(format!(
                        "invalid value {v:?} for flag -verify-sample-percent (1-100)"
                    ))?;
            }
            "help" | "h" => return Err(String::new()),
            _ => return Err(format!("flag provided but not defined: {arg}")),
        }
    }
    out.verify = super::Verify::parse(&verify_mode, verify_percent).ok_or(format!(
        "invalid value {verify_mode:?} for flag -verify (off, sample, full)"
    ))?;
    if out.lease_name.is_empty() != out.writer_selector.is_empty() {
        return Err("-lease-name and -writer-selector are used together".into());
    }
    Ok(out)
}

pub fn target_from_env() -> config::S3Config {
    let env = |k: &str| std::env::var(format!("FORKLIFT_MIGRATE_TO_S3_{k}")).unwrap_or_default();
    config::S3Config {
        bucket: env("BUCKET"),
        prefix: env("PREFIX"),
        region: env("REGION"),
        endpoint: env("ENDPOINT"),
        force_path_style: matches!(env("FORCE_PATH_STYLE").as_str(), "true" | "1"),
        access_key_id: env("ACCESS_KEY_ID"),
        secret_access_key: env("SECRET_ACCESS_KEY"),
        provider: env("PROVIDER").to_ascii_lowercase(),
        admin_endpoint: env("ADMIN_ENDPOINT"),
        admin_token: env("ADMIN_TOKEN"),
    }
}

/// Terminal I/O behind the interactive flow, so it can be driven by a test.
pub trait Console {
    fn line(&mut self, prompt: &str) -> io::Result<String>;
    fn secret(&mut self, prompt: &str) -> io::Result<String>;
    fn say(&mut self, text: &str) -> io::Result<()>;
}

pub struct Terminal<R, W> {
    pub input: R,
    pub output: W,
}

impl<R: BufRead, W: Write> Console for Terminal<R, W> {
    fn line(&mut self, prompt: &str) -> io::Result<String> {
        write!(self.output, "{prompt}")?;
        self.output.flush()?;
        let mut buf = String::new();
        if self.input.read_line(&mut buf)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "input closed"));
        }
        Ok(buf.trim().to_string())
    }

    fn secret(&mut self, prompt: &str) -> io::Result<String> {
        let echo = EchoGuard::off();
        let value = self.line(prompt);
        drop(echo);
        writeln!(self.output)?;
        value
    }

    fn say(&mut self, text: &str) -> io::Result<()> {
        writeln!(self.output, "{text}")
    }
}

/// Restores terminal echo on drop. A no-op when stdin is not a terminal.
struct EchoGuard(Option<libc::termios>);

impl EchoGuard {
    fn off() -> EchoGuard {
        // SAFETY: tcgetattr/tcsetattr only read and write the termios struct
        // for stdin, which outlives the guard.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return EchoGuard(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return EchoGuard(None);
            }
            let saved = t;
            t.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
            EchoGuard(Some(saved))
        }
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: see `off`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
            }
        }
    }
}

fn ask(c: &mut dyn Console, label: &str, default: &str) -> io::Result<String> {
    let v = c.line(&format!("{label} [{default}]: "))?;
    Ok(if v.is_empty() { default.to_string() } else { v })
}

fn ask_yes(c: &mut dyn Console, label: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let v = c.line(&format!("{label} [{hint}]: "))?;
    Ok(match v.to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

/// Fills the target from prompts, offering `defaults` (the environment).
pub fn prompt_target(
    c: &mut dyn Console,
    defaults: &config::S3Config,
) -> io::Result<config::S3Config> {
    let mut t = defaults.clone();
    t.endpoint = ask(c, "Target S3 endpoint (empty for AWS)", &defaults.endpoint)?;
    loop {
        t.bucket = ask(c, "Target bucket", &defaults.bucket)?;
        if !t.bucket.is_empty() {
            break;
        }
        c.say("A bucket is required.")?;
    }
    t.prefix = ask(c, "Target key prefix", &defaults.prefix)?;
    t.region = ask(c, "Target region", &defaults.region)?;
    t.force_path_style = ask_yes(
        c,
        "Path-style addressing",
        defaults.force_path_style || !t.endpoint.is_empty(),
    )?;
    t.access_key_id = ask(
        c,
        "Target access key id (empty for the default chain)",
        &defaults.access_key_id,
    )?;
    if !t.access_key_id.is_empty() {
        let hint = if defaults.secret_access_key.is_empty() {
            ""
        } else {
            " (empty keeps the environment value)"
        };
        let s = c.secret(&format!("Target secret access key{hint}: "))?;
        if !s.is_empty() {
            t.secret_access_key = s;
        }
    }
    Ok(t)
}

pub fn plan_summary(r: &Report) -> String {
    format!(
        "{} blobs named by the metadata: {} to copy ({}), {} already in the target. The metadata snapshot is copied last.",
        r.required,
        r.copied,
        super::preflight::bytes(r.bytes_copied.max(0) as u64),
        r.skipped
    )
}

fn options(args: &Args, dry_run: bool, overwrite_meta: bool) -> Options {
    Options {
        concurrency: args.concurrency,
        dry_run,
        overwrite_meta,
        require_conditional_writes: args.require_conditional_writes,
        allow_missing_source_blobs: args.allow_missing_source_blobs,
        verify: args.verify,
        staging: args.staging.clone(),
    }
}

/// The writer guard: with a Lease and selector, forklift pods must be gone
/// and the Lease held; without them the operator vouches for it.
async fn guard(args: &Args) -> (Guard, Option<LeaseHold>) {
    if args.lease_name.is_empty() {
        return (Guard::unverified(), None);
    }
    let namespace = std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "default".into());
    let fail = |name, detail: String| {
        (
            Guard {
                checks: vec![Check::fail(name, detail)],
                held: Box::new(|| false),
            },
            None,
        )
    };
    let client = match in_cluster_client() {
        Ok(c) => c,
        Err(e) => return fail("writers-stopped", e),
    };
    let waited = std::time::Instant::now();
    match wait_for_no_writers(&client, &namespace, &args.writer_selector, args.wait).await {
        Ok(left) if left.is_empty() => {}
        Ok(left) => {
            return fail(
                "writers-stopped",
                format!(
                    "forklift pods still running after {}s: {}",
                    args.wait.as_secs(),
                    left.join(", ")
                ),
            );
        }
        Err(e) => return fail("writers-stopped", e),
    }
    let identity = format!(
        "forklift-migrate-{}",
        std::env::var("POD_NAME").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
    );
    let ha = config::HAConfig {
        enabled: true,
        lease_name: args.lease_name.clone(),
        lease_namespace: namespace.clone(),
        identity: identity.clone(),
        lease_duration: Duration::from_secs(15),
        renew_deadline: Duration::from_secs(10),
        retry_period: Duration::from_secs(2),
    };
    let writers_gone_after = waited.elapsed();
    let acquiring = std::time::Instant::now();
    let hold = match LeaseHold::acquire(ha, args.wait).await {
        Ok(h) => h,
        Err(e) => {
            return (
                Guard {
                    checks: vec![
                        Check::fail("lease", e).took(acquiring.elapsed()),
                        Check::pass(
                            "writers-stopped",
                            format!(
                                "no pods match {}; writers gone after {}",
                                args.writer_selector,
                                super::preflight::duration(writers_gone_after)
                            ),
                        ),
                    ],
                    held: Box::new(|| false),
                },
                None,
            );
        }
    };
    let mut checks = vec![
        Check::pass(
            "lease",
            format!("{namespace}/{} held as {identity}", args.lease_name),
        )
        .took(acquiring.elapsed()),
    ];
    let listing = std::time::Instant::now();
    let check =
        match wait_for_no_writers(&client, &namespace, &args.writer_selector, Duration::ZERO).await
        {
            Ok(left) if left.is_empty() => Check::pass(
                "writers-stopped",
                format!(
                    "no pods match {}; writers gone after {}",
                    args.writer_selector,
                    super::preflight::duration(writers_gone_after)
                ),
            ),
            Ok(left) => Check::fail(
                "writers-stopped",
                format!("forklift pods appeared: {}", left.join(", ")),
            ),
            Err(e) => Check::fail("writers-stopped", e),
        };
    checks.push(check.took(listing.elapsed()));
    let guard = Guard {
        checks,
        held: hold.watch(),
    };
    (guard, Some(hold))
}

/// Waits until the store answers, so a target starting alongside the Job (or
/// a bucket its chart hook has yet to create) is not mistaken for a failure.
async fn wait_reachable(e: &Endpoint, wait: Duration) -> Result<(), String> {
    objstore::retry_transient("migrate store", wait, Duration::from_secs(1), || async {
        match e.objects.head_object(&e.bucket, &e.meta_key).await {
            Err(err) if err.is_not_found() => Ok(()),
            other => other.map(|_| ()),
        }
    })
    .await
    .map_err(|err| format!("{} is not reachable: {err}", e.describe()))
}

async fn open_pair(
    source: &config::S3Config,
    target: &config::S3Config,
    wait: Duration,
) -> Result<(OpenEndpoint, OpenEndpoint), String> {
    let src = endpoint(source).await?;
    let dst = endpoint(target).await?;
    wait_reachable(&src.endpoint, wait).await?;
    wait_reachable(&dst.endpoint, wait).await?;
    Ok((src, dst))
}

/// Runs the interactive flow: prompt, preflight and plan, confirmation, copy.
/// `Ok(None)` when the operator declines.
pub async fn run_interactive(
    c: &mut dyn Console,
    source: &config::S3Config,
    args: &Args,
) -> Result<Option<Report>, String> {
    let io_err = |e: io::Error| e.to_string();
    c.say(&format!("Source: {}", describe_config(source)))
        .map_err(io_err)?;
    let target = prompt_target(c, &target_from_env()).map_err(io_err)?;
    let (src, dst) = open_pair(source, &target, args.wait).await?;
    c.say(&format!("Target: {}", dst.endpoint.describe()))
        .map_err(io_err)?;

    let mut overwrite = args.overwrite_meta;
    if !overwrite && has_meta(&dst.endpoint).await.map_err(|e| e.to_string())? {
        overwrite = ask_yes(
            c,
            "The target already holds a metadata snapshot. Replace it?",
            false,
        )
        .map_err(io_err)?;
        if !overwrite {
            return Ok(None);
        }
    }
    c.say("Running preflight checks...").map_err(io_err)?;
    let (plan_guard, plan_hold) = guard(args).await;
    let plan = migrate(
        &src.endpoint,
        &dst.endpoint,
        &options(args, true, overwrite),
        plan_guard,
    )
    .await;
    if let Some(h) = plan_hold {
        h.release().await;
    }
    let plan = plan.map_err(|e| e.to_string())?;
    c.say(&render(&plan.preflight)).map_err(io_err)?;
    c.say(&plan_summary(&plan)).map_err(io_err)?;
    if args.dry_run {
        return Ok(Some(plan));
    }
    if !ask_yes(c, "Proceed with the copy?", false).map_err(io_err)? {
        return Ok(None);
    }
    let (run_guard, run_hold) = guard(args).await;
    let report = migrate(
        &src.endpoint,
        &dst.endpoint,
        &options(args, false, overwrite),
        run_guard,
    )
    .await;
    if let Some(h) = run_hold {
        h.release().await;
    }
    let report = report.map_err(|e| e.to_string())?;
    c.say(&render(&report.postflight)).map_err(io_err)?;
    Ok(Some(report))
}

pub async fn run(source: &config::S3Config, args: &Args) -> Result<Report, String> {
    let target = target_from_env();
    if target.bucket.is_empty() {
        return Err("FORKLIFT_MIGRATE_TO_S3_BUCKET is required (or pass --interactive)".into());
    }
    let (src, dst) = open_pair(source, &target, args.wait).await?;
    let (g, hold) = guard(args).await;
    let result = migrate(
        &src.endpoint,
        &dst.endpoint,
        &options(args, args.dry_run, args.overwrite_meta),
        g,
    )
    .await;
    if let Some(h) = hold {
        h.release().await;
    }
    result.map_err(|e| e.to_string())
}

fn describe_config(s3: &config::S3Config) -> String {
    let endpoint = normalize_endpoint(&s3.endpoint);
    let endpoint = if endpoint.is_empty() {
        "s3://".to_string()
    } else {
        endpoint
    };
    let prefix = s3.prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{endpoint} bucket={}", s3.bucket)
    } else {
        format!("{endpoint} bucket={} prefix={prefix}", s3.bucket)
    }
}

pub struct OpenEndpoint {
    pub endpoint: Endpoint,
    _staging: tempfile::TempDir,
}

pub(crate) fn normalize_endpoint(endpoint: &str) -> String {
    let e = endpoint.trim().trim_end_matches('/');
    match url::Url::parse(e) {
        Ok(u) if u.host_str().is_some() => {
            let port = u
                .port_or_known_default()
                .map(|p| format!(":{p}"))
                .unwrap_or_default();
            format!(
                "{}://{}{port}",
                u.scheme(),
                u.host_str().unwrap_or_default().to_ascii_lowercase()
            )
        }
        _ => e.to_ascii_lowercase(),
    }
}

async fn endpoint(s3: &config::S3Config) -> Result<OpenEndpoint, String> {
    let cfg = storage::S3Config {
        bucket: s3.bucket.clone(),
        prefix: s3.prefix.clone(),
        region: s3.region.clone(),
        endpoint: s3.endpoint.clone(),
        force_path_style: s3.force_path_style,
        access_key_id: s3.access_key_id.clone(),
        secret_access_key: s3.secret_access_key.clone(),
    };
    let staging = tempfile::tempdir().map_err(|e| format!("staging dir: {e}"))?;
    let blobs = storage::S3BlobStore::new(&cfg, staging.path())
        .await
        .map_err(|e| format!("open bucket {}: {e}", s3.bucket))?;
    let client = blobs.client();
    let admin = match (!s3.provider.is_empty())
        .then(|| storage::admin::provider(&s3.provider))
        .flatten()
    {
        Some(spec) => storage::cluster_admin(
            spec,
            &storage::AdminConfig {
                s3_endpoint: s3.endpoint.clone(),
                admin_endpoint: s3.admin_endpoint.clone(),
                region: s3.region.clone(),
                access_key_id: s3.access_key_id.clone(),
                secret_access_key: s3.secret_access_key.clone(),
                admin_token: s3.admin_token.clone(),
            },
        )
        .ok()
        .flatten(),
        None => None,
    };
    Ok(OpenEndpoint {
        endpoint: Endpoint {
            blobs: Arc::new(blobs),
            objects: Arc::new(S3Api::new(client)),
            bucket: s3.bucket.clone(),
            prefix: s3.prefix.trim_matches('/').to_string(),
            meta_key: objstore::meta_key(&s3.prefix),
            endpoint: normalize_endpoint(&s3.endpoint),
            admin,
        },
        _staging: staging,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::migrate::cli::*;

    #[derive(Default)]
    struct Script {
        answers: VecDeque<String>,
        secrets: Vec<String>,
        said: Vec<String>,
    }

    impl Script {
        fn new(answers: &[&str]) -> Script {
            Script {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                ..Script::default()
            }
        }
    }

    impl Console for Script {
        fn line(&mut self, _prompt: &str) -> io::Result<String> {
            self.answers
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no answer"))
        }

        fn secret(&mut self, prompt: &str) -> io::Result<String> {
            self.secrets.push(prompt.to_string());
            self.line(prompt)
        }

        fn say(&mut self, text: &str) -> io::Result<()> {
            self.said.push(text.to_string());
            Ok(())
        }
    }

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags() {
        let got = parse_args(&strings(&[
            "-i",
            "--dry-run",
            "-overwrite-meta",
            "--concurrency=3",
            "-require-conditional-writes",
            "-allow-missing-source-blobs",
            "-lease-name",
            "fk-leader",
            "-writer-selector=app=forklift",
            "-wait=90s",
            "-staging",
            "/scratch",
            "-verify=sample",
            "-verify-sample-percent",
            "10",
        ]))
        .unwrap();
        assert_eq!(
            got,
            Args {
                interactive: true,
                dry_run: true,
                overwrite_meta: true,
                concurrency: 3,
                require_conditional_writes: true,
                allow_missing_source_blobs: true,
                lease_name: "fk-leader".into(),
                writer_selector: "app=forklift".into(),
                wait: Duration::from_secs(90),
                staging: PathBuf::from("/scratch"),
                verify: super::super::Verify::Sample { percent: 10 },
            }
        );
        assert_eq!(
            parse_args(&strings(&["-verify=full"])).unwrap().verify,
            super::super::Verify::Full
        );
        for bad in [
            &["-concurrency", "0"][..],
            &["-concurrency", "999"],
            &["-concurrency"],
            &["-wait", "soon"],
            &["-nope"],
            &["-lease-name", "x"],
            &["-verify=most"],
            &["-verify-sample-percent=0"],
        ] {
            assert!(parse_args(&strings(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(parse_args(&strings(&["-h"])).unwrap_err(), "");
    }

    #[test]
    fn prompts_with_defaults_and_hidden_secret() {
        let defaults = config::S3Config {
            bucket: "forklift".into(),
            secret_access_key: "from-env".into(),
            ..Default::default()
        };
        let mut c = Script::new(&["http://sw:8333", "", "", "us-east-1", "", "ak", ""]);
        let t = prompt_target(&mut c, &defaults).unwrap();
        assert_eq!(t.endpoint, "http://sw:8333");
        assert_eq!(t.bucket, "forklift", "empty answer keeps the default");
        assert_eq!(t.region, "us-east-1");
        assert!(t.force_path_style, "defaults on with a custom endpoint");
        assert_eq!(t.access_key_id, "ak");
        assert_eq!(t.secret_access_key, "from-env");
        assert_eq!(c.secrets.len(), 1, "secret read without echo");
    }

    #[test]
    fn bucket_is_required() {
        let mut c = Script::new(&["", "", "b", "", "", "n", ""]);
        let t = prompt_target(&mut c, &config::S3Config::default()).unwrap();
        assert_eq!(t.bucket, "b");
        assert!(!t.force_path_style);
        assert!(c.said.iter().any(|s| s.contains("required")));
        assert!(
            c.secrets.is_empty(),
            "no secret prompt without an access key"
        );
    }

    #[test]
    fn terminal_reads_lines_and_eof() {
        let mut t = Terminal {
            input: io::Cursor::new(b"value\n".to_vec()),
            output: Vec::new(),
        };
        assert_eq!(t.line("q: ").unwrap(), "value");
        assert!(t.line("q: ").is_err(), "eof");
        t.say("done").unwrap();
        let out = String::from_utf8(t.output).unwrap();
        assert!(out.contains("q: ") && out.ends_with("done\n"), "{out:?}");
    }

    #[test]
    fn endpoints_normalise_for_comparison() {
        assert_eq!(
            normalize_endpoint("HTTP://MinIO:9000/"),
            "http://minio:9000"
        );
        assert_eq!(
            normalize_endpoint("https://s3.example.com"),
            "https://s3.example.com:443"
        );
        assert_eq!(
            normalize_endpoint("http://s3.example.com:80"),
            "http://s3.example.com:80"
        );
        assert_eq!(normalize_endpoint(""), "");
    }

    #[test]
    fn describes_configs() {
        assert_eq!(
            describe_config(&config::S3Config {
                endpoint: "http://M:9000/".into(),
                bucket: "b".into(),
                prefix: "/p/".into(),
                ..Default::default()
            }),
            "http://m:9000 bucket=b prefix=p"
        );
        assert_eq!(
            describe_config(&config::S3Config {
                bucket: "b".into(),
                ..Default::default()
            }),
            "s3:// bucket=b"
        );
    }

    #[test]
    fn summaries_are_readable() {
        let r = Report {
            required: 3,
            copied: 2,
            skipped: 1,
            bytes_copied: 3 * 1024 * 1024,
            ..Report::default()
        };
        assert_eq!(
            plan_summary(&r),
            "3 blobs named by the metadata: 2 to copy (3.0 MiB), 1 already in the target. The metadata snapshot is copied last."
        );
    }

    #[tokio::test]
    async fn no_lease_means_an_unverified_warning() {
        let (g, hold) = guard(&Args::default()).await;
        assert!(hold.is_none());
        assert_eq!(g.checks.len(), 1);
        assert_eq!(g.checks[0].status, crate::migrate::Status::Warn);
        assert!((g.held)());
    }

    #[tokio::test]
    async fn lease_outside_a_cluster_fails_the_guard() {
        let args = Args {
            lease_name: "fk-leader".into(),
            writer_selector: "app=forklift".into(),
            wait: Duration::from_millis(10),
            ..Args::default()
        };
        let (g, hold) = guard(&args).await;
        assert!(hold.is_none());
        assert_eq!(g.checks[0].status, crate::migrate::Status::Fail);
        assert!(!(g.held)());
    }
}
