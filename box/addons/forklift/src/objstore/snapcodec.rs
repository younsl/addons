//! On-the-wire encoding of the metadata snapshot object.
//!
//! The object is either a plain SQLite file or a zstd frame holding one.
//! Readers decide by the leading magic bytes rather than by configuration, so
//! a reader accepts both regardless of how the current leader writes, and
//! turning compression on or off never strands an object already in the
//! bucket.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// The level trades CPU for size on a multi-gigabyte file: higher levels
/// barely shrink a SQLite page image further but cost several times the time.
const ZSTD_LEVEL: i32 = 3;

const BUF_SIZE: usize = 1 << 20;

/// How the leader encodes the snapshot it uploads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Compression {
    #[default]
    None,
    Zstd,
}

impl Compression {
    /// Parses the configuration value; `None` for an unknown name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "none" => Some(Self::None),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd => "zstd",
        }
    }
}

/// Reports whether the file at `path` starts with a zstd frame.
pub fn is_zstd(path: &Path) -> io::Result<bool> {
    let mut head = [0u8; 4];
    let mut f = File::open(path)?;
    let mut filled = 0;
    while filled < head.len() {
        match f.read(&mut head[filled..])? {
            0 => return Ok(false),
            n => filled += n,
        }
    }
    Ok(head == ZSTD_MAGIC)
}

/// Compresses `src` into `dst` and returns the size of `dst`.
pub fn compress(src: &Path, dst: &Path) -> io::Result<u64> {
    let input = BufReader::with_capacity(BUF_SIZE, File::open(src)?);
    let mut out = BufWriter::with_capacity(BUF_SIZE, File::create(dst)?);
    zstd::stream::copy_encode(input, &mut out, ZSTD_LEVEL)?;
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(file.metadata()?.len())
}

/// Moves the downloaded object at `raw` to `dst` as a plain SQLite file,
/// decompressing it on the way when it is a zstd frame. `raw` is gone
/// afterwards either way.
pub fn materialize(raw: &Path, dst: &Path) -> io::Result<()> {
    if !is_zstd(raw)? {
        return std::fs::rename(raw, dst);
    }
    let tmp = sibling(dst, ".decoded");
    let res = decompress(raw, &tmp).and_then(|()| std::fs::rename(&tmp, dst));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    let _ = std::fs::remove_file(raw);
    res
}

/// A plain SQLite copy of `path` in `dir` when `path` is compressed, `None`
/// when it is already plain. The copy is removed when dropped.
pub fn decoded_copy(path: &Path, dir: &Path) -> io::Result<Option<tempfile::NamedTempFile>> {
    if !is_zstd(path)? {
        return Ok(None);
    }
    let copy = tempfile::NamedTempFile::new_in(dir)?;
    decompress(path, copy.path())?;
    Ok(Some(copy))
}

fn decompress(src: &Path, dst: &Path) -> io::Result<()> {
    let input = BufReader::with_capacity(BUF_SIZE, File::open(src)?);
    let mut out = BufWriter::with_capacity(BUF_SIZE, File::create(dst)?);
    zstd::stream::copy_decode(input, &mut out)?;
    out.flush()?;
    out.into_inner()
        .map_err(io::IntoInnerError::into_error)?
        .sync_all()
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn parse_names() {
        assert_eq!(Compression::parse(""), Some(Compression::None));
        assert_eq!(Compression::parse("none"), Some(Compression::None));
        assert_eq!(Compression::parse("zstd"), Some(Compression::Zstd));
        assert_eq!(Compression::parse("gzip"), None);
        assert_eq!(Compression::Zstd.as_str(), "zstd");
        assert_eq!(Compression::None.as_str(), "none");
    }

    #[test]
    fn compress_then_materialize_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.db");
        let body: Vec<u8> = b"SQLite format 3\0"
            .iter()
            .copied()
            .chain((0..200_000u32).flat_map(|i| (i % 251).to_le_bytes()))
            .collect();
        write(&plain, &body);
        let packed = dir.path().join("packed.zst");
        let n = compress(&plain, &packed).unwrap();
        assert!(n < body.len() as u64);
        assert!(is_zstd(&packed).unwrap());
        assert!(!is_zstd(&plain).unwrap());

        let out = dir.path().join("out.db");
        materialize(&packed, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert!(!packed.exists());
    }

    #[test]
    fn materialize_moves_a_plain_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw");
        write(&raw, b"SQLite format 3\0rest");
        let out = dir.path().join("out.db");
        materialize(&raw, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"SQLite format 3\0rest");
        assert!(!raw.exists());
    }

    #[test]
    fn materialize_rejects_a_corrupt_frame_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw");
        write(&raw, &[0x28, 0xB5, 0x2F, 0xFD, 0xFF, 0xFF, 0xFF]);
        let out = dir.path().join("out.db");
        assert!(materialize(&raw, &out).is_err());
        assert!(!out.exists());
        assert!(!dir.path().join("out.db.decoded").exists());
        assert!(!raw.exists());
    }

    #[test]
    fn short_file_is_not_zstd() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("short");
        write(&p, &[0x28, 0xB5]);
        assert!(!is_zstd(&p).unwrap());
    }

    #[test]
    fn decoded_copy_only_for_compressed_input() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.db");
        write(&plain, b"SQLite format 3\0data");
        assert!(decoded_copy(&plain, dir.path()).unwrap().is_none());
        let packed = dir.path().join("packed.zst");
        compress(&plain, &packed).unwrap();
        let copy = decoded_copy(&packed, dir.path()).unwrap().unwrap();
        assert_eq!(
            std::fs::read(copy.path()).unwrap(),
            b"SQLite format 3\0data"
        );
    }
}
