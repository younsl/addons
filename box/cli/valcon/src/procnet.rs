use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const TCP_ESTABLISHED: &str = "01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Remote {
    pub ip: IpAddr,
    pub port: u16,
}

/// True when the text holds a `/proc/net/tcp` or `/proc/net/tcp6` header.
pub fn has_table(text: &str) -> bool {
    text.contains("rem_address") || text.contains("remote_address")
}

/// Remote endpoints of ESTABLISHED sockets in `/proc/net/tcp` and `/proc/net/tcp6` output.
pub fn established_remotes(text: &str) -> impl Iterator<Item = Remote> + '_ {
    text.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        if !fields.next()?.ends_with(':') {
            return None;
        }
        let _local = fields.next()?;
        let remote = fields.next()?;
        (fields.next()? == TCP_ESTABLISHED)
            .then(|| parse_endpoint(remote))
            .flatten()
    })
}

/// ESTABLISHED sockets per remote IP for one remote port.
pub fn count_by_ip(text: &str, port: u16) -> HashMap<IpAddr, u32> {
    let mut counts = HashMap::new();
    for remote in established_remotes(text).filter(|r| r.port == port) {
        *counts.entry(remote.ip).or_default() += 1;
    }
    counts
}

fn parse_endpoint(field: &str) -> Option<Remote> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let ip = match addr.len() {
        8 => IpAddr::V4(parse_v4(addr)?),
        32 => {
            let v6 = parse_v6(addr)?;
            v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)
        }
        _ => return None,
    };
    Some(Remote { ip, port })
}

// The kernel prints each 32-bit word in host byte order. x86_64 and arm64 are little-endian.
fn parse_v4(hex: &str) -> Option<Ipv4Addr> {
    let word = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(word.to_le_bytes()))
}

fn parse_v6(hex: &str) -> Option<Ipv6Addr> {
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let word = u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok()?;
        *chunk = word.to_le_bytes();
    }
    Some(Ipv6Addr::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0A00000A:A1B2 0100000A:18EB 01 00000000:00000000 00:00000000 00000000  1000        0 1 1
   1: 0A00000A:A1B3 0200000A:18EB 01 00000000:00000000 00:00000000 00000000  1000        0 2 1
   2: 0A00000A:A1B4 0501000A:18EB 06 00000000:00000000 00:00000000 00000000  1000        0 3 1
   3: 0A00000A:A1B5 0100000A:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 4 1
   4: 0A00000A:A1B6 04030201:18EB 01 00000000:00000000 00:00000000 00000000  1000        0 5 1
";

    const TCP6: &str = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue
   0: 0000000000000000FFFF00000A00000A:B000 0000000000000000FFFF00000501000A:18EB 01 00000000:00000000
   1: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000
   2: 00000000000000000000000001000000:1F90 000080FE00000000FF0000000100000A:18EB 01 00000000:00000000
";

    #[test]
    fn decodes_little_endian_ipv4() {
        let r = parse_endpoint("0501000A:18EB").unwrap();
        assert_eq!(r.ip, "10.0.1.5".parse::<IpAddr>().unwrap());
        assert_eq!(r.port, 6379);
    }

    #[test]
    fn decodes_ipv4_mapped_and_native_ipv6() {
        let r = parse_endpoint("0000000000000000FFFF00000501000A:18EB").unwrap();
        assert_eq!(r.ip, "10.0.1.5".parse::<IpAddr>().unwrap());
        let r = parse_endpoint("000080FE00000000FF0000000100000A:18EB").unwrap();
        assert!(r.ip.is_ipv6());
    }

    #[test]
    fn rejects_malformed_endpoints() {
        assert_eq!(parse_endpoint("nocolon"), None);
        assert_eq!(parse_endpoint("0100000A:ZZZZ"), None);
        assert_eq!(parse_endpoint("0100:18EB"), None);
        assert_eq!(parse_endpoint("GGGGGGGG:18EB"), None);
    }

    #[test]
    fn skips_headers_and_non_established() {
        assert_eq!(established_remotes(TCP).count(), 4);
        assert_eq!(established_remotes(TCP6).count(), 2);
        assert_eq!(established_remotes("garbage\n0: short").count(), 0);
    }

    #[test]
    fn counts_by_remote_ip() {
        let text = format!("{TCP}{TCP6}");
        let c = count_by_ip(&text, 6379);
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(c.get(&ip("10.0.0.1")), Some(&1));
        assert_eq!(c.get(&ip("10.0.1.5")), Some(&1));
        assert_eq!(c.get(&ip("1.2.3.4")), Some(&1));
        assert_eq!(c.values().sum::<u32>(), 5);
        assert!(count_by_ip(&text, 443).contains_key(&ip("10.0.0.1")));
    }

    #[test]
    fn detects_table() {
        assert!(has_table(TCP));
        assert!(has_table(TCP6));
        assert!(!has_table("cat: not found"));
    }
}
