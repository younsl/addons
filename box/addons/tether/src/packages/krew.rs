//! krew plugins from install receipts, matching `kubectl krew list`.

use std::fs;
use std::io;
use std::path::Path;

/// Installed plugins, sorted. Plugins from a custom index are `index/name`.
pub fn plugins(krew_root: &Path) -> io::Result<Vec<String>> {
    let mut plugins = Vec::new();
    for entry in fs::read_dir(krew_root.join("receipts"))? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "yaml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let index = fs::read_to_string(&path)
            .ok()
            .and_then(|text| receipt_index(&text).map(str::to_string));
        plugins.push(match index.as_deref() {
            None | Some("default") => name.to_string(),
            Some(index) => format!("{index}/{name}"),
        });
    }
    plugins.sort();
    Ok(plugins)
}

/// `status.source.name` of a receipt, the index the plugin came from.
fn receipt_index(text: &str) -> Option<&str> {
    let mut in_status = false;
    let mut in_source = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            in_status = line.trim_end() == "status:";
            in_source = false;
        } else if in_status && line.trim_end() == "  source:" {
            in_source = true;
        } else if in_source {
            if let Some(name) = line.strip_prefix("    name:") {
                return Some(name.trim());
            }
            if !line.starts_with("    ") {
                in_source = false;
            }
        }
    }
    None
}

pub fn render(plugins: &[String], krew_version: &str, date: &str) -> String {
    let mut out = format!(
        "#---------------------------------\n# Backup completed on {date} \n# Krew version is {krew_version}   \n#---------------------------------\n"
    );
    for plugin in plugins {
        out.push_str(plugin);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECEIPT: &str = "apiVersion: v1alpha2\nkind: Plugin\nmetadata:\n  name: stern\nspec:\n  platforms:\n  - selector: {}\n    name: nested\nstatus:\n  source:\n    name: default\n";

    #[test]
    fn index_from_status_source() {
        assert_eq!(receipt_index(RECEIPT), Some("default"));
        assert_eq!(
            receipt_index(&RECEIPT.replace("name: default", "name: custom")),
            Some("custom")
        );
        assert_eq!(receipt_index("kind: Plugin\n"), None);
    }

    #[test]
    fn plugins_sorted_with_custom_index_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let receipts = dir.path().join("receipts");
        fs::create_dir_all(&receipts).expect("receipts");
        fs::write(receipts.join("tree.yaml"), RECEIPT).expect("tree");
        fs::write(receipts.join("stern.yaml"), RECEIPT).expect("stern");
        fs::write(
            receipts.join("foo.yaml"),
            RECEIPT.replace("name: default", "name: mine"),
        )
        .expect("foo");
        fs::write(receipts.join("notes.txt"), "x").expect("ignored");

        assert_eq!(
            plugins(dir.path()).expect("plugins"),
            vec!["mine/foo", "stern", "tree"]
        );
    }

    #[test]
    fn missing_receipts_dir_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(plugins(dir.path()).is_err());
    }

    #[test]
    fn render_keeps_existing_header_format() {
        let out = render(&["stern".into(), "tree".into()], "v0.5.0", "2026-10-11");
        assert_eq!(
            out,
            "#---------------------------------\n# Backup completed on 2026-10-11 \n# Krew version is v0.5.0   \n#---------------------------------\nstern\ntree\n"
        );
    }
}
