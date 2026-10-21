//! WAL shipping: how a [`SyncMode::Wal`](super::SyncMode::Wal) leader keeps
//! the bucket current, and how every other process rebuilds the database
//! from it.
//!
//! The bucket holds one generation at a time: a base copy of the database file
//! at the snapshot key, tagged with the generation id, plus the frames
//! committed after it under `wal/<generation>/<seq>.seg`, one new key per
//! segment. Shipping a change never overwrites an object. Only a new
//! generation replaces the base, and the previous generation's segments are
//! deleted once the new base is in place.
//!
//! The leader runs every checkpoint itself: automatic checkpoints are off and
//! the read pool is `query_only`, and a checkpoint only runs while the leader
//! holds the write connection, after it has read every committed frame. No
//! frame therefore reaches the database file without first being captured, and
//! the file changes only when the leader checkpoints, which is what lets a
//! plain copy taken right after a checkpoint serve as a generation's base.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use prometheus::Gauge;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use super::{
    Error, HeadObjectOutput, MetaSync, ObjectApi, PutBody, PutObjectInput, Result, SyncState,
};
use crate::meta;
use crate::objstore::snapcodec::{self, Compression};
use crate::objstore::walfmt::{self, Cursor, Frame, WalRead};

/// How often a WAL-mode leader captures and ships committed frames, which
/// bounds how much a crash can lose.
pub(super) const SHIP_TICK: Duration = Duration::from_secs(5);
/// The base object's metadata key naming its generation. A base without it is
/// a self-contained snapshot.
pub(super) const GENERATION_META_KEY: &str = "generation";
/// The base object's metadata key holding the SHA-256 of its bytes.
pub(super) const SHA256_META_KEY: &str = "sha256";

/// Page bytes in one segment, and the largest single transaction captured.
/// A bigger transaction (an index build, a VACUUM) is never held in memory:
/// the generation is replaced and the new base carries it.
const SEGMENT_FRAME_BYTES: usize = 16 << 20;
/// Page bytes captured per round. A cycle runs several rounds, uploading
/// between them, so memory stays bounded however much the WAL holds.
const CAPTURE_BYTES: usize = 16 << 20;
/// The WAL size past which a fully shipped WAL is checkpointed.
const CHECKPOINT_WAL_BYTES: u64 = 64 << 20;
/// How long a checkpoint waits for readers. Short, because it holds the write
/// connection meanwhile. A busy checkpoint is retried next cycle.
const CHECKPOINT_BUSY: Duration = Duration::from_millis(250);
/// Encoded segments held while the bucket is unreachable, kept well under
/// the chart's default memory limit. Past this the generation is abandoned
/// and the next base carries the changes instead.
const MAX_PENDING_BYTES: usize = 32 << 20;
/// A generation is replaced once its segments outgrow its base (and this
/// floor), or reach [`MAX_SEGMENTS`], which bounds both restore time and the
/// garbage a replacement leaves.
const ROTATE_MIN_BYTES: u64 = 64 << 20;
const MAX_SEGMENTS: u64 = 2000;
/// Capture rounds per cycle. A cycle keeps capturing until the WAL is read to
/// its end, so a final flush leaves nothing behind, but writers that outpace
/// it cannot hold one cycle forever.
const MAX_ROUNDS: usize = 64;

/// The leader's WAL shipping state.
#[derive(Default)]
pub(super) struct Shipper {
    generation: Option<Generation>,
    cursor: Option<Cursor>,
    /// Every committed frame was read and a checkpoint attempted since, so a
    /// WAL restart (new salts) cannot have dropped an unread frame.
    restart_ok: bool,
    pending: VecDeque<Pending>,
    pending_bytes: usize,
}

struct Generation {
    id: String,
    next_seq: u64,
    next_frame: u64,
    base: Base,
    base_bytes: u64,
    segment_bytes: u64,
    segments: u64,
    /// Frames of this generation were checkpointed without being captured,
    /// so it takes no further segments and is replaced.
    broken: bool,
}

enum Base {
    Local {
        path: PathBuf,
        size: u64,
        raw: u64,
        sha: String,
    },
    Published,
}

struct Pending {
    generation: String,
    seq: u64,
    bytes: Vec<u8>,
}

impl Shipper {
    fn rotation_wanted(&self, final_flush: bool) -> bool {
        match &self.generation {
            None => true,
            Some(g) if g.broken => true,
            Some(_) if final_flush => false,
            Some(g) => {
                matches!(g.base, Base::Published)
                    && (g.segments >= MAX_SEGMENTS
                        || g.segment_bytes >= g.base_bytes.max(ROTATE_MIN_BYTES))
            }
        }
    }

    fn drop_pending_of(&mut self, id: &str) {
        self.pending.retain(|p| p.generation != id);
        self.pending_bytes = self.pending.iter().map(|p| p.bytes.len()).sum();
    }

    fn drop_pending_except(&mut self, id: &str) {
        self.pending.retain(|p| p.generation == id);
        self.pending_bytes = self.pending.iter().map(|p| p.bytes.len()).sum();
    }

    /// Forgets all shipping state, as on demotion: the next term starts its
    /// own generation.
    pub(super) fn discard(&mut self, pending_gauge: &Gauge) {
        if let Some(Generation {
            base: Base::Local { path, .. },
            ..
        }) = &self.generation
        {
            let _ = std::fs::remove_file(path);
        }
        *self = Self::default();
        pending_gauge.set(0.0);
    }

    fn mark_broken(&mut self, why: &str) {
        if let Some(g) = self.generation.as_mut()
            && !g.broken
        {
            g.broken = true;
            tracing::warn!(generation = %g.id, why, "objstore: wal generation abandoned; the next base carries its changes");
        }
    }
}

/// A standby's copy of a generation: its base plus the segments applied so
/// far.
pub(super) struct Replica {
    generation: String,
    base_etag: Option<String>,
    path: PathBuf,
    next_seq: u64,
    next_frame: u64,
    /// Segments were applied since the copy was last integrity-checked.
    dirty: bool,
}

/// What [`capture`] read and did, under the write connection.
struct Captured {
    batches: Vec<(u32, Vec<Frame>)>,
    cursor: Option<Cursor>,
    restart_ok: bool,
    truncated: bool,
    /// The WAL was truncated to start a generation: asked for, or forced by a
    /// transaction too large to capture.
    rotated: bool,
    /// Every committed frame in the WAL was read.
    drained: bool,
    /// Committed frames were folded into the database file without being
    /// captured.
    lost: bool,
}

/// Reads committed frames and, when asked or when the WAL is large and fully
/// read, checkpoints it with TRUNCATE. Runs on the write connection, so no
/// commit can land between the read and the checkpoint. With `read` false
/// (no generation could take the frames) it only checkpoints.
fn capture(
    conn: &rusqlite::Connection,
    wal: &Path,
    mut cursor: Option<Cursor>,
    mut restart_ok: bool,
    rotate: bool,
    read: bool,
) -> meta::Result<Captured> {
    let mut batches = Vec::new();
    let mut taken = 0usize;
    let mut complete = false;
    let mut reset = false;
    let mut oversized = false;
    if read {
        loop {
            let read = walfmt::read_committed(wal, cursor, SEGMENT_FRAME_BYTES)
                .map_err(|e| meta::Error::Other(format!("read wal: {e}")))?;
            match read {
                WalRead::Reset if restart_ok && cursor.is_some() => {
                    cursor = None;
                    restart_ok = false;
                }
                WalRead::Reset => {
                    reset = true;
                    cursor = None;
                    break;
                }
                WalRead::Frames {
                    frames,
                    cursor: next,
                    complete: done,
                    oversized: big,
                } => {
                    cursor = next;
                    if let Some(c) = next
                        && !frames.is_empty()
                    {
                        restart_ok = false;
                        taken += frames.len() * c.page_size() as usize;
                        batches.push((c.page_size(), frames));
                    }
                    if done {
                        complete = true;
                        break;
                    }
                    if big {
                        oversized = true;
                        break;
                    }
                    if taken >= CAPTURE_BYTES {
                        break;
                    }
                }
            }
        }
    }
    let drained = read && complete && !reset;
    let wal_len = std::fs::metadata(wal).map_or(0, |m| m.len());
    let mut truncated = false;
    if rotate || oversized || (drained && wal_len >= CHECKPOINT_WAL_BYTES) {
        conn.busy_timeout(CHECKPOINT_BUSY)
            .map_err(|e| meta::Error::sqlite("set busy timeout", e))?;
        let busy = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(|e| meta::Error::sqlite("checkpoint wal", e));
        conn.busy_timeout(meta::BUSY_TIMEOUT)
            .map_err(|e| meta::Error::sqlite("set busy timeout", e))?;
        if busy? == 0 {
            truncated = true;
            cursor = None;
            restart_ok = false;
        } else if drained {
            restart_ok = true;
        }
    }
    Ok(Captured {
        batches,
        cursor,
        restart_ok,
        truncated,
        rotated: truncated && (rotate || oversized),
        drained,
        lost: reset || (truncated && !drained),
    })
}

/// Local base copies are named `base-<generation>.db[.zst]` in the work
/// directory.
const BASE_FILE_PREFIX: &str = "base-";

/// Removes base copies left by an earlier generation or a crashed process.
/// Each is a full database copy, and the work directory outlives a container
/// restart.
fn remove_stale_bases(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(BASE_FILE_PREFIX)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn wal_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    PathBuf::from(p)
}

/// Looks up an S3 user-metadata value; keys are case-insensitive.
pub(super) fn meta_value(md: &HashMap<String, String>, key: &str) -> Option<String> {
    md.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

/// The generation a base object belongs to, `None` for a self-contained
/// snapshot. An id that could not have been minted here is ignored rather
/// than spliced into an object key.
pub(super) fn generation_of(head: &HeadObjectOutput) -> Option<String> {
    meta_value(&head.metadata, GENERATION_META_KEY).filter(|g| valid_generation(g))
}

fn valid_generation(g: &str) -> bool {
    !g.is_empty() && g.len() <= 64 && g.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Generation ids sort by term, then time, so a listing shows them in order.
fn new_generation_id(fence: i64) -> String {
    let millis = chrono::Utc::now().timestamp_millis().max(0);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("{:012}-{millis:013}-{}", fence.max(0), &nonce[..8])
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// Runs `PRAGMA quick_check` on a database file without touching it: the
/// `immutable` URI opens no WAL or shared-memory file next to it.
fn verify_db(path: &Path) -> std::result::Result<(), String> {
    let uri = format!("file:{}?immutable=1", path.display());
    let conn = rusqlite::Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("open: {e}"))?;
    let verdict: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| format!("quick_check: {e}"))?;
    if verdict == "ok" {
        Ok(())
    } else {
        Err(format!("quick_check: {verdict}"))
    }
}

fn blocking_err(op: &'static str) -> impl FnOnce(tokio::task::JoinError) -> Error {
    move |e| Error::io(op, std::io::Error::other(e))
}

/// The prefix WAL segments live under, next to the snapshot `key`.
fn wal_prefix_for(key: &str) -> String {
    match key.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/wal/"),
        None => "wal/".to_owned(),
    }
}

fn segment_seq(key: &str, generation_prefix: &str) -> Option<u64> {
    key.strip_prefix(generation_prefix)
        .and_then(|k| k.strip_suffix(".seg"))
        .filter(|n| n.len() == 20 && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
}

/// The segment keys of a generation that a reader can apply: those numbered
/// contiguously from 0.
fn contiguous_segments(keys: Vec<String>, generation_prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        match segment_seq(&key, generation_prefix) {
            Some(seq) if seq == out.len() as u64 => out.push(key),
            Some(_) => break,
            None => {}
        }
    }
    out
}

/// Identifies the metadata the bucket holds now: the base object's ETag and,
/// for a WAL generation, how many segments extend it. It changes whenever a
/// leader publishes anything. `None` when there is no snapshot.
pub async fn metadata_version(
    api: &dyn ObjectApi,
    bucket: &str,
    key: &str,
) -> Result<Option<String>> {
    let head = match api.head_object(bucket, key).await {
        Ok(h) => h,
        Err(e) if e.is_not_found() => return Ok(None),
        Err(e) => return Err(e),
    };
    let etag = head.e_tag.clone().unwrap_or_default();
    let Some(generation) = generation_of(&head) else {
        return Ok(Some(etag));
    };
    let prefix = format!("{}{generation}/", wal_prefix_for(key));
    let segments = contiguous_segments(api.list_keys(bucket, &prefix).await?, &prefix).len();
    Ok(Some(format!("{etag}|{generation}|{segments}")))
}

/// The database [`fetch_metadata`] rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMetadata {
    /// [`metadata_version`] of what was fetched.
    pub version: String,
    /// Size of the rebuilt SQLite file.
    pub size: u64,
}

/// Rebuilds the database the bucket describes as a plain SQLite file at
/// `dst`: the snapshot, or a WAL generation's base with its contiguous
/// segments replayed. The caller checks the result's integrity. `None` when
/// there is no snapshot.
pub async fn fetch_metadata(
    api: &dyn ObjectApi,
    bucket: &str,
    key: &str,
    dst: &Path,
) -> Result<Option<FetchedMetadata>> {
    let head = match api.head_object(bucket, key).await {
        Ok(h) => h,
        Err(e) if e.is_not_found() => return Ok(None),
        Err(e) => return Err(e),
    };
    let out = api.get_object(bucket, key).await?;
    if head.e_tag.is_some() && out.e_tag.is_some() && head.e_tag != out.e_tag {
        return Err(Error::Unavailable(
            "snapshot replaced between HEAD and GET".into(),
        ));
    }
    let tmp = {
        let mut p = dst.as_os_str().to_owned();
        p.push(".download");
        PathBuf::from(p)
    };
    let mut f = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| Error::io("stage metadata", e))?;
    let mut body = out.body;
    let copied = super::copy_hashing(&mut body, &mut f).await;
    drop(f);
    let (_, sha) = match copied {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::io("download metadata", e));
        }
    };
    if let Some(want) = meta_value(&head.metadata, SHA256_META_KEY)
        && !want.eq_ignore_ascii_case(&sha)
    {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(Error::ObjectStore(format!(
            "snapshot sha256 {sha} does not match the recorded {want}"
        )));
    }
    let (raw, plain) = (tmp, dst.to_path_buf());
    tokio::task::spawn_blocking(move || snapcodec::materialize(&raw, &plain))
        .await
        .map_err(blocking_err("stage metadata"))?
        .map_err(|e| Error::io("stage metadata", e))?;

    let etag = head.e_tag.clone().unwrap_or_default();
    let version = match generation_of(&head) {
        None => etag,
        Some(generation) => {
            let prefix = format!("{}{generation}/", wal_prefix_for(key));
            let keys = contiguous_segments(api.list_keys(bucket, &prefix).await?, &prefix);
            let mut next_frame = 0u64;
            for seg_key in &keys {
                let mut body = api.get_object(bucket, seg_key).await?.body;
                let mut bytes = Vec::new();
                body.read_to_end(&mut bytes)
                    .await
                    .map_err(|e| Error::io("download wal segment", e))?;
                let path = dst.to_path_buf();
                let want = next_frame;
                let frames = tokio::task::spawn_blocking(move || {
                    let seg = walfmt::decode_segment(&bytes)?;
                    if seg.first_frame != want {
                        return Err(walfmt::Error::Invalid(format!(
                            "segment starts at frame {}, want {want}",
                            seg.first_frame
                        )));
                    }
                    walfmt::apply_segment(&path, &seg)?;
                    Ok(seg.frames.len() as u64)
                })
                .await
                .map_err(blocking_err("replay wal segment"))?
                .map_err(|e| Error::ObjectStore(format!("wal segment {seg_key}: {e}")))?;
                next_frame += frames;
            }
            format!("{etag}|{generation}|{}", keys.len())
        }
    };
    let size = tokio::fs::metadata(dst)
        .await
        .map_err(|e| Error::io("stat metadata", e))?
        .len();
    Ok(Some(FetchedMetadata { version, size }))
}

impl MetaSync {
    fn wal_prefix(&self) -> String {
        wal_prefix_for(&self.key)
    }

    fn generation_prefix(&self, generation: &str) -> String {
        format!("{}{generation}/", self.wal_prefix())
    }

    fn segment_key(&self, generation: &str, seq: u64) -> String {
        format!("{}{seq:020}.seg", self.generation_prefix(generation))
    }

    /// One leader cycle: capture committed frames, start a generation when
    /// one is due, upload what is pending, and publish an unpublished base.
    /// A final flush (demotion, shutdown) only starts a generation when there
    /// is no usable one, since nothing else could carry the changes.
    pub(super) async fn ship(&self, st: &mut SyncState, final_flush: bool) -> Result<()> {
        for _ in 0..MAX_ROUNDS {
            let rotate = st.ship.rotation_wanted(final_flush);
            let read = st.ship.generation.as_ref().is_some_and(|g| !g.broken);
            let wal = wal_path(self.store.path());
            let (cursor, restart_ok) = (st.ship.cursor, st.ship.restart_ok);
            let cap = self
                .store
                .write(move |conn| capture(conn, &wal, cursor, restart_ok, rotate, read))
                .await
                .map_err(|e| Error::Meta(e).context("capture wal"))?;
            st.ship.cursor = cap.cursor;
            st.ship.restart_ok = cap.restart_ok;
            if cap.lost {
                st.ship
                    .mark_broken("wal frames were checkpointed before they were read");
            }
            let progressed = !cap.batches.is_empty();
            self.queue_segments(st, cap.batches).await;
            if cap.rotated {
                self.start_generation(st).await?;
            }
            self.wal_pending_bytes.set(st.ship.pending_bytes as f64);
            self.upload_pending(st).await?;
            self.publish_base(st).await?;
            if cap.drained || cap.truncated || (!progressed && !cap.lost) {
                return Ok(());
            }
        }
        tracing::warn!("objstore: wal writes outpaced shipping this cycle; continuing next cycle");
        Ok(())
    }

    /// Encodes captured frames as the next segments of the current generation.
    /// Without a usable generation they are dropped: they stay in the database
    /// file and reach the bucket with the next base.
    async fn queue_segments(&self, st: &mut SyncState, batches: Vec<(u32, Vec<Frame>)>) {
        if batches.is_empty() {
            return;
        }
        let Some(g) = st.ship.generation.as_ref().filter(|g| !g.broken) else {
            return;
        };
        let (id, first_seq, first_frame) = (g.id.clone(), g.next_seq, g.next_frame);
        let encoded = tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(batches.len());
            let mut frame = first_frame;
            for (page_size, frames) in batches {
                let bytes = walfmt::encode_segment(page_size, frame, &frames)?;
                frame += frames.len() as u64;
                out.push((bytes, frames.len() as u64));
            }
            walfmt::Result::Ok(out)
        })
        .await;
        let encoded = match encoded {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                st.ship.mark_broken(&format!("encode segment: {e}"));
                return;
            }
            Err(e) => {
                st.ship.mark_broken(&format!("encode segment: {e}"));
                return;
            }
        };
        let sh = &mut st.ship;
        let g = sh.generation.as_mut().expect("generation checked above");
        let mut seq = first_seq;
        for (bytes, frames) in encoded {
            g.next_frame += frames;
            sh.pending_bytes += bytes.len();
            sh.pending.push_back(Pending {
                generation: id.clone(),
                seq,
                bytes,
            });
            seq += 1;
        }
        g.next_seq = seq;
        if sh.pending_bytes > MAX_PENDING_BYTES {
            sh.drop_pending_of(&id);
            sh.mark_broken("unshipped segments exceeded the memory bound");
        }
    }

    /// Copies the just-checkpointed database file as the base of a new
    /// generation. The file cannot change until the next checkpoint, which only
    /// this task runs, under `sync_mu`.
    async fn start_generation(&self, st: &mut SyncState) -> Result<()> {
        let id = new_generation_id(self.fence.load(Ordering::SeqCst));
        let work_dir = self.work_dir();
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|e| Error::io("create objstore dir", e))?;
        let raw = work_dir.join(format!("{BASE_FILE_PREFIX}{id}.db"));
        let db = self.store.path().to_path_buf();
        let compression = self.compression;
        let made =
            tokio::task::spawn_blocking(move || -> std::io::Result<(PathBuf, u64, u64, String)> {
                remove_stale_bases(&work_dir);
                let packed = raw.with_extension("db.zst");
                let made = (|| {
                    std::fs::copy(&db, &raw)?;
                    let raw_size = std::fs::metadata(&raw)?.len();
                    let (path, size) = match compression {
                        Compression::None => (raw.clone(), raw_size),
                        Compression::Zstd => {
                            let size = snapcodec::compress(&raw, &packed)?;
                            std::fs::remove_file(&raw)?;
                            (packed.clone(), size)
                        }
                    };
                    let sha = sha256_file(&path)?;
                    Ok((path, size, raw_size, sha))
                })();
                if made.is_err() {
                    let _ = std::fs::remove_file(&raw);
                    let _ = std::fs::remove_file(&packed);
                }
                made
            })
            .await
            .map_err(blocking_err("copy wal base"))?;
        let (path, size, raw, sha) = match made {
            Ok(v) => v,
            Err(e) => {
                self.wal_rotations.with_label_values(&["error"]).inc();
                st.ship.mark_broken("copy of the new base failed");
                return Err(Error::io("copy wal base", e));
            }
        };
        tracing::info!(generation = %id, bytes = size, "objstore: started wal generation");
        st.ship.generation = Some(Generation {
            id,
            next_seq: 0,
            next_frame: 0,
            base: Base::Local {
                path,
                size,
                raw,
                sha,
            },
            base_bytes: size,
            segment_bytes: 0,
            segments: 0,
            broken: false,
        });
        self.wal_rotations.with_label_values(&["started"]).inc();
        Ok(())
    }

    /// Uploads pending segments in order, stopping at the first failure so
    /// the bucket never holds a later segment without an earlier one.
    /// Segments of a generation that a published base has superseded are
    /// dropped instead.
    async fn upload_pending(&self, st: &mut SyncState) -> Result<()> {
        let fence = self.fence.load(Ordering::SeqCst).to_string();
        loop {
            let sh = &mut st.ship;
            let Some(p) = sh.pending.front() else {
                break;
            };
            let current = sh.generation.as_ref();
            let superseded =
                current.is_some_and(|g| g.id != p.generation && matches!(g.base, Base::Published));
            if superseded {
                let p = sh.pending.pop_front().expect("front exists");
                sh.pending_bytes -= p.bytes.len();
                continue;
            }
            let key = self.segment_key(&p.generation, p.seq);
            let put = PutObjectInput {
                bucket: self.bucket.clone(),
                key: key.clone(),
                body: PutBody::Bytes(p.bytes.clone()),
                content_length: p.bytes.len() as i64,
                metadata: HashMap::from([(super::FENCE_META_KEY.to_owned(), fence.clone())]),
                if_match: None,
                if_none_match: Some("*".to_owned()),
            };
            match self.api.put_object(put).await {
                Ok(_) => {}
                Err(e) if e.is_precondition_failed() => {
                    if !self.same_object(&key, &p.bytes).await {
                        let generation = p.generation.clone();
                        self.wal_segments.with_label_values(&["conflict"]).inc();
                        if sh.generation.as_ref().is_some_and(|g| g.id == generation) {
                            sh.mark_broken("another writer holds a segment key");
                        }
                        sh.drop_pending_of(&generation);
                        self.wal_pending_bytes.set(sh.pending_bytes as f64);
                        return Err(Error::Fencing(format!(
                            "fencing: wal segment {key} was written by another leader"
                        )));
                    }
                }
                Err(e) => {
                    self.wal_segments.with_label_values(&["error"]).inc();
                    self.wal_pending_bytes.set(sh.pending_bytes as f64);
                    return Err(e.context("upload wal segment"));
                }
            }
            let p = sh.pending.pop_front().expect("front exists");
            sh.pending_bytes -= p.bytes.len();
            if let Some(g) = sh.generation.as_mut()
                && g.id == p.generation
            {
                g.segment_bytes = g.segment_bytes.saturating_add(p.bytes.len() as u64);
                g.segments += 1;
            }
            self.wal_segments.with_label_values(&["shipped"]).inc();
        }
        self.wal_pending_bytes.set(st.ship.pending_bytes as f64);
        Ok(())
    }

    /// Whether `key` already holds exactly `bytes`: a retried PUT whose first
    /// attempt landed but whose response was lost.
    async fn same_object(&self, key: &str, bytes: &[u8]) -> bool {
        let Ok(out) = self.api.get_object(&self.bucket, key).await else {
            return false;
        };
        let mut body = out.body;
        let mut got = Vec::with_capacity(bytes.len());
        body.read_to_end(&mut got).await.is_ok() && got == bytes
    }

    /// Puts an unpublished base at the snapshot key, which makes its
    /// generation current, then drops what the previous generation left.
    async fn publish_base(&self, st: &mut SyncState) -> Result<()> {
        let Some(g) = st.ship.generation.as_mut() else {
            return Ok(());
        };
        let Base::Local {
            path,
            size,
            raw,
            sha,
        } = &g.base
        else {
            return Ok(());
        };
        let metadata = HashMap::from([
            (GENERATION_META_KEY.to_owned(), g.id.clone()),
            (SHA256_META_KEY.to_owned(), sha.clone()),
        ]);
        if let Err(e) = self.put_fenced(path.clone(), *size as i64, metadata).await {
            self.wal_rotations.with_label_values(&["error"]).inc();
            self.uploads.with_label_values(&["error"]).inc();
            return Err(e.context("publish wal base"));
        }
        let _ = tokio::fs::remove_file(path).await;
        self.snapshot_bytes.set(*size as f64);
        self.database_bytes.set(*raw as f64);
        g.base = Base::Published;
        let id = g.id.clone();
        st.ship.drop_pending_except(&id);
        self.wal_pending_bytes.set(st.ship.pending_bytes as f64);
        self.wal_rotations.with_label_values(&["published"]).inc();
        self.uploads.with_label_values(&["ok"]).inc();
        tracing::info!(generation = %id, "objstore: published wal base");
        self.delete_generations_except(Some(&id)).await;
        Ok(())
    }

    /// Deletes every segment outside generation `keep` (all of them for
    /// `None`). Best effort: leftovers are harmless, since readers only follow
    /// the generation the base names, and the next attempt retries them.
    pub(super) async fn delete_generations_except(&self, keep: Option<&str>) {
        let prefix = self.wal_prefix();
        let keys = match self.api.list_keys(&self.bucket, &prefix).await {
            Ok(keys) => keys,
            Err(e) => {
                tracing::warn!(err = %e, "objstore: list wal segments for cleanup failed");
                return;
            }
        };
        let keep = keep.map(|g| self.generation_prefix(g));
        let mut deleted = 0usize;
        for key in keys {
            if keep.as_ref().is_some_and(|k| key.starts_with(k.as_str())) {
                continue;
            }
            match self.api.delete_object(&self.bucket, &key).await {
                Ok(()) => deleted += 1,
                Err(e) => {
                    tracing::warn!(key, err = %e, "objstore: delete stale wal segment failed");
                    return;
                }
            }
        }
        if deleted > 0 {
            tracing::info!(deleted, "objstore: deleted stale wal segments");
        }
    }

    /// Brings the standby's copy of `generation` up to date and returns its
    /// path: a fresh base when the generation or base object changed, then
    /// every contiguous segment not yet applied. With `verify` (boot and
    /// promotion) the copy must be complete and current: a missing segment, or
    /// a base replaced while catching up, fails instead of serving a partial
    /// state, and a copy changed since its last check is integrity-checked.
    pub(super) async fn follow_generation(
        &self,
        st: &mut SyncState,
        head: &HeadObjectOutput,
        generation: &str,
        verify: bool,
    ) -> Result<PathBuf> {
        let reuse = match &st.replica {
            Some(r) => {
                r.generation == generation
                    && r.base_etag.is_some()
                    && r.base_etag == head.e_tag
                    && tokio::fs::metadata(&r.path).await.is_ok()
            }
            None => false,
        };
        if reuse {
            self.downloads.with_label_values(&["unchanged"]).inc();
        } else {
            st.replica = None;
            let path = self.work_dir().join("replica.db");
            let (found, size, etag) = match self.download(&path, Some(head)).await {
                Ok(v) => v,
                Err(e) => {
                    self.downloads.with_label_values(&["error"]).inc();
                    return Err(e.context("download wal base"));
                }
            };
            if !found {
                return Err(Error::Unavailable(
                    "snapshot vanished during download".into(),
                ));
            }
            self.snapshot_bytes.set(size as f64);
            let check = path.clone();
            tokio::task::spawn_blocking(move || verify_db(&check))
                .await
                .map_err(blocking_err("verify wal base"))?
                .map_err(|e| Error::ObjectStore(format!("wal base failed verification: {e}")))?;
            self.downloads.with_label_values(&["ok"]).inc();
            st.replica = Some(Replica {
                generation: generation.to_owned(),
                base_etag: etag,
                path,
                next_seq: 0,
                next_frame: 0,
                dirty: false,
            });
        }
        let applied = self.apply_segments(st).await;
        if verify && matches!(applied, Ok(true)) {
            return Err(Error::Unavailable(format!(
                "wal generation {generation} is missing a segment; refusing a partial restore"
            )));
        }
        if verify {
            let now = self
                .api
                .head_object(&self.bucket, &self.key)
                .await
                .map_err(|e| e.context("recheck snapshot"))?;
            if now.e_tag != head.e_tag {
                return Err(Error::Unavailable(
                    "snapshot replaced while catching up; retrying".into(),
                ));
            }
        }
        let replica = st.replica.as_mut().ok_or_else(|| {
            Error::ObjectStore("standby copy discarded after a failed replay".into())
        })?;
        applied?;
        if verify && replica.dirty {
            let check = replica.path.clone();
            tokio::task::spawn_blocking(move || verify_db(&check))
                .await
                .map_err(blocking_err("verify wal replica"))?
                .map_err(|e| {
                    Error::ObjectStore(format!("replayed database failed verification: {e}"))
                })?;
            replica.dirty = false;
        }
        Ok(replica.path.clone())
    }

    /// Applies the replica's next contiguous segments and reports whether a
    /// gap stopped it. A segment that fails validation leaves the copy as it
    /// was; one that fails while being written discards the copy, since it is
    /// then neither the old state nor the new.
    async fn apply_segments(&self, st: &mut SyncState) -> Result<bool> {
        let Some(replica) = st.replica.as_mut() else {
            return Ok(false);
        };
        let prefix = self.generation_prefix(&replica.generation);
        let keys = self
            .api
            .list_keys(&self.bucket, &prefix)
            .await
            .map_err(|e| e.context("list wal segments"))?;
        for key in keys {
            let Some(seq) = segment_seq(&key, &prefix) else {
                continue;
            };
            if seq < replica.next_seq {
                continue;
            }
            if seq > replica.next_seq {
                tracing::warn!(
                    generation = %replica.generation,
                    want = replica.next_seq,
                    found = seq,
                    "objstore: wal segment missing; waiting for the next base"
                );
                return Ok(true);
            }
            let out = self
                .api
                .get_object(&self.bucket, &key)
                .await
                .map_err(|e| e.context("download wal segment"))?;
            let mut body = out.body;
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes)
                .await
                .map_err(|e| Error::io("download wal segment", e))?;
            let want_frame = replica.next_frame;
            let segment = tokio::task::spawn_blocking(move || {
                let seg = walfmt::decode_segment(&bytes)?;
                if seg.first_frame != want_frame {
                    return Err(walfmt::Error::Invalid(format!(
                        "segment starts at frame {}, want {want_frame}",
                        seg.first_frame
                    )));
                }
                Ok(seg)
            })
            .await
            .map_err(blocking_err("decode wal segment"))?;
            let segment = match segment {
                Ok(s) => s,
                Err(e) => {
                    self.wal_segments.with_label_values(&["error"]).inc();
                    return Err(Error::ObjectStore(format!("wal segment {key}: {e}")));
                }
            };
            let frames = segment.frames.len() as u64;
            let path = replica.path.clone();
            let applied =
                tokio::task::spawn_blocking(move || walfmt::apply_segment(&path, &segment))
                    .await
                    .map_err(blocking_err("apply wal segment"))?;
            if let Err(e) = applied {
                self.wal_segments.with_label_values(&["error"]).inc();
                let _ = tokio::fs::remove_file(&replica.path).await;
                st.replica = None;
                return Err(Error::ObjectStore(format!("apply wal segment {key}: {e}")));
            }
            replica.next_seq += 1;
            replica.next_frame += frames;
            replica.dirty = true;
            self.wal_segments.with_label_values(&["applied"]).inc();
        }
        Ok(false)
    }

    /// A standby with automatic checkpoints off still writes a little (startup
    /// reconciliation), so it truncates its own WAL each cycle.
    pub(super) async fn checkpoint_standby(&self) {
        if !self.store.options().manual_checkpoint {
            return;
        }
        let res = self
            .store
            .write(|conn| {
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                    .map_err(|e| meta::Error::sqlite("checkpoint wal", e))
            })
            .await;
        if let Err(e) = res {
            tracing::warn!(err = %e, "objstore: standby wal checkpoint failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use async_trait::async_trait;

    use super::*;
    use crate::objstore::metasync::{
        GetObjectOutput, MetaOptions, PutObjectOutput, SyncMode, parse_fence,
    };

    const KEY: &str = "forklift/meta/forklift.db";

    #[derive(Clone)]
    struct Obj {
        data: Vec<u8>,
        etag: String,
        meta: HashMap<String, String>,
    }

    /// An in-memory bucket that enforces conditional writes and can fail or
    /// half-fail segment uploads on demand.
    #[derive(Default)]
    struct Bucket {
        objects: parking_lot::Mutex<HashMap<String, Obj>>,
        /// Every key a PUT stored, in order.
        written: parking_lot::Mutex<Vec<String>>,
        puts: AtomicUsize,
        fail_segments: AtomicBool,
        /// Stores the next segment but reports a failure, like a response
        /// lost after the write landed.
        lose_next_response: AtomicBool,
    }

    impl Bucket {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn keys(&self, prefix: &str) -> Vec<String> {
            let mut k: Vec<String> = self
                .objects
                .lock()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect();
            k.sort();
            k
        }

        fn generation(&self) -> Option<String> {
            let objects = self.objects.lock();
            meta_value(&objects.get(KEY)?.meta, GENERATION_META_KEY)
        }

        fn segments(&self) -> Vec<String> {
            self.keys("forklift/meta/wal/")
        }
    }

    #[async_trait]
    impl ObjectApi for Bucket {
        async fn put_object(&self, input: PutObjectInput) -> Result<PutObjectOutput> {
            let data = match &input.body {
                PutBody::Bytes(b) => b.clone(),
                PutBody::File(p) => {
                    std::fs::read(p).map_err(|e| Error::ObjectStore(e.to_string()))?
                }
            };
            let segment = input.key.ends_with(".seg");
            if segment && self.fail_segments.load(Ordering::SeqCst) {
                return Err(Error::Unavailable("injected outage".into()));
            }
            let mut objects = self.objects.lock();
            let current = objects.get(&input.key);
            if input.if_none_match.is_some() && current.is_some() {
                return Err(Error::PreconditionFailed("exists".into()));
            }
            if let Some(want) = &input.if_match
                && current.map(|o| &o.etag) != Some(want)
            {
                return Err(Error::PreconditionFailed("etag".into()));
            }
            let n = self.puts.fetch_add(1, Ordering::SeqCst) + 1;
            self.written.lock().push(input.key.clone());
            let etag = format!("\"{n}\"");
            objects.insert(
                input.key.clone(),
                Obj {
                    data,
                    etag: etag.clone(),
                    meta: input.metadata.clone(),
                },
            );
            if segment && self.lose_next_response.swap(false, Ordering::SeqCst) {
                return Err(Error::Unavailable("injected lost response".into()));
            }
            Ok(PutObjectOutput { e_tag: Some(etag) })
        }

        async fn get_object(&self, _bucket: &str, key: &str) -> Result<GetObjectOutput> {
            let o = self
                .objects
                .lock()
                .get(key)
                .cloned()
                .ok_or(Error::NotFound)?;
            Ok(GetObjectOutput {
                content_length: Some(o.data.len() as i64),
                e_tag: Some(o.etag),
                body: Box::new(std::io::Cursor::new(o.data)),
            })
        }

        async fn head_object(&self, _bucket: &str, key: &str) -> Result<HeadObjectOutput> {
            let o = self
                .objects
                .lock()
                .get(key)
                .cloned()
                .ok_or(Error::NotFound)?;
            Ok(HeadObjectOutput {
                e_tag: Some(o.etag),
                content_length: Some(o.data.len() as i64),
                metadata: o.meta,
            })
        }

        async fn delete_object(&self, _bucket: &str, key: &str) -> Result<()> {
            self.objects.lock().remove(key);
            Ok(())
        }

        async fn list_keys(&self, _bucket: &str, prefix: &str) -> Result<Vec<String>> {
            Ok(self.keys(prefix))
        }
    }

    struct Pod {
        _dir: tempfile::TempDir,
        store: Arc<meta::Store>,
        sync: Arc<MetaSync>,
    }

    async fn pod(bucket: &Arc<Bucket>, mode: SyncMode) -> Pod {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            meta::Store::open_with(
                dir.path().join("forklift.db"),
                meta::OpenOptions {
                    manual_checkpoint: mode == SyncMode::Wal,
                },
            )
            .await
            .unwrap(),
        );
        store
            .write(|c| {
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS wal_test (id INTEGER PRIMARY KEY, v TEXT NOT NULL)",
                )
                .map_err(|e| meta::Error::sqlite("create", e))
            })
            .await
            .unwrap();
        let sync = MetaSync::new(MetaOptions {
            store: Arc::clone(&store),
            api: Arc::clone(bucket) as Arc<dyn ObjectApi>,
            bucket: "b".into(),
            key: KEY.into(),
            data_dir: dir.path().to_path_buf(),
            interval: Duration::from_secs(60),
            compression: Compression::Zstd,
            mode,
            registry: None,
        });
        Pod {
            _dir: dir,
            store,
            sync,
        }
    }

    async fn leader(bucket: &Arc<Bucket>, fence: i64) -> Pod {
        let p = pod(bucket, SyncMode::Wal).await;
        p.sync.restore_on_boot().await.unwrap();
        p.sync.promote(fence).await.unwrap();
        p
    }

    async fn insert(store: &meta::Store, from: i64, n: i64) {
        store
            .write(move |c| {
                let tx = c.transaction().map_err(|e| meta::Error::sqlite("tx", e))?;
                for i in from..from + n {
                    tx.execute(
                        "INSERT INTO wal_test(id, v) VALUES (?1, ?2)",
                        rusqlite::params![i, format!("{i}-{}", "v".repeat((i % 50) as usize))],
                    )
                    .map_err(|e| meta::Error::sqlite("insert", e))?;
                }
                tx.commit().map_err(|e| meta::Error::sqlite("commit", e))
            })
            .await
            .unwrap();
    }

    async fn rows(store: &meta::Store) -> Vec<(i64, String)> {
        store
            .read(|c| {
                let ok: String = c
                    .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                    .map_err(|e| meta::Error::sqlite("check", e))?;
                assert_eq!(ok, "ok");
                let mut stmt = c
                    .prepare("SELECT id, v FROM wal_test ORDER BY id")
                    .map_err(|e| meta::Error::sqlite("prepare", e))?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| meta::Error::sqlite("query", e))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|e| meta::Error::sqlite("read", e))
            })
            .await
            .unwrap()
    }

    /// What a freshly booted pod restores from the bucket.
    async fn restored(bucket: &Arc<Bucket>) -> Vec<(i64, String)> {
        let p = pod(bucket, SyncMode::Wal).await;
        p.sync.restore_on_boot().await.unwrap();
        rows(&p.store).await
    }

    #[tokio::test]
    async fn leader_ships_standby_follows_and_promotes() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let first = bucket
            .generation()
            .expect("base published with a generation");
        assert_eq!(bucket.segments(), Vec::<String>::new());

        insert(&a.store, 0, 300).await;
        a.sync.sync().await.unwrap();
        let base_etag = bucket.objects.lock()[KEY].etag.clone();
        insert(&a.store, 300, 300).await;
        a.sync.sync().await.unwrap();
        assert_eq!(
            bucket.objects.lock()[KEY].etag,
            base_etag,
            "shipping must not touch the base"
        );
        assert!(bucket.segments().len() >= 2);
        assert_eq!(restored(&bucket).await, rows(&a.store).await);

        let b = pod(&bucket, SyncMode::Wal).await;
        b.sync.restore_on_boot().await.unwrap();
        insert(&a.store, 600, 100).await;
        a.sync.sync().await.unwrap();
        b.sync.sync().await.unwrap();
        {
            let st = b.sync.sync_mu.lock().await;
            let r = st.replica.as_ref().expect("standby keeps a replica");
            assert_eq!(r.generation, first);
            assert!(r.next_seq >= 3);
        }
        insert(&a.store, 700, 50).await;
        a.sync.demote().await;
        b.sync.promote(2).await.unwrap();
        assert_eq!(rows(&b.store).await, rows(&a.store).await);
        assert_eq!(
            b.sync.promotions.with_label_values(&["wal"]).get(),
            1,
            "promotion went through the wal path"
        );

        b.sync.sync().await.unwrap();
        let second = bucket.generation().unwrap();
        assert_ne!(second, first, "a new term starts its own generation");
        assert!(second.starts_with("000000000002-"));
        assert!(
            bucket.segments().iter().all(|k| k.contains(&second)),
            "the previous generation's segments are deleted"
        );
        assert_eq!(parse_fence(&bucket.objects.lock()[KEY].meta), 2);
        assert_eq!(restored(&bucket).await, rows(&b.store).await);
    }

    #[tokio::test]
    async fn final_flush_ships_what_the_last_cycle_missed() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        insert(&a.store, 0, 10).await;
        a.sync.final_sync().await;
        assert_eq!(restored(&bucket).await.len(), 10);
    }

    #[tokio::test]
    async fn a_failed_upload_is_retried_in_order() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        insert(&a.store, 0, 50).await;
        a.sync.sync().await.unwrap();

        bucket.fail_segments.store(true, Ordering::SeqCst);
        insert(&a.store, 50, 50).await;
        a.sync.sync().await.unwrap_err();
        insert(&a.store, 100, 50).await;
        a.sync.sync().await.unwrap_err();
        assert!(a.sync.wal_pending_bytes.get() > 0.0);
        assert_eq!(
            restored(&bucket).await.len(),
            50,
            "the bucket keeps the last shipped state"
        );

        bucket.fail_segments.store(false, Ordering::SeqCst);
        a.sync.sync().await.unwrap();
        assert!(a.sync.wal_pending_bytes.get().abs() < f64::EPSILON);
        assert_eq!(restored(&bucket).await, rows(&a.store).await);
    }

    #[tokio::test]
    async fn a_lost_put_response_counts_as_shipped() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        bucket.lose_next_response.store(true, Ordering::SeqCst);
        insert(&a.store, 0, 20).await;
        a.sync.sync().await.unwrap_err();
        a.sync.sync().await.unwrap();
        let st = a.sync.sync_mu.lock().await;
        assert!(!st.ship.generation.as_ref().unwrap().broken);
        drop(st);
        assert_eq!(restored(&bucket).await.len(), 20);
    }

    #[tokio::test]
    async fn a_foreign_segment_breaks_the_generation_until_the_next_base() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let generation = bucket.generation().unwrap();
        bucket.objects.lock().insert(
            format!("forklift/meta/wal/{generation}/{:020}.seg", 0),
            Obj {
                data: b"someone else".to_vec(),
                etag: "\"x\"".into(),
                meta: HashMap::new(),
            },
        );
        insert(&a.store, 0, 20).await;
        let err = a.sync.sync().await.unwrap_err();
        assert!(err.to_string().contains("fencing"), "{err}");
        assert_eq!(
            a.sync.wal_segments.with_label_values(&["conflict"]).get(),
            1
        );
        a.sync.sync().await.unwrap();
        assert_ne!(bucket.generation().unwrap(), generation);
        assert_eq!(restored(&bucket).await.len(), 20);
    }

    /// A checkpoint the shipper did not run folds frames away unread. The
    /// generation must stop taking segments (a later frame on top of a missing
    /// one would corrupt every replica) and be replaced.
    #[tokio::test]
    async fn a_foreign_checkpoint_abandons_the_generation() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let generation = bucket.generation().unwrap();
        insert(&a.store, 0, 20).await;
        a.sync.sync().await.unwrap();
        let shipped = bucket.segments().len();

        insert(&a.store, 20, 20).await;
        a.store
            .write(|c| {
                c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                    .map_err(|e| meta::Error::sqlite("checkpoint", e))
            })
            .await
            .unwrap();
        insert(&a.store, 40, 20).await;
        a.sync.sync().await.unwrap();
        let old_segments = bucket
            .written
            .lock()
            .iter()
            .filter(|k| k.contains(&generation) && k.ends_with(".seg"))
            .count();
        assert_eq!(old_segments, shipped, "no segment after the gap");
        assert_ne!(
            bucket.generation().unwrap(),
            generation,
            "replaced in the same cycle"
        );
        assert_eq!(restored(&bucket).await, rows(&a.store).await);
    }

    #[tokio::test]
    async fn rotation_replaces_the_generation_and_drops_old_segments() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let first = bucket.generation().unwrap();
        for i in 0..3 {
            insert(&a.store, i * 100, 100).await;
            a.sync.sync().await.unwrap();
        }
        a.sync
            .sync_mu
            .lock()
            .await
            .ship
            .generation
            .as_mut()
            .unwrap()
            .segment_bytes = u64::MAX;
        insert(&a.store, 300, 100).await;
        a.sync.sync().await.unwrap();
        let second = bucket.generation().unwrap();
        assert_ne!(second, first);
        assert!(bucket.segments().iter().all(|k| k.contains(&second)));
        insert(&a.store, 400, 100).await;
        a.sync.sync().await.unwrap();
        assert_eq!(restored(&bucket).await, rows(&a.store).await);
        assert_eq!(
            a.sync.wal_rotations.with_label_values(&["published"]).get(),
            2
        );
    }

    #[tokio::test]
    async fn standby_stops_at_a_missing_segment() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        for i in 0..3 {
            insert(&a.store, i * 10, 10).await;
            a.sync.sync().await.unwrap();
        }
        let segments = bucket.segments();
        assert_eq!(segments.len(), 3);
        bucket.objects.lock().remove(&segments[1]);
        let b = pod(&bucket, SyncMode::Wal).await;
        b.sync.sync().await.unwrap();
        let st = b.sync.sync_mu.lock().await;
        assert_eq!(st.replica.as_ref().unwrap().next_seq, 1);
    }

    #[tokio::test]
    async fn a_corrupt_segment_fails_closed() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        insert(&a.store, 0, 10).await;
        a.sync.sync().await.unwrap();
        let b = pod(&bucket, SyncMode::Wal).await;
        b.sync.sync().await.unwrap();

        insert(&a.store, 10, 10).await;
        a.sync.sync().await.unwrap();
        let last = bucket.segments().pop().unwrap();
        bucket.objects.lock().get_mut(&last).unwrap().data[20] ^= 0xFF;
        b.sync.sync_mu.lock().await.last_standby = None;
        b.sync.sync().await.unwrap_err();
        assert_eq!(
            b.sync
                .sync_mu
                .lock()
                .await
                .replica
                .as_ref()
                .unwrap()
                .next_seq,
            1,
            "a rejected segment leaves the replica as it was"
        );
        b.sync.promote(2).await.unwrap_err();
        assert!(!b.sync.is_leader(), "promotion fails closed");
        let c = pod(&bucket, SyncMode::Wal).await;
        c.sync.restore_on_boot().await.unwrap_err();
    }

    #[tokio::test]
    async fn modes_switch_both_ways_between_terms() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        insert(&a.store, 0, 30).await;
        a.sync.sync().await.unwrap();
        a.sync.demote().await;

        let s = pod(&bucket, SyncMode::Snapshot).await;
        s.sync.promote(2).await.unwrap();
        assert_eq!(rows(&s.store).await.len(), 30);
        insert(&s.store, 30, 30).await;
        s.sync.sync().await.unwrap();
        assert_eq!(
            bucket.generation(),
            None,
            "a snapshot carries no generation"
        );
        assert!(
            bucket.segments().is_empty(),
            "snapshot mode clears wal segments"
        );
        assert_eq!(restored(&bucket).await.len(), 60);
        s.sync.demote().await;

        let w = pod(&bucket, SyncMode::Wal).await;
        w.sync.promote(3).await.unwrap();
        insert(&w.store, 60, 30).await;
        w.sync.sync().await.unwrap();
        w.sync.sync().await.unwrap();
        assert!(bucket.generation().is_some());
        assert_eq!(restored(&bucket).await, rows(&w.store).await);
        assert_eq!(rows(&w.store).await.len(), 90);
    }

    #[tokio::test]
    async fn a_superseded_leader_cannot_publish_a_base() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let b = leader(&bucket, 2).await;
        b.sync.sync().await.unwrap();
        insert(&b.store, 0, 5).await;
        b.sync.sync().await.unwrap();

        a.sync.sync_mu.lock().await.ship.mark_broken("test");
        insert(&a.store, 100, 5).await;
        let err = a.sync.sync().await.unwrap_err();
        assert!(err.to_string().contains("fencing"), "{err}");
        assert_eq!(restored(&bucket).await, rows(&b.store).await);
    }

    #[tokio::test]
    async fn fetch_metadata_rebuilds_a_generation() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        insert(&a.store, 0, 40).await;
        a.sync.sync().await.unwrap();
        let api: &dyn ObjectApi = bucket.as_ref();
        let v1 = metadata_version(api, "b", KEY).await.unwrap().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("rebuilt.db");
        let fetched = fetch_metadata(api, "b", KEY, &dst).await.unwrap().unwrap();
        assert_eq!(fetched.version, v1);
        verify_db(&dst).unwrap();
        let n: i64 = rusqlite::Connection::open(&dst)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM wal_test", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 40);

        insert(&a.store, 40, 1).await;
        a.sync.sync().await.unwrap();
        assert_ne!(metadata_version(api, "b", KEY).await.unwrap().unwrap(), v1);
        let empty = Bucket::new();
        assert_eq!(
            metadata_version(empty.as_ref(), "b", KEY).await.unwrap(),
            None
        );
        assert_eq!(
            fetch_metadata(empty.as_ref(), "b", KEY, &dst)
                .await
                .unwrap(),
            None
        );
    }

    /// One transaction larger than a segment is never held in memory: the
    /// generation is replaced and the new base carries it.
    #[tokio::test]
    async fn an_oversized_transaction_rotates_the_generation() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let first = bucket.generation().unwrap();
        insert(&a.store, 0, 10).await;
        a.sync.sync().await.unwrap();
        a.store
            .write(|c| {
                let tx = c.transaction().map_err(|e| meta::Error::sqlite("tx", e))?;
                for i in 0..48i64 {
                    let v = format!("{i}:{}", "z".repeat(512 * 1024));
                    tx.execute(
                        "INSERT INTO wal_test(id, v) VALUES (?1, ?2)",
                        rusqlite::params![1000 + i, v],
                    )
                    .map_err(|e| meta::Error::sqlite("insert", e))?;
                }
                tx.commit().map_err(|e| meta::Error::sqlite("commit", e))
            })
            .await
            .unwrap();
        a.sync.sync().await.unwrap();
        assert_ne!(
            bucket.generation().unwrap(),
            first,
            "the big transaction rides the new base"
        );
        let largest = bucket
            .segments()
            .iter()
            .map(|k| bucket.objects.lock()[k].data.len())
            .max()
            .unwrap_or(0);
        assert!(
            largest < SEGMENT_FRAME_BYTES,
            "a segment held the whole transaction"
        );
        assert_eq!(restored(&bucket).await, rows(&a.store).await);
    }

    /// A demotion whose caller gives up waiting still finishes: the final
    /// flush lands, leadership is released and the shipping state is gone.
    #[tokio::test]
    async fn a_timed_out_demotion_still_completes() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        insert(&a.store, 0, 25).await;
        let _ = tokio::time::timeout(Duration::from_micros(1), a.sync.demote()).await;
        for _ in 0..200 {
            if !a.sync.is_leader() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!a.sync.is_leader());
        let st = a.sync.sync_mu.lock().await;
        assert!(st.ship.generation.is_none() && st.ship.pending.is_empty());
        drop(st);
        assert_eq!(restored(&bucket).await.len(), 25);
    }

    /// Boot and promotion refuse a generation with a hole, rather than serve
    /// a state that silently drops the segments after it.
    #[tokio::test]
    async fn promotion_and_boot_refuse_a_gap() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        for i in 0..3 {
            insert(&a.store, i * 10, 10).await;
            a.sync.sync().await.unwrap();
        }
        let segments = bucket.segments();
        bucket.objects.lock().remove(&segments[1]);
        let b = pod(&bucket, SyncMode::Wal).await;
        let err = b.sync.promote(2).await.unwrap_err();
        assert!(err.to_string().contains("missing a segment"), "{err}");
        assert!(!b.sync.is_leader());
        let c = pod(&bucket, SyncMode::Wal).await;
        let err = c.sync.restore_on_boot().await.unwrap_err();
        assert!(err.is_transient(), "boot retries a gap: {err}");
    }

    #[tokio::test]
    async fn stale_base_copies_are_removed() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        let work = a.sync.work_dir();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("base-left-by-a-crash.db.zst"), b"old").unwrap();
        a.sync.sync().await.unwrap();
        let left: Vec<_> = std::fs::read_dir(&work)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(BASE_FILE_PREFIX))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A reader holding a snapshot blocks the rotation's TRUNCATE. The
    /// shipper keeps the generation, and when the next writer restarts the WAL
    /// with new salts it continues from the new header instead of abandoning
    /// the generation, since every frame was read before the checkpoint.
    #[tokio::test]
    async fn a_busy_rotation_and_the_restart_after_it_lose_nothing() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let first = bucket.generation().unwrap();
        insert(&a.store, 0, 20).await;
        a.sync.sync().await.unwrap();

        let reader = rusqlite::Connection::open(a.store.path()).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM wal_test;")
            .unwrap();
        insert(&a.store, 20, 20).await;
        a.sync
            .sync_mu
            .lock()
            .await
            .ship
            .generation
            .as_mut()
            .unwrap()
            .segment_bytes = u64::MAX;
        a.sync.sync().await.unwrap();
        assert_eq!(
            bucket.generation().unwrap(),
            first,
            "rotation waits for the reader"
        );
        assert!(a.sync.sync_mu.lock().await.ship.restart_ok);
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);

        a.sync
            .sync_mu
            .lock()
            .await
            .ship
            .generation
            .as_mut()
            .unwrap()
            .segment_bytes = 0;
        insert(&a.store, 40, 20).await;
        a.sync.sync().await.unwrap();
        {
            let st = a.sync.sync_mu.lock().await;
            assert!(!st.ship.generation.as_ref().unwrap().broken);
        }
        assert_eq!(bucket.generation().unwrap(), first);
        assert_eq!(restored(&bucket).await, rows(&a.store).await);
    }

    /// Writers keep committing while the leader ships, a rotation lands in
    /// the middle, and a standby follows along. Every restore, the promoted
    /// standby included, matches the leader exactly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_ship_consistently() {
        let bucket = Bucket::new();
        let a = leader(&bucket, 1).await;
        a.sync.sync().await.unwrap();
        let b = pod(&bucket, SyncMode::Wal).await;
        b.sync.restore_on_boot().await.unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let store = Arc::clone(&a.store);
            let stop = Arc::clone(&stop);
            tokio::spawn(async move {
                let mut next = 0i64;
                while !stop.load(Ordering::SeqCst) {
                    insert(&store, next, 7).await;
                    store
                        .write(move |c| {
                            c.execute(
                                "UPDATE wal_test SET v = v || '+' WHERE id % 13 = ?1",
                                [next % 13],
                            )
                            .map_err(|e| meta::Error::sqlite("update", e))?;
                            c.execute(
                                "DELETE FROM wal_test WHERE id % 17 = ?1 AND id < ?2",
                                [next % 17, next / 2],
                            )
                            .map_err(|e| meta::Error::sqlite("delete", e))
                            .map(|_| ())
                        })
                        .await
                        .unwrap();
                    next += 7;
                    tokio::task::yield_now().await;
                }
                next
            })
        };
        for round in 0..40 {
            a.sync.sync().await.unwrap();
            if round == 20 {
                a.sync
                    .sync_mu
                    .lock()
                    .await
                    .ship
                    .generation
                    .as_mut()
                    .unwrap()
                    .segment_bytes = u64::MAX;
            }
            if round % 5 == 0 {
                b.sync.sync_mu.lock().await.last_standby = None;
                b.sync.sync().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stop.store(true, Ordering::SeqCst);
        let written = writer.await.unwrap();
        assert!(written > 100, "writer barely ran: {written}");
        a.sync.demote().await;
        let want = rows(&a.store).await;
        let got = restored(&bucket).await;
        if got != want {
            let first = got.iter().zip(&want).position(|(g, w)| g != w);
            panic!(
                "restored {} rows, leader {} rows, first difference at {first:?}: got {:?} want {:?}; segments {}",
                got.len(),
                want.len(),
                first.map(|i| &got[i]),
                first.map(|i| &want[i]),
                bucket.segments().len()
            );
        }
        b.sync.promote(2).await.unwrap();
        assert_eq!(rows(&b.store).await, want);
        assert!(a.sync.wal_rotations.with_label_values(&["published"]).get() >= 2);
    }
}
