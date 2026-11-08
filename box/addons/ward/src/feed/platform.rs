//! Drops YARA rules that can only match non-Linux file formats. A rule whose
//! condition requires a PE, Mach-O, OLE, RTF, PDF, or LNK header never
//! matches an ELF binary or a script, yet still costs every agent memory.
//! Rule names that lead with a platform (`MALPEDIA_Win_...`) count too.

/// Leading bytes of the file formats that never run on a Linux node.
const FOREIGN_MAGIC: &[&[u8]] = &[
    b"MZ",
    &[0xd0, 0xcf, 0x11, 0xe0],
    b"{\\rt",
    b"%PDF",
    &[0x4c, 0x00, 0x00, 0x00],
    &[0xfe, 0xed, 0xfa, 0xce],
    &[0xfe, 0xed, 0xfa, 0xcf],
    &[0xce, 0xfa, 0xed, 0xfe],
    &[0xcf, 0xfa, 0xed, 0xfe],
];

/// Platform tokens in rule names, in the first few `_` separated words.
const FOREIGN_NAMES: &[&str] = &[
    "win",
    "windows",
    "win32",
    "win64",
    "msil",
    "dotnet",
    "osx",
    "macos",
    "mac",
    "ios",
    "apk",
    "android",
    "powershell",
    "ps1",
    "vbs",
    "vba",
    "hta",
    "lnk",
    "doc",
    "xls",
    "msi",
    "pdf",
];
const LINUX_NAMES: &[&str] = &[
    "elf", "linux", "lnx", "unix", "sh", "bash", "php", "py", "python", "js", "jar", "multi",
];

/// Whether the first platform word in `name`, after the source prefix, is
/// a non-Linux one.
fn foreign_name(name: &str) -> bool {
    name.split('_')
        .skip(1)
        .take(3)
        .map(str::to_ascii_lowercase)
        .find_map(|w| {
            if FOREIGN_NAMES.contains(&w.as_str()) {
                Some(true)
            } else if LINUX_NAMES.contains(&w.as_str()) {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(false)
}

/// The name of the rule a line declares, with whether it is private or
/// global, when the line starts a rule.
fn rule_start(line: &str) -> Option<(&str, bool)> {
    let mut rest = line;
    let mut special = false;
    while let Some(r) = rest
        .strip_prefix("private ")
        .or_else(|| rest.strip_prefix("global "))
    {
        rest = r.trim_start();
        special = true;
    }
    let name = rest.strip_prefix("rule ")?.trim_start();
    let end = name
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(name.len());
    (end > 0).then(|| (&name[..end], special))
}

/// The condition of one rule's source, up to its closing brace.
fn condition(rule: &str) -> Option<&str> {
    let start = rule.rfind("condition:")? + "condition:".len();
    let body = &rule[start..];
    let end = body
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .find(|&i| {
            body[i..]
                .lines()
                .next()
                .is_some_and(|l| l.trim_end() == "}")
        })
        .unwrap_or(body.len());
    Some(&body[..end])
}

/// `expr` split at `keyword` where it stands at nesting depth zero, outside
/// string literals.
fn split_top<'a>(expr: &'a str, keyword: &str) -> Vec<&'a str> {
    let bytes = expr.as_bytes();
    let (mut depth, mut quoted, mut escaped) = (0i32, false, false);
    let (mut parts, mut from, mut i) = (Vec::new(), 0usize, 0usize);
    while i < bytes.len() {
        let b = bytes[i];
        if quoted {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => quoted = false,
                _ => {}
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0
                    && b.is_ascii_whitespace()
                    && expr[i + 1..].starts_with(keyword)
                    && expr[i + 1 + keyword.len()..]
                        .bytes()
                        .next()
                        .is_some_and(|c| c.is_ascii_whitespace() || c == b'(') =>
                {
                    parts.push(&expr[from..i]);
                    i += 1 + keyword.len();
                    from = i;
                    continue;
                }
                _ => {}
            }
        }
        i += 1;
    }
    parts.push(&expr[from..]);
    parts
}

/// `expr` without parentheses that enclose all of it.
fn strip_parens(mut expr: &str) -> &str {
    loop {
        expr = expr.trim();
        let Some(inner) = expr.strip_prefix('(').and_then(|e| e.strip_suffix(')')) else {
            return expr;
        };
        if !balanced(inner) {
            return expr;
        }
        expr = inner;
    }
}

/// Whether every prefix of `expr` closes no more parentheses than it opens.
fn balanced(expr: &str) -> bool {
    let mut depth = 0i32;
    let mut quoted = false;
    let mut escaped = false;
    for b in expr.bytes() {
        if quoted {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => quoted = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => quoted = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn parse_number(s: &str) -> Option<u64> {
    s.strip_prefix("0x")
        .map_or_else(|| s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
}

/// Whether one comparison requires a non-Linux file format.
fn foreign_atom(atom: &str) -> bool {
    let a: String = atom
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if a == "uint32(uint32(0x3c))==0x00004550" || a == "uint32(uint32(0x3c))==0x4550" {
        return true;
    }
    if a.starts_with("pe.") || a.starts_with("dotnet.") {
        return !["==0", "==false", "!=", "<", "defined"]
            .iter()
            .any(|t| a.contains(t));
    }
    let Some((lhs, rhs)) = a.split_once("==") else {
        return false;
    };
    let Some(value) = parse_number(rhs) else {
        return false;
    };
    let header: Vec<u8> = match lhs {
        "uint16(0)" => u16::try_from(value).map(|v| v.to_le_bytes().to_vec()),
        "uint16be(0)" => u16::try_from(value).map(|v| v.to_be_bytes().to_vec()),
        "uint32(0)" => u32::try_from(value).map(|v| v.to_le_bytes().to_vec()),
        "uint32be(0)" => u32::try_from(value).map(|v| v.to_be_bytes().to_vec()),
        _ => return false,
    }
    .unwrap_or_default();
    !header.is_empty()
        && FOREIGN_MAGIC
            .iter()
            .any(|m| m.starts_with(&header) || header.starts_with(m))
}

/// Whether `expr` can only be true for a non-Linux file format.
fn foreign(expr: &str) -> bool {
    let e = strip_parens(expr);
    let any = split_top(e, "or");
    if any.len() > 1 {
        return any.iter().all(|d| foreign(d));
    }
    let all = split_top(e, "and");
    if all.len() > 1 {
        return all.iter().any(|c| foreign(c));
    }
    foreign_atom(e)
}

/// `source` without its rules for non-Linux file formats, and the names of
/// the rules it left out. Private and global rules always stay, since other
/// rules may depend on them.
pub fn linux_rules(source: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(source.len());
    let mut dropped = Vec::new();
    let mut chunk = String::new();
    let mut current: Option<(String, bool)> = None;
    let mut flush = |chunk: &mut String, current: &Option<(String, bool)>| {
        match current {
            Some((name, false)) if foreign_name(name) || condition(chunk).is_some_and(foreign) => {
                dropped.push(name.clone());
            }
            _ => out.push_str(chunk),
        }
        chunk.clear();
    };
    for line in source.split_inclusive('\n') {
        if let Some((name, special)) = rule_start(line) {
            flush(&mut chunk, &current);
            current = Some((name.to_string(), special));
        }
        chunk.push_str(line);
    }
    flush(&mut chunk, &current);
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"import "pe"

rule T_Mz_Only
{
	strings:
		$a = "evil"
	condition:
		uint16( 0 ) == 0x5A4D and filesize < 2MB and all of them
}

rule T_Mz_Or_Macho
{
	condition:
		( uint16be( 0 ) == 0x4d5a or uint32( 0 ) == 0xfeedfacf ) and filesize < 1MB
}

rule T_Elf_Or_Mz
{
	condition:
		uint32be( 0 ) == 0x7f454c46 or uint16( 0 ) == 0x5a4d
}

rule T_Pe_Module : tag1
{
	condition:
		pe.is_pe and pe.number_of_sections > 3
}

private rule T_Mz_Helper
{
	condition:
		uint16( 0 ) == 0x5a4d
}

rule T_Not_Pe
{
	condition:
		not pe.is_pe and filesize < 1MB
}

rule T_Grouped
{
	condition:
		( ( uint16( 0 ) == 0x5A4D and uint32( uint32( 0x3C ) ) == 0x00004550 ) and 2 of them ) or ( uint32( 0 ) == 0xe011cfd0 and "a or b" == "x" )
}

rule T_Elf
{
	condition:
		uint32be( 0 ) == 0x7f454c46 and filesize < 1MB
}
"#;

    #[test]
    fn keeps_rules_that_can_match_on_linux() {
        let (out, dropped) = linux_rules(SOURCE);
        assert_eq!(
            dropped,
            vec!["T_Mz_Only", "T_Mz_Or_Macho", "T_Pe_Module", "T_Grouped"]
        );
        for kept in [
            "T_Elf_Or_Mz",
            "T_Mz_Helper",
            "T_Not_Pe",
            "rule T_Elf\n",
            "import \"pe\"",
        ] {
            assert!(out.contains(kept), "{kept} missing");
        }
        let (rules, errors) = crate::content::compile_yara(&[crate::content::YaraSource {
            name: "t.yar".into(),
            source: out,
        }]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rules.unwrap().iter().len(), 4);
    }

    #[test]
    fn atoms_and_parentheses() {
        assert!(foreign_atom("uint32( 0 ) == 0x46445025"));
        assert!(foreign_atom("uint32be(0) == 0xd0cf11e0"));
        assert!(foreign_atom("uint16(0)==23117"));
        assert!(!foreign_atom("uint32be(0) == 0x7f454c46"));
        assert!(!foreign_atom("uint32(0) == 0x04034b50"));
        assert!(!foreign_atom("pe.number_of_signatures == 0"));
        assert!(!foreign_atom("uint16(0x40) == 0x5a4d"));
        assert_eq!(strip_parens(" ((a) and (b)) "), "(a) and (b)");
        assert_eq!(strip_parens("(a) and (b)"), "(a) and (b)");
        assert!(foreign("(uint16(0) == 0x5a4d)"));
        assert!(!foreign("uint16(0) == 0x5a4d and x or y"));
        assert_eq!(rule_start("global private rule G : t {"), Some(("G", true)));
        assert!(foreign_name("MALPEDIA_Win_Lazardoor_Auto"));
        assert!(foreign_name("SIGNATURE_BASE_APT_MacOS_Backdoor"));
        assert!(!foreign_name("MALPEDIA_Elf_Win_Mix"));
        assert!(!foreign_name("DITEKSHEN_MALWARE_Linux_Mirai"));
        assert!(!foreign_name("Windows_Only_Without_Prefix_Word"));
        assert_eq!(rule_start("  rule Indented"), None);
        assert!(condition("rule X { strings: $a = \"b\" }").is_none());
    }
}
