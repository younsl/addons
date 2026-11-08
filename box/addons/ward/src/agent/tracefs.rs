//! Tracepoint record layouts from tracefs `format` files, so the eBPF
//! programs read the fields at this kernel's offsets.

use std::collections::HashMap;
use std::path::Path;

/// Field name to byte offset, from a tracefs `format` document.
pub fn parse_format(s: &str) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for line in s.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("field:") else {
            continue;
        };
        let mut decl = "";
        let mut offset = None;
        for part in rest.split(';') {
            let part = part.trim();
            if let Some(o) = part.strip_prefix("offset:") {
                offset = o.parse().ok();
            } else if decl.is_empty() {
                decl = part;
            }
        }
        let name = decl
            .split_whitespace()
            .last()
            .unwrap_or_default()
            .split('[')
            .next()
            .unwrap_or_default();
        if let (Some(o), false) = (offset, name.is_empty()) {
            out.insert(name.to_string(), o);
        }
    }
    out
}

/// Reads `events/<category>/<event>/format` under `tracefs`.
pub fn read_format(tracefs: &Path, category: &str, event: &str) -> HashMap<String, u32> {
    std::fs::read_to_string(
        tracefs
            .join("events")
            .join(category)
            .join(event)
            .join("format"),
    )
    .map(|s| parse_format(&s))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXEC: &str = "name: sched_process_exec
ID: 300
format:
\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;
\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;

\tfield:__data_loc char[] filename;\toffset:8;\tsize:4;\tsigned:0;
\tfield:pid_t pid;\toffset:12;\tsize:4;\tsigned:1;
\tfield:__u8 daddr[4];\toffset:36;\tsize:4;\tsigned:0;
";

    #[test]
    fn parses_fields() {
        let f = parse_format(EXEC);
        assert_eq!(f["filename"], 8);
        assert_eq!(f["pid"], 12);
        assert_eq!(f["daddr"], 36);
        assert_eq!(f["common_pid"], 4);
        assert!(parse_format("junk").is_empty());
    }

    #[test]
    fn reads_from_a_tracefs_tree() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events/sched/sched_process_exec");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("format"), EXEC).unwrap();
        assert_eq!(
            read_format(dir.path(), "sched", "sched_process_exec")["pid"],
            12
        );
        assert!(read_format(dir.path(), "sock", "x").is_empty());
    }
}
