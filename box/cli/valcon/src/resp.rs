//! Minimal RESP2 encoder and parser, enough for pipelined read commands.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Self>>),
}

impl Value {
    pub fn text(&self) -> String {
        match self {
            Self::Simple(s) | Self::Error(s) => s.clone(),
            Self::Int(n) => n.to_string(),
            Self::Bulk(Some(b)) => String::from_utf8_lossy(b).into_owned(),
            Self::Bulk(None) | Self::Array(None) => String::new(),
            Self::Array(Some(items)) => items.iter().map(Self::text).collect::<Vec<_>>().join(" "),
        }
    }

    pub const fn int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bulk(Some(b)) => Some(b),
            _ => None,
        }
    }

    pub fn items(&self) -> &[Self] {
        match self {
            Self::Array(Some(items)) => items,
            _ => &[],
        }
    }

    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RespError {
    #[error("reply ended early")]
    Incomplete,
    #[error("invalid reply: {0}")]
    Invalid(String),
}

/// Encodes one command as a RESP array of bulk strings.
pub fn encode(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Parses every complete reply in `buf`, in order.
pub fn parse_all(buf: &[u8]) -> Result<Vec<Value>, RespError> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < buf.len() {
        let (v, next) = parse_one(buf, pos)?;
        out.push(v);
        pos = next;
    }
    Ok(out)
}

fn line(buf: &[u8], pos: usize) -> Result<(&str, usize), RespError> {
    let rest = buf.get(pos..).ok_or(RespError::Incomplete)?;
    let end = rest
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or(RespError::Incomplete)?;
    let text = std::str::from_utf8(&rest[..end]).map_err(|e| RespError::Invalid(e.to_string()))?;
    Ok((text, pos + end + 2))
}

fn number(s: &str) -> Result<i64, RespError> {
    s.parse()
        .map_err(|_| RespError::Invalid(format!("bad length {s:?}")))
}

fn parse_one(buf: &[u8], pos: usize) -> Result<(Value, usize), RespError> {
    let kind = *buf.get(pos).ok_or(RespError::Incomplete)?;
    let (head, next) = line(buf, pos + 1)?;
    match kind {
        b'+' => Ok((Value::Simple(head.to_owned()), next)),
        b'-' => Ok((Value::Error(head.to_owned()), next)),
        b':' => Ok((Value::Int(number(head)?), next)),
        b'$' => {
            let Ok(len) = usize::try_from(number(head)?) else {
                return Ok((Value::Bulk(None), next));
            };
            let end = next + len;
            let data = buf.get(next..end).ok_or(RespError::Incomplete)?;
            if buf.get(end..end + 2) != Some(b"\r\n") {
                return Err(RespError::Incomplete);
            }
            Ok((Value::Bulk(Some(data.to_vec())), end + 2))
        }
        b'*' => {
            let Ok(len) = usize::try_from(number(head)?) else {
                return Ok((Value::Array(None), next));
            };
            let mut items = Vec::with_capacity(len.min(1024));
            let mut cur = next;
            for _ in 0..len {
                let (v, n) = parse_one(buf, cur)?;
                items.push(v);
                cur = n;
            }
            Ok((Value::Array(Some(items)), cur))
        }
        other => Err(RespError::Invalid(format!(
            "unexpected type byte {other:#04x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_bulk_array() {
        assert_eq!(encode(&[b"GET", b"k"]), b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n");
    }

    #[test]
    fn parses_every_type_in_sequence() {
        let buf = b"+OK\r\n-ERR nope\r\n:42\r\n$3\r\nabc\r\n$-1\r\n*2\r\n$1\r\na\r\n:1\r\n*-1\r\n";
        let v = parse_all(buf).unwrap();
        assert_eq!(v[0], Value::Simple("OK".into()));
        assert!(v[1].is_error());
        assert_eq!(v[2].int(), Some(42));
        assert_eq!(v[3].bytes(), Some(&b"abc"[..]));
        assert_eq!(v[4], Value::Bulk(None));
        assert_eq!(v[5].items().len(), 2);
        assert_eq!(v[5].text(), "a 1");
        assert_eq!(v[6], Value::Array(None));
        assert_eq!(v[6].items().len(), 0);
        assert_eq!(v[4].text(), "");
        assert_eq!(v[1].text(), "ERR nope");
    }

    #[test]
    fn binary_bulk_with_crlf_inside() {
        let v = parse_all(b"$4\r\na\r\nb\r\n").unwrap();
        assert_eq!(v[0].bytes(), Some(&b"a\r\nb"[..]));
    }

    #[test]
    fn reports_truncation_and_garbage() {
        assert_eq!(parse_all(b"$5\r\nab"), Err(RespError::Incomplete));
        assert_eq!(parse_all(b"+OK"), Err(RespError::Incomplete));
        assert_eq!(parse_all(b"*2\r\n:1\r\n"), Err(RespError::Incomplete));
        assert!(matches!(parse_all(b"?x\r\n"), Err(RespError::Invalid(_))));
        assert!(matches!(parse_all(b":x\r\n"), Err(RespError::Invalid(_))));
        assert!(matches!(
            parse_all(b"$3\r\nabcXY"),
            Err(RespError::Incomplete)
        ));
    }
}
