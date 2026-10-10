//! Carries the Argo CD UI extension script inside the binary.
//!
//! Shipping the script as a `ConfigMap` works, but it lets the panel and the API
//! it talks to drift apart: a chart upgrade that forgets the `ConfigMap` leaves an
//! old script calling a contract that has moved. Embedding it means one
//! artifact carries both halves and they can only ever be the same version.
//!
//! argocd-server serves extensions from its own filesystem rather than fetching
//! them over HTTP, so the script still has to land on disk. `install-extension`
//! does that from an init container sharing argocd-server's extensions volume.

use std::path::{Path, PathBuf};

use thiserror::Error;

const SCRIPT: &str = include_str!("../assets/extension.js");

/// Substituted with the name argocd-server proxies the backend under, so the
/// script and argocd-cm cannot disagree about the path.
const NAME_PLACEHOLDER: &str = "__EXTENSION_NAME__";

/// The proxy extension name assumed when none is given.
pub const DEFAULT_NAME: &str = "promotion-gate";

/// What the script must be called on disk. Argo CD serves every `.js` file it
/// finds under its extensions directory. The name only shows up in the
/// browser's network tab.
pub const FILE_NAME: &str = "extensions-PromotionGate.js";

/// The per-extension directory Argo CD expects inside `<extensions>/resources`.
pub const DIR_NAME: &str = "extension-PromotionGate.js";

/// Why the script could not be produced.
#[derive(Debug, Error)]
pub enum ExtensionError {
    #[error("embedded extension script still contains {NAME_PLACEHOLDER}")]
    PlaceholderLeft,
    #[error("write tar: {0}")]
    Tar(#[source] std::io::Error),
    #[error("create extension directory {path}: {source}")]
    CreateDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("write extension script {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Returns the extension script with the proxy extension name applied.
pub fn script(extension_name: &str) -> Result<String, ExtensionError> {
    let name = if extension_name.trim().is_empty() {
        DEFAULT_NAME
    } else {
        extension_name
    };
    let rendered = SCRIPT.replace(NAME_PLACEHOLDER, name);
    if rendered.contains(NAME_PLACEHOLDER) {
        return Err(ExtensionError::PlaceholderLeft);
    }
    Ok(rendered)
}

/// Packs the script in the layout argocd-extension-installer unpacks.
///
/// Serving this lets argocd-server keep the upstream wiring, the standard
/// installer image pointed at an `EXTENSION_URL`, while the script still comes
/// from the same build as the API it calls. The modification time is fixed at
/// the epoch because a timestamp taken from the clock would make the archive
/// differ on every request for no reason.
pub fn tar(extension_name: &str) -> Result<Vec<u8>, ExtensionError> {
    let body = script(extension_name)?;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header
        .set_path(Path::new("resources").join(DIR_NAME).join(FILE_NAME))
        .map_err(ExtensionError::Tar)?;
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append(&header, body.as_bytes())
        .map_err(ExtensionError::Tar)?;
    builder.into_inner().map_err(ExtensionError::Tar)
}

/// Writes the script into an Argo CD extensions directory.
///
/// The layout matches what argocd-server walks:
/// `<root>/resources/extension-PromotionGate.js/extensions-PromotionGate.js`.
pub fn install(root: &Path, extension_name: &str) -> Result<PathBuf, ExtensionError> {
    let body = script(extension_name)?;
    let dir = root.join("resources").join(DIR_NAME);
    std::fs::create_dir_all(&dir).map_err(|source| ExtensionError::CreateDir {
        path: dir.display().to_string(),
        source,
    })?;
    let path = dir.join(FILE_NAME);
    // World readable on purpose: argocd-server runs as a different user than
    // the init container that writes this.
    std::fs::write(&path, body).map_err(|source| ExtensionError::Write {
        path: path.display().to_string(),
        source,
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_substitutes_the_name_and_keeps_the_contract() {
        let rendered = script("my-gate").unwrap();
        assert!(!rendered.contains(NAME_PLACEHOLDER));
        assert!(rendered.contains("/extensions/my-gate/api/v1/gate"));
        assert!(
            rendered.contains("window.extensionsAPI.registerStatusPanelExtension"),
            "the registration call is the whole contract with Argo CD"
        );
        let defaulted = script("  ").unwrap();
        assert!(defaulted.contains(&format!("/extensions/{DEFAULT_NAME}/")));
    }

    #[test]
    fn tar_is_deterministic_and_carries_the_installer_layout() {
        let first = tar("promotion-gate").unwrap();
        let second = tar("promotion-gate").unwrap();
        assert_eq!(first, second);

        let mut archive = tar::Archive::new(first.as_slice());
        let mut entries = archive.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(
            entry.path().unwrap().to_str().unwrap(),
            "resources/extension-PromotionGate.js/extensions-PromotionGate.js"
        );
        assert_eq!(entry.header().mode().unwrap(), 0o644);
        assert_eq!(entry.header().mtime().unwrap(), 0);
        assert_eq!(
            entry.header().size().unwrap(),
            script("promotion-gate").unwrap().len() as u64
        );
        assert!(entries.next().is_none());
    }

    #[test]
    fn install_writes_where_argocd_server_looks() {
        let dir = tempfile::tempdir().unwrap();
        let path = install(dir.path(), "gate").unwrap();
        assert_eq!(
            path,
            dir.path().join("resources").join(DIR_NAME).join(FILE_NAME)
        );
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("/extensions/gate/"));

        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        let err = install(&blocked, "gate").unwrap_err();
        assert!(matches!(err, ExtensionError::CreateDir { .. }), "{err}");
    }
}
