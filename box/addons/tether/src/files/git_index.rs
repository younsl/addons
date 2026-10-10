//! Paths listed in a git index file, versions 2 to 4.

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Git's offset varint, used by index version 4 for path prefix lengths.
fn offset_varint(data: &[u8]) -> Option<(usize, usize)> {
    let mut byte = *data.first()?;
    let mut value = usize::from(byte & 0x7f);
    let mut read = 1;
    while byte & 0x80 != 0 {
        byte = *data.get(read)?;
        read += 1;
        value = ((value + 1) << 7) + usize::from(byte & 0x7f);
    }
    Some((value, read))
}

/// Repository-relative paths of every index entry, or `None` when the file
/// is not an index this reader understands.
pub fn tracked(index: &[u8]) -> Option<Vec<String>> {
    if index.get(0..4)? != b"DIRC" {
        return None;
    }
    let version = u32_at(index, 4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = usize::try_from(u32_at(index, 8)?).ok()?;
    let mut at = 12;
    let mut previous: Vec<u8> = Vec::new();
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let start = at;
        at += 62;
        let flags = u16::from_be_bytes(index.get(at - 2..at)?.try_into().ok()?);
        if version >= 3 && flags & 0x4000 != 0 {
            at += 2;
        }
        let path = if version == 4 {
            let (strip, read) = offset_varint(index.get(at..)?)?;
            at += read;
            let end = at + index.get(at..)?.iter().position(|b| *b == 0)?;
            let keep = previous.len().checked_sub(strip)?;
            previous.truncate(keep);
            previous.extend_from_slice(&index[at..end]);
            at = end + 1;
            previous.clone()
        } else {
            let end = at + index.get(at..)?.iter().position(|b| *b == 0)?;
            let path = index[at..end].to_vec();
            at = start + ((end - start + 8) & !7);
            path
        };
        paths.push(String::from_utf8(path).ok()?);
    }
    Some(paths)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn header(version: u32, count: u32) -> Vec<u8> {
        let mut out = b"DIRC".to_vec();
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out
    }

    fn fixed(path_len: usize) -> Vec<u8> {
        let mut entry = vec![0_u8; 60];
        let flags = u16::try_from(path_len.min(0xfff)).expect("flags");
        entry.extend_from_slice(&flags.to_be_bytes());
        entry
    }

    /// A version 2 index listing `paths`, as git writes it.
    pub fn index_v2(paths: &[&str]) -> Vec<u8> {
        let mut out = header(2, u32::try_from(paths.len()).expect("count"));
        for path in paths {
            let mut entry = fixed(path.len());
            entry.extend_from_slice(path.as_bytes());
            let padded = (entry.len() + 8) & !7;
            entry.resize(padded, 0);
            out.extend(entry);
        }
        out.extend_from_slice(&[0_u8; 20]);
        out
    }

    #[test]
    fn reads_version_2() {
        let paths = ["a", "configs/git/config", "configs/zsh/.zshrc"];
        assert_eq!(
            tracked(&index_v2(&paths)).expect("index"),
            paths.map(String::from).to_vec()
        );
    }

    #[test]
    fn reads_version_3_extended_flags() {
        let mut out = header(3, 1);
        let mut entry = vec![0_u8; 60];
        entry.extend_from_slice(&(0x4000_u16 | 3).to_be_bytes());
        entry.extend_from_slice(&[0, 0]);
        entry.extend_from_slice(b"abc");
        let padded = (entry.len() + 8) & !7;
        entry.resize(padded, 0);
        out.extend(entry);
        assert_eq!(tracked(&out).expect("index"), vec!["abc"]);
    }

    #[test]
    fn reads_version_4_prefix_compression() {
        let mut out = header(4, 2);
        for (strip, suffix) in [(0_u8, "configs/git/config"), (6, "ignore")] {
            out.extend(fixed(0));
            out.push(strip);
            out.extend_from_slice(suffix.as_bytes());
            out.push(0);
        }
        assert_eq!(
            tracked(&out).expect("index"),
            vec!["configs/git/config", "configs/git/ignore"]
        );
    }

    #[test]
    fn offset_varint_matches_git() {
        assert_eq!(offset_varint(&[0x05]), Some((5, 1)));
        assert_eq!(offset_varint(&[0x80, 0x00]), Some((128, 2)));
        assert_eq!(offset_varint(&[0x80]), None);
    }

    #[test]
    fn rejects_other_files() {
        assert_eq!(tracked(b"not an index"), None);
        assert_eq!(tracked(&header(5, 0)), None);
        let mut truncated = index_v2(&["abc"]);
        truncated.truncate(30);
        assert_eq!(tracked(&truncated), None);
    }
}
