//! Global npm packages under `<prefix>/lib/node_modules`, without npm itself.

use std::fs;
use std::path::Path;

fn entries(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

pub fn packages(prefix: &Path) -> Vec<String> {
    let modules = prefix.join("lib/node_modules");
    let mut names = Vec::new();
    for name in entries(&modules) {
        if name.starts_with('@') {
            names.extend(
                entries(&modules.join(&name))
                    .into_iter()
                    .map(|pkg| format!("{name}/{pkg}")),
            );
        } else if name != "npm" {
            names.push(name);
        }
    }
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_scoped_and_skips_npm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("lib/node_modules");
        for pkg in ["corepack", "npm", "Zed", "@openai", "@scope/tool", ".bin"] {
            fs::create_dir_all(modules.join(pkg)).expect("pkg");
        }
        assert_eq!(packages(dir.path()), vec!["@scope/tool", "corepack", "Zed"]);
        assert_eq!(packages(Path::new("/nonexistent")).len(), 0);
    }
}
