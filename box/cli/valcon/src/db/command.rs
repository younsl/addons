//! The only commands this tool sends. Every variant is read-only, and there is no way to send
//! free-form commands, so the browser cannot modify data.

use crate::resp;

const PAGE: &[u8] = b"100";
const PREVIEW_BYTES: &[u8] = b"4095";
const PREVIEW_ITEMS: &[u8] = b"99";
const STREAM_ITEMS: &[u8] = b"20";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadCommand {
    Info,
    Select(u32),
    Scan { cursor: String, pattern: String },
    Type(Vec<u8>),
    Pttl(Vec<u8>),
    MemoryUsage(Vec<u8>),
    Strlen(Vec<u8>),
    GetRange(Vec<u8>),
    Hlen(Vec<u8>),
    Hscan(Vec<u8>),
    Llen(Vec<u8>),
    Lrange(Vec<u8>),
    Scard(Vec<u8>),
    Sscan(Vec<u8>),
    Zcard(Vec<u8>),
    Zrange(Vec<u8>),
    Xlen(Vec<u8>),
    Xrange(Vec<u8>),
}

impl ReadCommand {
    pub fn encode(&self) -> Vec<u8> {
        let db;
        let args: Vec<&[u8]> = match self {
            Self::Info => vec![b"INFO"],
            Self::Select(n) => {
                db = n.to_string();
                vec![b"SELECT", db.as_bytes()]
            }
            Self::Scan { cursor, pattern } => vec![
                b"SCAN",
                cursor.as_bytes(),
                b"MATCH",
                pattern.as_bytes(),
                b"COUNT",
                PAGE,
            ],
            Self::Type(k) => vec![b"TYPE", k],
            Self::Pttl(k) => vec![b"PTTL", k],
            Self::MemoryUsage(k) => vec![b"MEMORY", b"USAGE", k],
            Self::Strlen(k) => vec![b"STRLEN", k],
            Self::GetRange(k) => vec![b"GETRANGE", k, b"0", PREVIEW_BYTES],
            Self::Hlen(k) => vec![b"HLEN", k],
            Self::Hscan(k) => vec![b"HSCAN", k, b"0", b"COUNT", PAGE],
            Self::Llen(k) => vec![b"LLEN", k],
            Self::Lrange(k) => vec![b"LRANGE", k, b"0", PREVIEW_ITEMS],
            Self::Scard(k) => vec![b"SCARD", k],
            Self::Sscan(k) => vec![b"SSCAN", k, b"0", b"COUNT", PAGE],
            Self::Zcard(k) => vec![b"ZCARD", k],
            Self::Zrange(k) => vec![b"ZRANGE", k, b"0", PREVIEW_ITEMS, b"WITHSCORES"],
            Self::Xlen(k) => vec![b"XLEN", k],
            Self::Xrange(k) => vec![b"XRANGE", k, b"-", b"+", b"COUNT", STREAM_ITEMS],
        };
        resp::encode(&args)
    }
}

/// Encodes a pipeline ending in `QUIT`, so the server closes the connection after the last
/// reply and the tunnel's stdout ends.
pub fn pipeline(commands: &[ReadCommand]) -> Vec<u8> {
    let mut out: Vec<u8> = commands.iter().flat_map(ReadCommand::encode).collect();
    out.extend(resp::encode(&[b"QUIT"]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_encodes_its_verb() {
        let k = b"k".to_vec();
        let cases = [
            (ReadCommand::Info, "INFO"),
            (ReadCommand::Select(3), "SELECT"),
            (
                ReadCommand::Scan {
                    cursor: "0".into(),
                    pattern: "*".into(),
                },
                "SCAN",
            ),
            (ReadCommand::Type(k.clone()), "TYPE"),
            (ReadCommand::Pttl(k.clone()), "PTTL"),
            (ReadCommand::MemoryUsage(k.clone()), "MEMORY"),
            (ReadCommand::Strlen(k.clone()), "STRLEN"),
            (ReadCommand::GetRange(k.clone()), "GETRANGE"),
            (ReadCommand::Hlen(k.clone()), "HLEN"),
            (ReadCommand::Hscan(k.clone()), "HSCAN"),
            (ReadCommand::Llen(k.clone()), "LLEN"),
            (ReadCommand::Lrange(k.clone()), "LRANGE"),
            (ReadCommand::Scard(k.clone()), "SCARD"),
            (ReadCommand::Sscan(k.clone()), "SSCAN"),
            (ReadCommand::Zcard(k.clone()), "ZCARD"),
            (ReadCommand::Zrange(k.clone()), "ZRANGE"),
            (ReadCommand::Xlen(k.clone()), "XLEN"),
            (ReadCommand::Xrange(k), "XRANGE"),
        ];
        for (cmd, verb) in cases {
            let parsed = resp::parse_all(&cmd.encode()).unwrap();
            assert_eq!(parsed[0].items()[0].text(), verb);
        }
    }

    #[test]
    fn pipeline_ends_with_quit() {
        let bytes = pipeline(&[ReadCommand::Select(1), ReadCommand::Info]);
        let parsed = resp::parse_all(&bytes).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[2].text(), "QUIT");
        assert_eq!(parsed[0].text(), "SELECT 1");
    }
}
