//! Integrity checks after the copy: the uploaded snapshot is byte-identical,
//! every named blob has the recorded size, and a sample (or all) of them hash
//! to their digest. A failure removes what it proved bad, so a rerun copies it
//! again instead of skipping it as present.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::preflight::{Check, Status, bytes, push};
use super::snapshot::{self, StagedSnapshot};
use super::{Endpoint, Error, Options, Result};

pub const CHECKS: &[(&str, &str)] = &[
    ("PV01", "target-metadata-hash"),
    ("PV02", "target-blob-sizes"),
    ("PV03", "target-blob-content"),
    ("PV04", "source-unchanged"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    Off,
    /// Hashes this share of the named blobs, at least [`MIN_SAMPLE`], always
    /// starting with the ones the copy skipped as already present.
    Sample {
        percent: u8,
    },
    Full,
}

impl Verify {
    pub fn parse(mode: &str, percent: u8) -> Option<Self> {
        match mode {
            "off" => Some(Self::Off),
            "sample" if (1..=100).contains(&percent) => Some(Self::Sample { percent }),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

impl std::fmt::Display for Verify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::Sample { percent } => write!(f, "sample {percent}%"),
            Self::Full => write!(f, "full"),
        }
    }
}

pub const MIN_SAMPLE: usize = 20;

pub struct Outcome {
    pub checks: Vec<Check>,
    /// Target blobs proven wrong; the caller deletes them.
    pub bad_blobs: Vec<String>,
    pub verified_blobs: usize,
}

pub async fn run(
    src: &Endpoint,
    dst: &Endpoint,
    snap: &StagedSnapshot,
    skipped: &BTreeSet<String>,
    source_missing: &BTreeSet<String>,
    opts: &Options,
) -> Outcome {
    let (verify, concurrency, staging) = (opts.verify, opts.concurrency, opts.staging.as_path());
    tracing::info!("postflight: downloading the target metadata snapshot");
    let mut checks = Vec::new();
    push(&mut checks, metadata_hash(dst, snap, staging).await);
    let named: BTreeMap<&String, Option<i64>> = snap
        .blobs
        .iter()
        .filter(|(d, _)| !source_missing.contains(*d))
        .map(|(d, s)| (d, *s))
        .collect();

    tracing::info!(
        blobs = named.len(),
        "postflight: checking blob sizes in the target"
    );
    let (sizes, mut bad) = blob_sizes(dst, &named, concurrency).await;
    push(&mut checks, sizes);

    let pool: Vec<&String> = named
        .keys()
        .copied()
        .filter(|d| !bad.contains(*d))
        .collect();
    let picked = pick(&pool, skipped, verify);
    tracing::info!(blobs = picked.len(), verify = %verify, "postflight: re-hashing blobs in the target");
    let (content, wrong, verified) =
        blob_content(dst, &picked, named.len(), verify, concurrency).await;
    push(&mut checks, content);
    bad.extend(wrong);

    push(&mut checks, source_unchanged(src, snap).await);
    checks.sort_by_key(|c| c.id);
    Outcome {
        checks,
        bad_blobs: bad.into_iter().collect(),
        verified_blobs: verified,
    }
}

async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Downloads the target snapshot again and compares it to the staged one
/// byte for byte (by hash), then re-reads its blob references.
async fn metadata_hash(dst: &Endpoint, snap: &StagedSnapshot, staging: &Path) -> Check {
    let started = Instant::now();
    let result: std::result::Result<String, String> = async {
        let want = sha256_file(snap.file.path())
            .await
            .map_err(|e| format!("hash staged snapshot: {e}"))?;
        let mut obj = dst
            .objects
            .get_object(&dst.bucket, &dst.meta_key)
            .await
            .map_err(|e| format!("download target snapshot: {e}"))?;
        let copy = tempfile::NamedTempFile::new_in(staging).map_err(|e| e.to_string())?;
        let mut out = tokio::fs::File::create(copy.path())
            .await
            .map_err(|e| e.to_string())?;
        tokio::io::copy(&mut obj.body, &mut out)
            .await
            .map_err(|e| format!("download target snapshot: {e}"))?;
        out.flush().await.map_err(|e| e.to_string())?;
        let got = sha256_file(copy.path()).await.map_err(|e| e.to_string())?;
        if got != want {
            return Err(format!(
                "target snapshot sha256 {got} differs from the staged {want}"
            ));
        }
        let path = copy.path().to_path_buf();
        let (blobs, _) = tokio::task::spawn_blocking(move || snapshot::inspect(&path))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if blobs.keys().ne(snap.blobs.keys()) {
            return Err("target snapshot names a different set of blobs".into());
        }
        Ok(want)
    }
    .await;
    let check = match result {
        Ok(sha) => Check::pass(
            "target-metadata-hash",
            format!(
                "sha256 {}... matches, {} ({})",
                &sha[..12],
                bytes(snap.size),
                "integrity ok"
            ),
        ),
        Err(e) => Check::fail("target-metadata-hash", e),
    };
    check.took(started.elapsed())
}

async fn blob_sizes(
    dst: &Endpoint,
    named: &BTreeMap<&String, Option<i64>>,
    concurrency: usize,
) -> (Check, BTreeSet<String>) {
    let started = Instant::now();
    let results: Vec<_> = stream::iter(named.iter())
        .map(|(d, want)| async move {
            let one = Instant::now();
            ((*d).clone(), *want, dst.blobs.size(d).await, one.elapsed())
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let mut bad = BTreeSet::new();
    let (mut missing, mut mismatched, mut unsized_) = (Vec::new(), Vec::new(), 0usize);
    let mut error = None;
    let mut latencies = Vec::with_capacity(results.len());
    for (d, want, got, took) in results {
        latencies.push(took);
        match (got, want) {
            (Err(e), _) => {
                error.get_or_insert_with(|| e.to_string());
            }
            (Ok(None), _) => missing.push(d),
            (Ok(Some(n)), Some(w)) if n != w => {
                mismatched.push(format!("{d} is {n} bytes, recorded {w}"));
                bad.insert(d);
            }
            (Ok(Some(_)), None) => unsized_ += 1,
            (Ok(Some(_)), Some(_)) => {}
        }
    }
    let rate = super::preflight::per_request(&latencies);
    let check = if let Some(e) = error {
        Check::fail("target-blob-sizes", format!("cannot read sizes: {e}"))
    } else if !missing.is_empty() {
        Check::fail(
            "target-blob-sizes",
            format!(
                "{} named blobs are missing (first: {}){rate}",
                missing.len(),
                missing[0]
            ),
        )
    } else if !mismatched.is_empty() {
        Check::fail(
            "target-blob-sizes",
            format!(
                "{} blobs have the wrong size (first: {}){rate}",
                mismatched.len(),
                mismatched[0]
            ),
        )
    } else {
        let note = if unsized_ > 0 {
            format!(", {unsized_} without a recorded size")
        } else {
            String::new()
        };
        Check::pass(
            "target-blob-sizes",
            format!(
                "all {} named blobs match their recorded size{note}{rate}",
                named.len()
            ),
        )
    };
    (check.took(started.elapsed()), bad)
}

/// Skipped blobs come first: the copy never read them, while every copied
/// blob was already hashed on its way in.
fn pick<'a>(pool: &[&'a String], skipped: &BTreeSet<String>, verify: Verify) -> Vec<&'a String> {
    let quota = match verify {
        Verify::Off => return Vec::new(),
        Verify::Full => pool.len(),
        Verify::Sample { percent } => {
            let share = (pool.len() * percent as usize).div_ceil(100);
            share.max(MIN_SAMPLE).min(pool.len())
        }
    };
    let mut keyed: Vec<(bool, u64, &String)> = pool
        .iter()
        .map(|d| (!skipped.contains(*d), rand::random::<u64>(), *d))
        .collect();
    keyed.sort_unstable();
    keyed.into_iter().take(quota).map(|(_, _, d)| d).collect()
}

async fn blob_content(
    dst: &Endpoint,
    picked: &[&String],
    named: usize,
    verify: Verify,
    concurrency: usize,
) -> (Check, Vec<String>, usize) {
    if verify == Verify::Off {
        return (
            Check::new(
                "target-blob-content",
                Status::Skip,
                "verification is off (--verify=off)",
            ),
            Vec::new(),
            0,
        );
    }
    let started = Instant::now();
    let results: Vec<_> = stream::iter(picked.iter())
        .map(|d| async move { ((*d).clone(), hash_blob(dst, d).await) })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let elapsed = started.elapsed();
    let mut wrong = Vec::new();
    let mut error = None;
    let mut read = 0u64;
    for (d, r) in results {
        match r {
            Ok((got, n)) => {
                read += n;
                if got != d {
                    wrong.push(d);
                }
            }
            Err(e) => {
                error.get_or_insert_with(|| format!("{d}: {e}"));
            }
        }
    }
    let mode = verify.to_string();
    let throughput = if elapsed > Duration::ZERO {
        format!(
            ", {}/s",
            bytes((read as f64 / elapsed.as_secs_f64()) as u64)
        )
    } else {
        String::new()
    };
    let check = if let Some(e) = error {
        Check::fail("target-blob-content", format!("cannot read: {e}"))
    } else if !wrong.is_empty() {
        Check::fail(
            "target-blob-content",
            format!(
                "{} of {} hashed blobs do not match their digest (first: {})",
                wrong.len(),
                picked.len(),
                wrong[0]
            ),
        )
    } else {
        Check::pass(
            "target-blob-content",
            format!(
                "{} of {named} named blobs re-hashed ({mode}), {} read{throughput}",
                picked.len(),
                bytes(read)
            ),
        )
    };
    (check.took(elapsed), wrong, picked.len())
}

async fn hash_blob(dst: &Endpoint, digest: &str) -> crate::storage::Result<(String, u64)> {
    let (mut r, _) = dst.blobs.open(digest).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r
            .read(&mut buf)
            .await
            .map_err(|e| crate::storage::Error::io("read blob", e))?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex::encode(h.finalize()), n))
}

async fn source_unchanged(src: &Endpoint, snap: &StagedSnapshot) -> Check {
    let started = Instant::now();
    let check = match src.objects.head_object(&src.bucket, &src.meta_key).await {
        Ok(head) if head.e_tag == snap.etag => Check::pass(
            "source-unchanged",
            "source snapshot ETag unchanged since preflight",
        ),
        Ok(_) => Check::fail(
            "source-unchanged",
            "source snapshot changed after the copy; a writer reached the source",
        ),
        Err(e) => Check::fail("source-unchanged", e.to_string()),
    };
    check.took(started.elapsed())
}

/// Removes what postflight proved wrong so a rerun copies it again.
pub async fn remediate(dst: &Endpoint, bad_blobs: &[String]) -> Result<String> {
    for d in bad_blobs {
        dst.blobs.delete(d).await.map_err(|source| Error::Blob {
            digest: d.clone(),
            source,
        })?;
    }
    dst.objects
        .delete_object(&dst.bucket, &dst.meta_key)
        .await?;
    Ok(format!(
        "removed {} bad target blobs and the target metadata snapshot; rerun to copy them again",
        bad_blobs.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digests(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:064x}")).collect()
    }

    #[test]
    fn verify_modes_parse() {
        assert_eq!(Verify::parse("off", 5), Some(Verify::Off));
        assert_eq!(Verify::parse("full", 0), Some(Verify::Full));
        assert_eq!(
            Verify::parse("sample", 5),
            Some(Verify::Sample { percent: 5 })
        );
        assert_eq!(Verify::parse("sample", 0), None);
        assert_eq!(Verify::parse("sample", 101), None);
        assert_eq!(Verify::parse("most", 5), None);
    }

    #[test]
    fn sampling_prefers_skipped_blobs_and_honours_the_floor() {
        let all = digests(1000);
        let pool: Vec<&String> = all.iter().collect();
        let skipped: BTreeSet<String> = all[..30].iter().cloned().collect();

        let picked = pick(&pool, &skipped, Verify::Sample { percent: 2 });
        assert_eq!(picked.len(), 20, "2% of 1000");
        assert!(
            picked.iter().all(|d| skipped.contains(*d)),
            "skipped blobs first"
        );
        let picked = pick(&pool, &skipped, Verify::Sample { percent: 1 });
        assert_eq!(picked.len(), MIN_SAMPLE);
        assert!(
            picked.iter().all(|d| skipped.contains(*d)),
            "skipped blobs first"
        );

        let small: Vec<&String> = all[..5].iter().collect();
        assert_eq!(
            pick(&small, &BTreeSet::new(), Verify::Sample { percent: 1 }).len(),
            5
        );
        assert_eq!(pick(&pool, &skipped, Verify::Full).len(), 1000);
        assert_eq!(
            pick(&pool, &skipped, Verify::Off),
            [] as [&std::string::String; 0]
        );
    }
}
