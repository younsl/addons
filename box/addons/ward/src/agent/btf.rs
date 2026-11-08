//! A minimal BTF reader that resolves struct member offsets from the running
//! kernel's `/sys/kernel/btf/vmlinux`. The eBPF programs read kernel structs
//! at these offsets, which the agent writes into their globals before load.

use std::collections::HashMap;

const MAGIC: u16 = 0xeb9f;
const KIND_INT: u32 = 1;
const KIND_ARRAY: u32 = 3;
const KIND_STRUCT: u32 = 4;
const KIND_UNION: u32 = 5;
const KIND_ENUM: u32 = 6;
const KIND_TYPEDEF: u32 = 8;
const KIND_VOLATILE: u32 = 9;
const KIND_CONST: u32 = 10;
const KIND_RESTRICT: u32 = 11;
const KIND_FUNC_PROTO: u32 = 13;
const KIND_VAR: u32 = 14;
const KIND_DATASEC: u32 = 15;
const KIND_DECL_TAG: u32 = 17;
const KIND_TYPE_TAG: u32 = 18;
const KIND_ENUM64: u32 = 19;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BtfError {
    #[error("not BTF data")]
    Magic,
    #[error("truncated BTF data")]
    Truncated,
}

#[derive(Clone, Debug)]
struct Member {
    name: String,
    type_id: u32,
    bit_offset: u32,
}

#[derive(Clone, Debug)]
struct Type {
    kind: u32,
    target: u32,
    members: Vec<Member>,
}

pub struct Btf {
    types: Vec<Type>,
    structs: HashMap<String, Vec<u32>>,
}

fn u16_at(b: &[u8], o: usize) -> Result<u16, BtfError> {
    Ok(u16::from_le_bytes(
        b.get(o..o + 2)
            .ok_or(BtfError::Truncated)?
            .try_into()
            .map_err(|_| BtfError::Truncated)?,
    ))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32, BtfError> {
    Ok(u32::from_le_bytes(
        b.get(o..o + 4)
            .ok_or(BtfError::Truncated)?
            .try_into()
            .map_err(|_| BtfError::Truncated)?,
    ))
}

fn str_at(strings: &[u8], off: u32) -> String {
    let start = off as usize;
    let Some(rest) = strings.get(start..) else {
        return String::new();
    };
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).into_owned()
}

impl Btf {
    /// Parses little-endian BTF, as every kernel ward targets writes it.
    pub fn parse(b: &[u8]) -> Result<Self, BtfError> {
        if u16_at(b, 0)? != MAGIC {
            return Err(BtfError::Magic);
        }
        let hdr_len = u32_at(b, 4)? as usize;
        let type_off = u32_at(b, 8)? as usize;
        let type_len = u32_at(b, 12)? as usize;
        let str_off = u32_at(b, 16)? as usize;
        let str_len = u32_at(b, 20)? as usize;
        let types_raw = b
            .get(hdr_len + type_off..hdr_len + type_off + type_len)
            .ok_or(BtfError::Truncated)?;
        let strings = b
            .get(hdr_len + str_off..hdr_len + str_off + str_len)
            .ok_or(BtfError::Truncated)?;

        let mut types = vec![Type {
            kind: 0,
            target: 0,
            members: Vec::new(),
        }];
        let mut structs: HashMap<String, Vec<u32>> = HashMap::new();
        let mut o = 0usize;
        while o < types_raw.len() {
            let name_off = u32_at(types_raw, o)?;
            let info = u32_at(types_raw, o + 4)?;
            let size_or_type = u32_at(types_raw, o + 8)?;
            o += 12;
            let vlen = (info & 0xffff) as usize;
            let kind = (info >> 24) & 0x1f;
            let kind_flag = info >> 31 == 1;
            let mut members = Vec::new();
            match kind {
                KIND_INT | KIND_VAR | KIND_DECL_TAG => o += 4,
                KIND_ARRAY => o += 12,
                KIND_STRUCT | KIND_UNION => {
                    for _ in 0..vlen {
                        let m_name = u32_at(types_raw, o)?;
                        let m_type = u32_at(types_raw, o + 4)?;
                        let m_off = u32_at(types_raw, o + 8)?;
                        members.push(Member {
                            name: str_at(strings, m_name),
                            type_id: m_type,
                            bit_offset: if kind_flag {
                                m_off & 0x00ff_ffff
                            } else {
                                m_off
                            },
                        });
                        o += 12;
                    }
                }
                KIND_ENUM | KIND_FUNC_PROTO => o += 8 * vlen,
                KIND_DATASEC | KIND_ENUM64 => o += 12 * vlen,
                _ => {}
            }
            let name = str_at(strings, name_off);
            let id = u32::try_from(types.len()).map_err(|_| BtfError::Truncated)?;
            if kind == KIND_STRUCT && !name.is_empty() {
                structs.entry(name).or_default().push(id);
            }
            types.push(Type {
                kind,
                target: size_or_type,
                members,
            });
        }
        Ok(Self { types, structs })
    }

    fn resolve(&self, mut id: u32) -> Option<&Type> {
        for _ in 0..16 {
            let t = self.types.get(id as usize)?;
            match t.kind {
                KIND_TYPEDEF | KIND_VOLATILE | KIND_CONST | KIND_RESTRICT | KIND_TYPE_TAG => {
                    id = t.target;
                }
                _ => return Some(t),
            }
        }
        None
    }

    fn member_bits(&self, t: &Type, member: &str, depth: u8) -> Option<u32> {
        if depth > 8 {
            return None;
        }
        if let Some(m) = t.members.iter().find(|m| m.name == member) {
            return Some(m.bit_offset);
        }
        t.members
            .iter()
            .filter(|m| m.name.is_empty())
            .find_map(|m| {
                let inner = self.resolve(m.type_id)?;
                matches!(inner.kind, KIND_STRUCT | KIND_UNION)
                    .then(|| self.member_bits(inner, member, depth + 1))
                    .flatten()
                    .map(|b| b + m.bit_offset)
            })
    }

    /// Byte offset of `member` in `struct name`, looking through anonymous
    /// structs and unions.
    pub fn offset(&self, name: &str, member: &str) -> Option<u32> {
        self.structs.get(name)?.iter().find_map(|id| {
            let t = self.types.get(*id as usize)?;
            self.member_bits(t, member, 0).map(|bits| bits / 8)
        })
    }

    #[cfg(test)]
    pub fn struct_count(&self) -> usize {
        self.structs.len()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds BTF bytes from `(name, kind, size_or_type, members)` entries.
    pub struct Builder {
        types: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Builder {
        pub fn new() -> Self {
            Self {
                types: Vec::new(),
                strings: vec![0],
            }
        }

        fn s(&mut self, s: &str) -> u32 {
            if s.is_empty() {
                return 0;
            }
            let off = u32::try_from(self.strings.len()).unwrap();
            self.strings.extend_from_slice(s.as_bytes());
            self.strings.push(0);
            off
        }

        pub fn int(&mut self, name: &str) {
            let n = self.s(name);
            self.types.extend_from_slice(&n.to_le_bytes());
            self.types
                .extend_from_slice(&(KIND_INT << 24).to_le_bytes());
            self.types.extend_from_slice(&8u32.to_le_bytes());
            self.types.extend_from_slice(&64u32.to_le_bytes());
        }

        pub fn composite(&mut self, kind: u32, name: &str, members: &[(&str, u32, u32)]) {
            let n = self.s(name);
            self.types.extend_from_slice(&n.to_le_bytes());
            let info = (kind << 24) | u32::try_from(members.len()).unwrap();
            self.types.extend_from_slice(&info.to_le_bytes());
            self.types.extend_from_slice(&64u32.to_le_bytes());
            for (m, t, bits) in members {
                let mn = self.s(m);
                self.types.extend_from_slice(&mn.to_le_bytes());
                self.types.extend_from_slice(&t.to_le_bytes());
                self.types.extend_from_slice(&bits.to_le_bytes());
            }
        }

        pub fn typedef(&mut self, name: &str, target: u32) {
            let n = self.s(name);
            self.types.extend_from_slice(&n.to_le_bytes());
            self.types
                .extend_from_slice(&(KIND_TYPEDEF << 24).to_le_bytes());
            self.types.extend_from_slice(&target.to_le_bytes());
        }

        pub fn enumeration(&mut self, name: &str, values: u32) {
            let n = self.s(name);
            self.types.extend_from_slice(&n.to_le_bytes());
            self.types
                .extend_from_slice(&((KIND_ENUM << 24) | values).to_le_bytes());
            self.types.extend_from_slice(&4u32.to_le_bytes());
            for _ in 0..values {
                self.types.extend_from_slice(&[0u8; 8]);
            }
        }

        pub fn finish(self) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(&MAGIC.to_le_bytes());
            b.push(1);
            b.push(0);
            b.extend_from_slice(&24u32.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&u32::try_from(self.types.len()).unwrap().to_le_bytes());
            b.extend_from_slice(&u32::try_from(self.types.len()).unwrap().to_le_bytes());
            b.extend_from_slice(&u32::try_from(self.strings.len()).unwrap().to_le_bytes());
            b.extend_from_slice(&self.types);
            b.extend_from_slice(&self.strings);
            b
        }
    }

    /// A tiny kernel: ids 1 int, 2 enum, 3 anon struct, 4 `mm_struct`,
    /// 5 typedef, 6 `task_struct`.
    pub fn kernel() -> Vec<u8> {
        let mut b = Builder::new();
        b.int("long");
        b.enumeration("state", 2);
        b.composite(
            KIND_STRUCT,
            "",
            &[("arg_start", 1, 64), ("arg_end", 1, 128)],
        );
        b.composite(KIND_STRUCT, "mm_struct", &[("mmap", 1, 0), ("", 3, 256)]);
        b.typedef("mm_t", 4);
        b.composite(
            KIND_STRUCT,
            "task_struct",
            &[("state", 2, 0), ("tgid", 1, 8 * 1240), ("mm", 5, 8 * 900)],
        );
        b.finish()
    }

    #[test]
    fn resolves_members_and_anonymous_structs() {
        let btf = Btf::parse(&kernel()).unwrap();
        assert_eq!(btf.struct_count(), 2);
        assert_eq!(btf.offset("task_struct", "tgid"), Some(1240));
        assert_eq!(btf.offset("task_struct", "mm"), Some(900));
        assert_eq!(btf.offset("mm_struct", "arg_start"), Some(40));
        assert_eq!(btf.offset("mm_struct", "arg_end"), Some(48));
        assert_eq!(btf.offset("mm_struct", "nope"), None);
        assert_eq!(btf.offset("nope", "x"), None);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Btf::parse(b"x").err(), Some(BtfError::Truncated));
        assert_eq!(Btf::parse(&[0u8; 24]).err(), Some(BtfError::Magic));
        let mut k = kernel();
        k.truncate(40);
        assert!(Btf::parse(&k).is_err());
    }
}
