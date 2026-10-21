//! SQLite WAL frames: reading the committed ones out of a live `-wal` file,
//! packing them into shippable segments, and replaying segments onto a copy
//! of the database file.
//!
//! Replay is physical. A frame is the full image of one page after a commit,
//! so writing every frame of a generation in order onto the database file as
//! it stood when the generation began reproduces the file a checkpoint would
//! produce, byte for byte. The reader trusts a frame only when its salt and
//! cumulative checksum match, exactly as SQLite's own recovery does, and only
//! up to the last commit frame, so a torn write or a rolled-back transaction
//! is never shipped.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

const WAL_HEADER_LEN: usize = 32;
const FRAME_HEADER_LEN: usize = 24;
const WAL_MAGIC_LE: u32 = 0x377f_0682;
const WAL_MAGIC_BE: u32 = 0x377f_0683;
const WAL_VERSION: u32 = 3_007_000;

const SEGMENT_MAGIC: &[u8; 8] = b"FKWALSG1";
const SEGMENT_HEADER_LEN: usize = 8 + 4 + 8 + 4;
const SEGMENT_FRAME_HEADER_LEN: usize = 8;
const SHA_LEN: usize = 32;
const SEGMENT_ZSTD_LEVEL: i32 = 3;

/// Errors from reading, decoding or replaying frames.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: io::Error,
    },
    /// A segment or database that fails validation.
    #[error("{0}")]
    Invalid(String),
}

fn io_err(op: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::Io { op, source }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One committed page image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub pgno: u32,
    /// Database size in pages after the commit this frame ends, or 0 for a
    /// frame inside a transaction.
    pub commit: u32,
    pub page: Vec<u8>,
}

/// Where the next unread frame of a WAL file starts, with the state needed to
/// validate it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    offset: u64,
    salt: [u32; 2],
    checksum: [u32; 2],
    big_endian: bool,
    page_size: u32,
}

impl Cursor {
    pub const fn page_size(&self) -> u32 {
        self.page_size
    }
}

/// The result of [`read_committed`].
#[derive(Debug)]
pub enum WalRead {
    /// Committed frames after the cursor (possibly none), the cursor past the
    /// last of them, and whether every committed frame in the file was taken.
    /// The frames may stop inside a transaction when the limit is reached:
    /// every frame before `end` is committed, so the rest of that transaction
    /// can be read next. `end` is the boundary after the last commit found,
    /// which a caller passes back as `known_end` to scan only what follows.
    Frames {
        frames: Vec<Frame>,
        cursor: Option<Cursor>,
        complete: bool,
        end: Option<Cursor>,
    },
    /// The file no longer continues the cursor, for the reason given.
    Reset(&'static str),
}

struct Header {
    salt: [u32; 2],
    checksum: [u32; 2],
    big_endian: bool,
    page_size: u32,
}

const fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// SQLite's WAL checksum: pairs of 32-bit words in the byte order the magic
/// number selects, accumulated over `data` starting from `seed`.
fn checksum(data: &[u8], big_endian: bool, seed: [u32; 2]) -> [u32; 2] {
    let [mut s0, mut s1] = seed;
    for pair in data.as_chunks::<8>().0 {
        let word = |at: usize| {
            let b = [pair[at], pair[at + 1], pair[at + 2], pair[at + 3]];
            if big_endian {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            }
        };
        s0 = s0.wrapping_add(word(0)).wrapping_add(s1);
        s1 = s1.wrapping_add(word(4)).wrapping_add(s0);
    }
    [s0, s1]
}

fn parse_header(b: &[u8; WAL_HEADER_LEN]) -> Option<Header> {
    let big_endian = match be32(b, 0) {
        WAL_MAGIC_LE => false,
        WAL_MAGIC_BE => true,
        _ => return None,
    };
    if be32(b, 4) != WAL_VERSION {
        return None;
    }
    let page_size = be32(b, 8);
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return None;
    }
    let stored = [be32(b, 24), be32(b, 28)];
    if checksum(&b[..24], big_endian, [0, 0]) != stored {
        return None;
    }
    Some(Header {
        salt: [be32(b, 16), be32(b, 20)],
        checksum: stored,
        big_endian,
        page_size,
    })
}

/// Reads the committed frames after `cursor` (or from the start of the file
/// when `None`), at most `limit` page bytes. A first pass validates the
/// frames from `known_end` (or the cursor) to find the last commit, and the
/// second reads frames up to that boundary only, so a torn write or a
/// rolled-back transaction is never returned and a large transaction can be
/// read in pieces without holding it whole. A missing or empty file, or one
/// without a valid header, holds no frames.
///
/// The caller must keep writers out while it reads if it intends to
/// checkpoint afterwards; the reader itself is safe against a concurrent
/// writer because it never trusts a frame past the last valid commit.
pub fn read_committed(
    wal: &Path,
    cursor: Option<Cursor>,
    limit: usize,
    known_end: Option<Cursor>,
) -> Result<WalRead> {
    let empty = || WalRead::Frames {
        frames: Vec::new(),
        cursor: None,
        complete: true,
        end: None,
    };
    let mut file = match File::open(wal) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(if cursor.is_some() {
                WalRead::Reset("the wal file was removed")
            } else {
                empty()
            });
        }
        Err(e) => return Err(io_err("open wal")(e)),
    };
    let len = file.metadata().map_err(io_err("stat wal"))?.len();
    let mut head = [0u8; WAL_HEADER_LEN];
    let header = if len >= WAL_HEADER_LEN as u64 {
        file.read_exact(&mut head)
            .map_err(io_err("read wal header"))?;
        parse_header(&head)
    } else {
        None
    };
    let Some(header) = header else {
        return Ok(if cursor.is_some() {
            WalRead::Reset("the wal file was truncated")
        } else {
            empty()
        });
    };
    let start = match cursor {
        Some(c) => {
            if c.salt != header.salt
                || c.page_size != header.page_size
                || c.big_endian != header.big_endian
            {
                return Ok(WalRead::Reset("the wal restarted with new salts"));
            }
            if c.offset > len {
                return Ok(WalRead::Reset("the wal file shrank below the cursor"));
            }
            c
        }
        None => Cursor {
            offset: WAL_HEADER_LEN as u64,
            salt: header.salt,
            checksum: header.checksum,
            big_endian: header.big_endian,
            page_size: header.page_size,
        },
    };
    let scan_from = match known_end {
        Some(e) if e.salt == start.salt && e.offset >= start.offset && e.offset <= len => e,
        _ => start,
    };

    let page_size = start.page_size as usize;
    let frame_len = (FRAME_HEADER_LEN + page_size) as u64;
    let mut buf = vec![0u8; FRAME_HEADER_LEN + page_size];
    let end = {
        file.seek(SeekFrom::Start(scan_from.offset))
            .map_err(io_err("seek wal"))?;
        let mut reader = io::BufReader::with_capacity(1 << 20, &mut file);
        let mut at = scan_from;
        let mut end = scan_from;
        while at.offset + frame_len <= len {
            reader
                .read_exact(&mut buf)
                .map_err(io_err("scan wal frame"))?;
            let Some(next) = next_frame(&buf, at) else {
                break;
            };
            at = next;
            if be32(&buf, 4) != 0 {
                end = at;
            }
        }
        end
    };

    file.seek(SeekFrom::Start(start.offset))
        .map_err(io_err("seek wal"))?;
    let mut reader = io::BufReader::with_capacity(1 << 20, file);
    let mut frames: Vec<Frame> = Vec::new();
    let mut at = start;
    while at.offset < end.offset && frames.len() * page_size < limit {
        reader
            .read_exact(&mut buf)
            .map_err(io_err("read wal frame"))?;
        let Some(next) = next_frame(&buf, at) else {
            return Err(Error::Invalid(
                "a committed wal frame changed while it was read".into(),
            ));
        };
        frames.push(Frame {
            pgno: be32(&buf, 0),
            commit: be32(&buf, 4),
            page: buf[FRAME_HEADER_LEN..].to_vec(),
        });
        at = next;
    }
    Ok(WalRead::Frames {
        frames,
        cursor: Some(at),
        complete: at.offset == end.offset,
        end: Some(end),
    })
}

/// Validates the frame in `buf` against the state at `at` and returns the
/// state after it, `None` when it does not continue the log.
fn next_frame(buf: &[u8], at: Cursor) -> Option<Cursor> {
    if [be32(buf, 8), be32(buf, 12)] != at.salt || be32(buf, 0) == 0 {
        return None;
    }
    let sum = checksum(&buf[..8], at.big_endian, at.checksum);
    let sum = checksum(&buf[FRAME_HEADER_LEN..], at.big_endian, sum);
    if sum != [be32(buf, 16), be32(buf, 20)] {
        return None;
    }
    Some(Cursor {
        offset: at.offset + buf.len() as u64,
        checksum: sum,
        ..at
    })
}

/// A decoded segment: the frames a generation shipped as one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub page_size: u32,
    /// Index of the first frame within its generation.
    pub first_frame: u64,
    pub frames: Vec<Frame>,
}

impl Segment {
    /// Database size in pages after the segment's last frame, when that frame
    /// ends a transaction.
    pub fn final_pages(&self) -> Option<u32> {
        self.frames.last().map(|f| f.commit).filter(|&c| c != 0)
    }

    /// Whether the segment ends on a commit. One that does not carries the
    /// first part of a transaction that later segments finish.
    pub fn ends_on_commit(&self) -> bool {
        self.final_pages().is_some()
    }
}

/// Packs `frames` into a zstd-compressed segment whose trailing SHA-256
/// covers everything before it.
pub fn encode_segment(page_size: u32, first_frame: u64, frames: &[Frame]) -> Result<Vec<u8>> {
    if frames.is_empty() {
        return Err(Error::Invalid("segment has no frames".into()));
    }
    let count = u32::try_from(frames.len())
        .map_err(|_| Error::Invalid("too many frames for one segment".into()))?;
    let mut raw = Vec::with_capacity(
        SEGMENT_HEADER_LEN
            + frames.len() * (SEGMENT_FRAME_HEADER_LEN + page_size as usize)
            + SHA_LEN,
    );
    raw.extend_from_slice(SEGMENT_MAGIC);
    raw.extend_from_slice(&page_size.to_be_bytes());
    raw.extend_from_slice(&first_frame.to_be_bytes());
    raw.extend_from_slice(&count.to_be_bytes());
    for f in frames {
        if f.page.len() != page_size as usize {
            return Err(Error::Invalid(format!(
                "frame for page {} is {} bytes, want {page_size}",
                f.pgno,
                f.page.len()
            )));
        }
        raw.extend_from_slice(&f.pgno.to_be_bytes());
        raw.extend_from_slice(&f.commit.to_be_bytes());
        raw.extend_from_slice(&f.page);
    }
    let digest = Sha256::digest(&raw);
    raw.extend_from_slice(&digest);
    zstd::bulk::compress(&raw, SEGMENT_ZSTD_LEVEL).map_err(io_err("compress segment"))
}

/// Decompresses and validates a segment written by [`encode_segment`].
pub fn decode_segment(bytes: &[u8]) -> Result<Segment> {
    let mut raw = Vec::new();
    zstd::stream::read::Decoder::new(bytes)
        .and_then(|mut d| d.read_to_end(&mut raw))
        .map_err(|e| Error::Invalid(format!("decompress segment: {e}")))?;
    if raw.len() < SEGMENT_HEADER_LEN + SHA_LEN || &raw[..8] != SEGMENT_MAGIC {
        return Err(Error::Invalid("not a forklift wal segment".into()));
    }
    let (body, sum) = raw.split_at(raw.len() - SHA_LEN);
    if Sha256::digest(body).as_slice() != sum {
        return Err(Error::Invalid("segment checksum mismatch".into()));
    }
    let page_size = be32(body, 8);
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(Error::Invalid(format!("segment page size {page_size}")));
    }
    let first_frame = u64::from_be_bytes(body[12..20].try_into().expect("8 bytes"));
    let count = be32(body, 20) as usize;
    let per = SEGMENT_FRAME_HEADER_LEN + page_size as usize;
    if body.len() != SEGMENT_HEADER_LEN + count * per {
        return Err(Error::Invalid(
            "segment length does not match its frame count".into(),
        ));
    }
    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let at = SEGMENT_HEADER_LEN + i * per;
        let pgno = be32(body, at);
        if pgno == 0 {
            return Err(Error::Invalid("segment frame for page 0".into()));
        }
        frames.push(Frame {
            pgno,
            commit: be32(body, at + 4),
            page: body[at + SEGMENT_FRAME_HEADER_LEN..at + per].to_vec(),
        });
    }
    if frames.is_empty() {
        return Err(Error::Invalid("segment has no frames".into()));
    }
    Ok(Segment {
        page_size,
        first_frame,
        frames,
    })
}

/// The page size a database file declares, `None` when the file is too short
/// to have a header (every page then comes from frames).
pub fn db_page_size(db: &Path) -> Result<Option<u32>> {
    let mut f = File::open(db).map_err(io_err("open database"))?;
    let mut head = [0u8; 100];
    match f.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(io_err("read database header")(e)),
    }
    if &head[..16] != b"SQLite format 3\0" {
        return Err(Error::Invalid("not a SQLite database".into()));
    }
    let raw = u16::from_be_bytes([head[16], head[17]]);
    Ok(Some(if raw == 1 { 65536 } else { u32::from(raw) }))
}

/// Checks that `db` is a structurally whole SQLite file: the header, a page
/// size that divides the file length, and, when the header vouches for it,
/// the page count it records. It reads 100 bytes, so it costs the same for a
/// database of any size, unlike `PRAGMA quick_check`, which reads every page.
pub fn check_db_file(db: &Path) -> Result<()> {
    let mut f = File::open(db).map_err(io_err("open database"))?;
    let len = f.metadata().map_err(io_err("stat database"))?.len();
    let mut head = [0u8; 100];
    f.read_exact(&mut head)
        .map_err(|_| Error::Invalid(format!("database is {len} bytes, too short for a header")))?;
    if &head[..16] != b"SQLite format 3\0" {
        return Err(Error::Invalid("not a SQLite database".into()));
    }
    let raw = u16::from_be_bytes([head[16], head[17]]);
    let page = if raw == 1 { 65536 } else { u64::from(raw) };
    if !(512..=65536).contains(&page) || !page.is_power_of_two() || len % page != 0 {
        return Err(Error::Invalid(format!(
            "database is {len} bytes, not whole {page}-byte pages"
        )));
    }
    let pages = u64::from(be32(&head, 28));
    let change_counter = be32(&head, 24);
    let valid_for = be32(&head, 92);
    if pages != 0 && valid_for == change_counter && pages * page != len {
        return Err(Error::Invalid(format!(
            "database header records {pages} pages, the file holds {}",
            len / page
        )));
    }
    Ok(())
}

/// Writes the frames of `segment` onto the database file at `db` in order,
/// resizing the file at every commit, then syncs it. When the segment ends a
/// transaction, the resulting length is checked against that commit.
pub fn apply_segment(db: &Path, segment: &Segment) -> Result<()> {
    if let Some(size) = db_page_size(db)?
        && size != segment.page_size
    {
        return Err(Error::Invalid(format!(
            "segment page size {} does not match the database's {size}",
            segment.page_size
        )));
    }
    let page = u64::from(segment.page_size);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(db)
        .map_err(io_err("open database"))?;
    for frame in &segment.frames {
        f.seek(SeekFrom::Start(u64::from(frame.pgno - 1) * page))
            .map_err(io_err("seek database"))?;
        f.write_all(&frame.page).map_err(io_err("write page"))?;
        if frame.commit != 0 {
            f.set_len(u64::from(frame.commit) * page)
                .map_err(io_err("resize database"))?;
        }
    }
    f.sync_all().map_err(io_err("sync database"))?;
    if let Some(pages) = segment.final_pages() {
        let want = u64::from(pages) * page;
        let got = f.metadata().map_err(io_err("stat database"))?.len();
        if got != want {
            return Err(Error::Invalid(format!(
                "database is {got} bytes after replay, want {want}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::*;

    struct Db {
        _dir: tempfile::TempDir,
        path: PathBuf,
        wal: PathBuf,
        conn: Connection,
    }

    fn open() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             PRAGMA secure_delete = ON;
             CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);
             CREATE INDEX t_v ON t(v);",
        )
        .unwrap();
        let wal = dir.path().join("live.db-wal");
        Db {
            _dir: dir,
            path,
            wal,
            conn,
        }
    }

    fn truncate(conn: &Connection) -> (i64, i64, i64) {
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
    }

    fn rows(path: &Path) -> BTreeMap<i64, String> {
        let c = Connection::open_with_flags(
            format!("file:{}?immutable=1", path.display()),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        let ok: String = c
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let mut stmt = c.prepare("SELECT id, v FROM t ORDER BY id").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Ships whatever is committed, as one or more segments, and returns the
    /// encoded segments together with the advanced cursor.
    fn ship(
        wal: &Path,
        cursor: &mut Option<Cursor>,
        next_frame: &mut u64,
        limit: usize,
    ) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut known = None;
        loop {
            let WalRead::Frames {
                frames,
                cursor: c,
                complete,
                end,
            } = read_committed(wal, *cursor, limit, known).unwrap()
            else {
                panic!("unexpected reset");
            };
            *cursor = c;
            known = end;
            if !frames.is_empty() {
                let page = c.unwrap().page_size();
                out.push(encode_segment(page, *next_frame, &frames).unwrap());
                *next_frame += frames.len() as u64;
            }
            if complete {
                return out;
            }
        }
    }

    fn replay(base: &Path, segments: &[Vec<u8>]) -> PathBuf {
        let out = base.with_extension("replica");
        std::fs::copy(base, &out).unwrap();
        let mut next = 0u64;
        for s in segments {
            let seg = decode_segment(s).unwrap();
            assert_eq!(seg.first_frame, next, "segments must be contiguous");
            next += seg.frames.len() as u64;
            apply_segment(&out, &seg).unwrap();
        }
        out
    }

    /// The core property: base plus every shipped frame equals the checkpointed
    /// source byte for byte, across inserts, updates, deletes, schema changes, a
    /// shrinking VACUUM and checkpoints in between.
    #[test]
    fn replay_reproduces_the_checkpointed_file() {
        let db = open();
        for i in 0..200 {
            db.conn
                .execute("INSERT INTO t(v) VALUES (?1)", [format!("seed-{i}")])
                .unwrap();
        }
        assert_eq!(truncate(&db.conn).0, 0);
        let base = db.path.with_extension("base");
        std::fs::copy(&db.path, &base).unwrap();

        let mut cursor = None;
        let mut next = 0u64;
        let mut segments = Vec::new();
        for round in 0..6 {
            let tx = db.conn.unchecked_transaction().unwrap();
            for i in 0..300 {
                tx.execute(
                    "INSERT INTO t(v) VALUES (?1)",
                    [format!("r{round}-{i}-{}", "x".repeat(i % 97))],
                )
                .unwrap();
            }
            tx.execute("UPDATE t SET v = v || '!' WHERE id % 7 = ?1", [round])
                .unwrap();
            tx.execute("DELETE FROM t WHERE id % 11 = ?1", [round])
                .unwrap();
            tx.commit().unwrap();
            if round == 2 {
                db.conn
                    .execute_batch(
                        "CREATE TABLE extra (k TEXT PRIMARY KEY); INSERT INTO extra VALUES ('a');",
                    )
                    .unwrap();
            }
            if round == 4 {
                db.conn
                    .execute_batch("DROP TABLE extra; DELETE FROM t WHERE id > 500; VACUUM;")
                    .unwrap();
            }
            segments.extend(ship(&db.wal, &mut cursor, &mut next, 64 * 1024));
            if round % 2 == 1 {
                assert_eq!(truncate(&db.conn).0, 0);
                cursor = None;
            }
        }
        segments.extend(ship(&db.wal, &mut cursor, &mut next, 64 * 1024));
        assert!(
            segments.len() >= 6,
            "every round ships at least one segment"
        );

        let replica = replay(&base, &segments);
        assert_eq!(truncate(&db.conn).0, 0);
        assert_eq!(
            std::fs::read(&replica).unwrap(),
            std::fs::read(&db.path).unwrap(),
            "replica differs from the checkpointed source"
        );
        assert_eq!(rows(&replica), rows(&db.path));
    }

    #[test]
    fn a_rolled_back_transaction_is_not_shipped() {
        let db = open();
        db.conn.execute_batch("PRAGMA cache_size = 2;").unwrap();
        truncate(&db.conn);
        let base = db.path.with_extension("base");
        std::fs::copy(&db.path, &base).unwrap();

        db.conn
            .execute("INSERT INTO t(v) VALUES ('kept')", [])
            .unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        for i in 0..2000 {
            tx.execute("INSERT INTO t(v) VALUES (?1)", [format!("{i:0>200}")])
                .unwrap();
        }
        let spilled = std::fs::metadata(&db.wal).unwrap().len();
        tx.rollback().unwrap();

        let mut cursor = None;
        let mut next = 0;
        let segments = ship(&db.wal, &mut cursor, &mut next, usize::MAX);
        let shipped: usize = segments
            .iter()
            .map(|s| decode_segment(s).unwrap().frames.len())
            .sum();
        assert!(
            spilled > 32 + 4 * 4120,
            "the transaction never spilled to the wal"
        );
        assert!(
            shipped < 4,
            "shipped {shipped} frames of a rolled-back transaction"
        );
        let replica = replay(&base, &segments);
        assert_eq!(
            rows(&replica).into_values().collect::<Vec<_>>(),
            vec!["kept".to_owned()]
        );
    }

    /// A transaction larger than the limit is read in pieces that end mid
    /// transaction, and the pieces replay to the same file.
    #[test]
    fn a_large_transaction_is_read_in_pieces() {
        let db = open();
        truncate(&db.conn);
        let base = db.path.with_extension("base");
        std::fs::copy(&db.path, &base).unwrap();
        db.conn
            .execute("INSERT INTO t(v) VALUES ('small')", [])
            .unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        for i in 0..400 {
            tx.execute("INSERT INTO t(v) VALUES (?1)", [format!("{i:0>500}")])
                .unwrap();
        }
        tx.commit().unwrap();

        let WalRead::Frames {
            frames,
            cursor,
            complete,
            end,
        } = read_committed(&db.wal, None, 64 * 1024, None).unwrap()
        else {
            panic!("reset");
        };
        assert!(!complete);
        assert!(frames.len() * 4096 <= 64 * 1024);
        assert!(cursor.unwrap().offset < end.unwrap().offset);

        let mut cursor = None;
        let mut next = 0;
        let segments = ship(&db.wal, &mut cursor, &mut next, 64 * 1024);
        let decoded: Vec<_> = segments
            .iter()
            .map(|s| decode_segment(s).unwrap())
            .collect();
        assert!(decoded.len() > 3);
        assert!(
            decoded.iter().any(|s| !s.ends_on_commit()),
            "some segment must end inside the large transaction"
        );
        assert!(decoded.last().unwrap().ends_on_commit());
        let replica = replay(&base, &segments);
        assert_eq!(truncate(&db.conn).0, 0);
        assert_eq!(
            std::fs::read(&replica).unwrap(),
            std::fs::read(&db.path).unwrap()
        );
    }

    /// Passing back the end boundary only rescans what follows it, and still
    /// finds commits made since.
    #[test]
    fn known_end_resumes_the_scan() {
        let db = open();
        truncate(&db.conn);
        db.conn
            .execute("INSERT INTO t(v) VALUES ('a')", [])
            .unwrap();
        let WalRead::Frames { cursor, end, .. } =
            read_committed(&db.wal, None, 4096, None).unwrap()
        else {
            panic!("reset");
        };
        db.conn
            .execute("INSERT INTO t(v) VALUES ('b')", [])
            .unwrap();
        let WalRead::Frames {
            frames, complete, ..
        } = read_committed(&db.wal, cursor, usize::MAX, end).unwrap()
        else {
            panic!("reset");
        };
        assert!(complete && !frames.is_empty());
    }

    #[test]
    fn check_db_file_catches_structural_damage() {
        let db = open();
        db.conn
            .execute("INSERT INTO t(v) VALUES ('a')", [])
            .unwrap();
        truncate(&db.conn);
        check_db_file(&db.path).unwrap();
        let bytes = std::fs::read(&db.path).unwrap();
        let short = db.path.with_extension("short");
        std::fs::write(&short, &bytes[..bytes.len() - 4096]).unwrap();
        let err = check_db_file(&short).unwrap_err().to_string();
        assert!(err.contains("records"), "{err}");
        std::fs::write(&short, &bytes[..bytes.len() - 100]).unwrap();
        assert!(
            check_db_file(&short)
                .unwrap_err()
                .to_string()
                .contains("whole")
        );
        std::fs::write(&short, b"nope").unwrap();
        assert!(check_db_file(&short).is_err());
        let mut garbage = bytes;
        garbage[0] = b'X';
        std::fs::write(&short, &garbage).unwrap();
        assert!(
            check_db_file(&short)
                .unwrap_err()
                .to_string()
                .contains("not a SQLite")
        );
    }

    #[test]
    fn a_corrupt_frame_ends_the_read() {
        let db = open();
        truncate(&db.conn);
        db.conn
            .execute("INSERT INTO t(v) VALUES ('a')", [])
            .unwrap();
        db.conn
            .execute("INSERT INTO t(v) VALUES ('b')", [])
            .unwrap();
        let WalRead::Frames { frames: all, .. } =
            read_committed(&db.wal, None, usize::MAX, None).unwrap()
        else {
            panic!("reset");
        };
        let mut bytes = std::fs::read(&db.wal).unwrap();
        let last = bytes.len() - 10;
        bytes[last] ^= 0xFF;
        std::fs::write(&db.wal, &bytes).unwrap();
        let WalRead::Frames { frames, .. } =
            read_committed(&db.wal, None, usize::MAX, None).unwrap()
        else {
            panic!("reset");
        };
        assert!(!frames.is_empty() && frames.len() < all.len());
        assert_ne!(frames.last().unwrap().commit, 0);
    }

    #[test]
    fn truncation_and_restart_read_as_reset() {
        let db = open();
        truncate(&db.conn);
        db.conn
            .execute("INSERT INTO t(v) VALUES ('a')", [])
            .unwrap();
        let WalRead::Frames { cursor, .. } =
            read_committed(&db.wal, None, usize::MAX, None).unwrap()
        else {
            panic!("reset");
        };
        assert!(matches!(
            read_committed(&db.wal, cursor, usize::MAX, None).unwrap(),
            WalRead::Frames { ref frames, .. } if frames.is_empty()
        ));
        truncate(&db.conn);
        assert!(matches!(
            read_committed(&db.wal, cursor, usize::MAX, None).unwrap(),
            WalRead::Reset(_)
        ));
        db.conn
            .execute("INSERT INTO t(v) VALUES ('b')", [])
            .unwrap();
        assert!(
            matches!(
                read_committed(&db.wal, cursor, usize::MAX, None).unwrap(),
                WalRead::Reset(_)
            ),
            "a new header must not continue the old cursor"
        );
        std::fs::remove_file(&db.wal).unwrap();
        assert!(matches!(
            read_committed(&db.wal, cursor, usize::MAX, None).unwrap(),
            WalRead::Reset(_)
        ));
        assert!(matches!(
            read_committed(&db.wal, None, usize::MAX, None).unwrap(),
            WalRead::Frames { ref frames, cursor: None, complete: true, .. } if frames.is_empty()
        ));
    }

    #[test]
    fn segment_codec_rejects_tampering() {
        let frames = vec![
            Frame {
                pgno: 2,
                commit: 0,
                page: vec![7; 512],
            },
            Frame {
                pgno: 1,
                commit: 2,
                page: vec![9; 512],
            },
        ];
        let enc = encode_segment(512, 40, &frames).unwrap();
        let seg = decode_segment(&enc).unwrap();
        assert_eq!(seg.first_frame, 40);
        assert_eq!(seg.frames, frames);
        assert_eq!(seg.final_pages(), Some(2));
        let head = decode_segment(&encode_segment(512, 40, &frames[..1]).unwrap()).unwrap();
        assert!(
            !head.ends_on_commit(),
            "a segment may stop inside a transaction"
        );

        let mut raw = zstd::bulk::decompress(&enc, 1 << 20).unwrap();
        raw[30] ^= 1;
        let tampered = zstd::bulk::compress(&raw, 1).unwrap();
        assert!(
            decode_segment(&tampered)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
        assert!(decode_segment(b"garbage").is_err());
        assert!(encode_segment(512, 0, &[]).is_err());
        assert!(
            encode_segment(1024, 0, &frames).is_err(),
            "page length must match the page size"
        );
    }

    #[test]
    fn apply_checks_page_size_and_final_length() {
        let db = open();
        truncate(&db.conn);
        let frames = vec![Frame {
            pgno: 1,
            commit: 1,
            page: vec![0; 512],
        }];
        let seg = decode_segment(&encode_segment(512, 0, &frames).unwrap()).unwrap();
        let err = apply_segment(&db.path, &seg).unwrap_err().to_string();
        assert!(err.contains("page size"), "{err}");

        let empty = db.path.with_extension("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(db_page_size(&empty).unwrap(), None);
        apply_segment(&empty, &seg).unwrap();
        assert_eq!(std::fs::metadata(&empty).unwrap().len(), 512);

        std::fs::write(&empty, vec![1u8; 200]).unwrap();
        assert!(db_page_size(&empty).is_err(), "garbage header accepted");
    }
}
