//! Go programs installed with `go install`, read from the build info each
//! binary embeds, the main package path that `go version -m` prints.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const MAGIC: &[u8] = b"\xff Go buildinf:";
const MACHO_64: u32 = 0xfeed_facf;
const LC_SEGMENT_64: u32 = 0x19;

/// Main package paths of the binaries in `go_bin`, in file name order.
pub fn packages(go_bin: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(go_bin)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();

    let mut out = Vec::new();
    for name in names {
        let path = go_bin.join(name);
        let executable = fs::symlink_metadata(&path)
            .is_ok_and(|m| m.file_type().is_file() && m.permissions().mode() & 0o111 != 0);
        if !executable {
            continue;
        }
        if let Some(pkg) = main_package(&path)
            && pkg != "command-line-arguments"
            && !out.contains(&pkg)
        {
            out.push(pkg);
        }
    }
    out
}

fn main_package(path: &Path) -> Option<String> {
    let data = buildinfo_bytes(path)?;
    let modinfo = parse_buildinfo(&data)?;
    modinfo
        .lines()
        .find_map(|line| line.strip_prefix("path\t").map(str::to_string))
}

/// The `__go_buildinfo` section of a Mach-O binary, or the whole file otherwise.
fn buildinfo_bytes(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut header = vec![0_u8; 64 * 1024];
    let read = file.read(&mut header).ok()?;
    header.truncate(read);
    if let Some((offset, size)) = macho_section(&header, b"__go_buildinfo") {
        let mut section = vec![0_u8; usize::try_from(size).ok()?];
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(&mut section).ok()?;
        return Some(section);
    }
    fs::read(path).ok()
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn macho_section(header: &[u8], name: &[u8]) -> Option<(u64, u64)> {
    if u32_at(header, 0)? != MACHO_64 {
        return None;
    }
    let ncmds = u32_at(header, 16)?;
    let mut at = 32_usize;
    for _ in 0..ncmds {
        let cmd = u32_at(header, at)?;
        let size = usize::try_from(u32_at(header, at + 4)?).ok()?;
        if cmd == LC_SEGMENT_64 {
            let nsects = u32_at(header, at + 64)?;
            for i in 0..usize::try_from(nsects).ok()? {
                let sect = at + 72 + i * 80;
                let sectname = header.get(sect..sect + 16)?;
                let trimmed = sectname.split(|b| *b == 0).next()?;
                if trimmed == name {
                    let size = u64_at(header, sect + 40)?;
                    let offset = u64::from(u32_at(header, sect + 48)?);
                    return Some((offset, size));
                }
            }
        }
        at += size;
    }
    None
}

fn uvarint(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0_usize;
    for (i, byte) in data.iter().enumerate().take(10) {
        value |= usize::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Module info string from Go 1.18+ build info, where strings are inline.
fn parse_buildinfo(data: &[u8]) -> Option<String> {
    let start = data.windows(MAGIC.len()).position(|w| w == MAGIC)?;
    let flags = *data.get(start + 15)?;
    if flags & 0x2 == 0 {
        return None;
    }
    let rest = data.get(start + 32..)?;
    let (version_len, n) = uvarint(rest)?;
    let rest = rest.get(n + version_len..)?;
    let (mod_len, n) = uvarint(rest)?;
    let mut modinfo = rest.get(n..n + mod_len)?;
    if modinfo.len() >= 33 && modinfo[modinfo.len() - 17] == b'\n' {
        modinfo = &modinfo[16..modinfo.len() - 16];
    }
    Some(String::from_utf8_lossy(modinfo).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = u8::try_from(n & 0x7f).expect("byte");
            n >>= 7;
            if n == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn buildinfo(path: &str) -> Vec<u8> {
        let mut data = b"junk".to_vec();
        data.extend_from_slice(MAGIC);
        data.push(8);
        data.push(0x2);
        data.resize(data.len() + 16, 0);
        let version = b"go1.25.0";
        data.extend(varint(version.len()));
        data.extend_from_slice(version);
        let mut modinfo = vec![b'S'; 16];
        modinfo.extend_from_slice(format!("path\t{path}\nmod\texample.com/m\tv1.0.0\n").as_bytes());
        modinfo.extend(vec![b'E'; 16]);
        data.extend(varint(modinfo.len()));
        data.extend(modinfo);
        data
    }

    fn macho(section: &[u8]) -> Vec<u8> {
        let mut data = vec![0_u8; 32 + 72 + 80];
        data[0..4].copy_from_slice(&MACHO_64.to_le_bytes());
        data[16..20].copy_from_slice(&1_u32.to_le_bytes());
        data[32..36].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        data[36..40].copy_from_slice(&(72_u32 + 80).to_le_bytes());
        data[96..100].copy_from_slice(&1_u32.to_le_bytes());
        let sect = 32 + 72;
        data[sect..sect + 14].copy_from_slice(b"__go_buildinfo");
        let offset = u32::try_from(data.len()).expect("offset");
        data[sect + 40..sect + 48].copy_from_slice(&(section.len() as u64).to_le_bytes());
        data[sect + 48..sect + 52].copy_from_slice(&offset.to_le_bytes());
        data.extend_from_slice(section);
        data
    }

    fn write_exec(path: &Path, data: &[u8]) {
        fs::write(path, data).expect("write");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn reads_main_package_from_raw_and_macho() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_exec(
            &dir.path().join("helm"),
            &macho(&buildinfo("helm.sh/helm/v3/cmd/helm")),
        );
        write_exec(
            &dir.path().join("jb"),
            &buildinfo("github.com/jsonnet-bundler/jsonnet-bundler/cmd/jb"),
        );
        write_exec(
            &dir.path().join("dup"),
            &buildinfo("helm.sh/helm/v3/cmd/helm"),
        );
        write_exec(
            &dir.path().join("script"),
            &buildinfo("command-line-arguments"),
        );
        write_exec(&dir.path().join("plain"), b"#!/bin/sh\n");
        fs::write(dir.path().join("noexec"), buildinfo("x/noexec")).expect("write");
        std::os::unix::fs::symlink(dir.path().join("jb"), dir.path().join("link")).expect("link");

        assert_eq!(
            packages(dir.path()),
            vec![
                "helm.sh/helm/v3/cmd/helm",
                "github.com/jsonnet-bundler/jsonnet-bundler/cmd/jb",
            ]
        );
        assert_eq!(packages(Path::new("/nonexistent")).len(), 0);
    }

    #[test]
    fn rejects_non_inline_buildinfo() {
        let mut data = MAGIC.to_vec();
        data.push(8);
        data.push(0);
        data.resize(64, 0);
        assert_eq!(parse_buildinfo(&data), None);
        assert_eq!(uvarint(&[0x80; 11]), None);
    }
}
