//! A sorted hash index with a bloom filter in front. Large feed hash sets
//! live on disk so a million hashes cost the agent about 2 MiB of
//! memory, and the bloom filter keeps clean files off the disk.

use std::fs::File;
use std::path::Path;

use crate::policy::Severity;

const RECORD: usize = 34;
const BLOOM_BITS_PER_ENTRY: usize = 10;
const BLOOM_HASHES: u64 = 7;
pub const INDEX_FILE: &str = "hashes.idx";

struct Bloom {
    bits: Vec<u64>,
    m: u64,
}

impl Bloom {
    fn new(n: usize) -> Self {
        let m = (n.max(1) * BLOOM_BITS_PER_ENTRY).next_power_of_two() as u64;
        Self {
            bits: vec![0; usize::try_from(m / 64).unwrap_or(1).max(1)],
            m,
        }
    }

    fn positions(&self, h: &[u8; 32]) -> impl Iterator<Item = u64> + '_ {
        let h1 = u64::from_le_bytes(h[0..8].try_into().unwrap_or_default());
        let h2 = u64::from_le_bytes(h[8..16].try_into().unwrap_or_default()) | 1;
        (0..BLOOM_HASHES).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) & (self.m - 1))
    }

    fn insert(&mut self, h: &[u8; 32]) {
        let pos: Vec<u64> = self.positions(h).collect();
        for p in pos {
            self.bits[usize::try_from(p / 64).unwrap_or(0)] |= 1 << (p % 64);
        }
    }

    fn contains(&self, h: &[u8; 32]) -> bool {
        self.positions(h)
            .all(|p| self.bits[usize::try_from(p / 64).unwrap_or(0)] & (1 << (p % 64)) != 0)
    }
}

enum Store {
    Memory(Vec<([u8; 32], u16)>),
    Disk { file: File },
}

pub struct HashIndex {
    store: Store,
    count: usize,
    bloom: Bloom,
    names: Vec<(String, Severity, String)>,
}

fn record(h: &[u8; 32], idx: u16) -> [u8; RECORD] {
    let mut r = [0u8; RECORD];
    r[..32].copy_from_slice(h);
    r[32..].copy_from_slice(&idx.to_le_bytes());
    r
}

impl HashIndex {
    /// Sorts `entries` and writes them under `dir`, or keeps them in memory
    /// without a directory.
    pub fn build(
        dir: Option<&Path>,
        mut entries: Vec<([u8; 32], u16)>,
        names: Vec<(String, Severity, String)>,
    ) -> std::io::Result<Self> {
        entries.sort_unstable_by_key(|e| e.0);
        entries.dedup_by(|a, b| a.0 == b.0);
        let mut bloom = Bloom::new(entries.len());
        for (h, _) in &entries {
            bloom.insert(h);
        }
        let count = entries.len();
        let store = match dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let path = dir.join(INDEX_FILE);
                let mut buf = Vec::with_capacity(count * RECORD);
                for (h, i) in &entries {
                    buf.extend_from_slice(&record(h, *i));
                }
                std::fs::write(&path, &buf)?;
                Store::Disk {
                    file: File::open(&path)?,
                }
            }
            None => Store::Memory(entries),
        };
        Ok(Self {
            store,
            count,
            bloom,
            names,
        })
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    fn read(&self, i: usize) -> Option<([u8; 32], u16)> {
        match &self.store {
            Store::Memory(v) => v.get(i).copied(),
            Store::Disk { file } => {
                let mut r = [0u8; RECORD];
                read_at(file, &mut r, (i * RECORD) as u64).ok()?;
                Some((r[..32].try_into().ok()?, u16::from_le_bytes([r[32], r[33]])))
            }
        }
    }

    pub fn lookup(&self, h: &[u8; 32]) -> Option<(String, Severity, String)> {
        if !self.bloom.contains(h) {
            return None;
        }
        let (mut lo, mut hi) = (0usize, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (key, idx) = self.read(mid)?;
            match key.cmp(h) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return self.names.get(usize::from(idx)).cloned(),
            }
        }
        None
    }

    pub fn lookup_hex(&self, hex: &str) -> Option<(String, Severity, String)> {
        if hex.len() != 64 {
            return None;
        }
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        self.lookup(&h)
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(not(unix))]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> [u8; 32] {
        sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(
            n.to_le_bytes(),
        ))
        .into()
    }

    fn names() -> Vec<(String, Severity, String)> {
        vec![
            ("A".into(), Severity::High, "a".into()),
            ("B".into(), Severity::Low, String::new()),
        ]
    }

    #[test]
    fn finds_every_entry_on_disk_and_in_memory() {
        let entries: Vec<_> = (0..5000u32)
            .map(|n| (h(n), u16::from(n % 2 == 1)))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        for index in [
            HashIndex::build(Some(dir.path()), entries.clone(), names()).unwrap(),
            HashIndex::build(None, entries, names()).unwrap(),
        ] {
            assert_eq!(index.len(), 5000);
            for n in [0u32, 1, 2500, 4999] {
                let want = if n % 2 == 1 { "B" } else { "A" };
                assert_eq!(index.lookup(&h(n)).unwrap().0, want, "{n}");
            }
            let misses = (5000..15000u32)
                .filter(|n| index.lookup(&h(*n)).is_some())
                .count();
            assert_eq!(misses, 0);
            let hex: String = h(7).iter().fold(String::new(), |mut s, b| {
                use std::fmt::Write as _;
                let _ = write!(s, "{b:02x}");
                s
            });
            assert_eq!(index.lookup_hex(&hex).unwrap().0, "B");
            assert!(index.lookup_hex("zz").is_none());
            assert!(index.lookup_hex(&"zz".repeat(32)).is_none());
        }
        assert!(dir.path().join(INDEX_FILE).exists());
    }

    #[test]
    fn bloom_rejects_most_misses() {
        let mut b = Bloom::new(10_000);
        for n in 0..10_000u32 {
            b.insert(&h(n));
        }
        assert!((0..10_000u32).all(|n| b.contains(&h(n))));
        let fp = (10_000..110_000u32).filter(|n| b.contains(&h(*n))).count();
        assert!(fp < 2_000, "false positives {fp}");
    }

    #[test]
    fn empty_index_works() {
        let index = HashIndex::build(None, vec![], vec![]).unwrap();
        assert!(index.lookup(&h(1)).is_none());
    }
}
