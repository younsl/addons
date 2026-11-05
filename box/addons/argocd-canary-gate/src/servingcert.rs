//! Serves the webhook's TLS keypair from disk and picks up a replacement
//! without a restart.
//!
//! A listener that reads the pair once outlives the certificate it was given.
//! Nothing corrects that on its own here: the pair comes from a Secret whose
//! name never changes, so no pod spec moves and no rollout happens when
//! cert-manager re-issues. cainjector publishes the new `ca.crt` into
//! `caBundle` while the process still offers the previous leaf, and with
//! `failurePolicy: Fail` that is every gated sync in the cluster, invisible from
//! this side because the request never reaches the handler.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use thiserror::Error;
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

/// Why the keypair could not be loaded.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("load webhook certificate {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("load webhook certificate {path}: no certificate found")]
    NoCertificate { path: String },
    #[error("load webhook certificate {path}: no private key found")]
    NoKey { path: String },
    #[error("load webhook certificate {path}: {source}")]
    Tls {
        path: String,
        #[source]
        source: rustls::Error,
    },
}

/// What the loaded leaf says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeafInfo {
    pub subject: String,
    pub issuer: String,
    pub dns_names: Vec<String>,
    /// Expiry as a unix timestamp.
    pub not_after_unix: i64,
}

/// One successful load.
#[derive(Debug)]
pub struct Loaded {
    pub config: Arc<ServerConfig>,
    /// `None` when the leaf could not be parsed. Serving still works.
    pub leaf: Option<LeafInfo>,
}

/// Loads the keypair on demand and remembers the file modification times so a
/// caller can poll for a change cheaply.
pub struct Reloader {
    cert_file: PathBuf,
    key_file: PathBuf,
    seen: Mutex<Option<(SystemTime, SystemTime)>>,
}

impl Reloader {
    /// Builds a reloader over the two PEM paths.
    pub fn new(cert_file: impl Into<PathBuf>, key_file: impl Into<PathBuf>) -> Self {
        Self {
            cert_file: cert_file.into(),
            key_file: key_file.into(),
            seen: Mutex::new(None),
        }
    }

    /// The certificate path, for log lines.
    #[must_use]
    pub fn cert_file(&self) -> &Path {
        &self.cert_file
    }

    /// Reads the pair and builds a fresh server configuration.
    ///
    /// Called eagerly at startup so a broken pair fails the process before the
    /// listener opens, rather than at the first handshake.
    pub fn load(&self) -> Result<Loaded, LoadError> {
        let cert_path = self.cert_file.display().to_string();
        let certs = read_certs(&self.cert_file)?;
        let key = read_key(&self.key_file)?;

        let leaf = certs.first().and_then(|der| leaf_info(der));

        let config = ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|source| LoadError::Tls {
            path: cert_path.clone(),
            source,
        })?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|source| LoadError::Tls {
            path: cert_path,
            source,
        })?;

        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(self.mod_times());
        }

        Ok(Loaded {
            config: Arc::new(config),
            leaf,
        })
    }

    /// Reports whether either file changed since the last successful load.
    /// Called on a timer, so the common path is two stats and no parse.
    #[must_use]
    pub fn changed(&self) -> bool {
        let current = self.mod_times();
        self.seen
            .lock()
            .map_or(true, |seen| seen.as_ref() != Some(&current))
    }

    fn mod_times(&self) -> (SystemTime, SystemTime) {
        (mod_time(&self.cert_file), mod_time(&self.key_file))
    }
}

fn mod_time(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, LoadError> {
    let display = path.display().to_string();
    let file = std::fs::File::open(path).map_err(|source| LoadError::Read {
        path: display.clone(),
        source,
    })?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<_, _>>()
        .map_err(|source| LoadError::Read {
            path: display.clone(),
            source,
        })?;
    if certs.is_empty() {
        return Err(LoadError::NoCertificate { path: display });
    }
    Ok(certs)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, LoadError> {
    let display = path.display().to_string();
    let file = std::fs::File::open(path).map_err(|source| LoadError::Read {
        path: display.clone(),
        source,
    })?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(|source| LoadError::Read {
            path: display.clone(),
            source,
        })?
        .ok_or(LoadError::NoKey { path: display })
}

fn leaf_info(der: &CertificateDer<'_>) -> Option<LeafInfo> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let common_name = |name: &x509_parser::x509::X509Name<'_>| {
        name.iter_common_name()
            .next()
            .and_then(|attr| attr.as_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let dns_names = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    GeneralName::DNSName(dns) => Some((*dns).to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Some(LeafInfo {
        subject: common_name(cert.subject()),
        issuer: common_name(cert.issuer()),
        dns_names,
        not_after_unix: cert.validity().not_after.timestamp(),
    })
}

#[cfg(test)]
pub mod testing {
    use std::path::{Path, PathBuf};

    /// Writes a self-signed pair for `dns_name` into `dir` and returns the
    /// certificate and key paths.
    pub fn write_pair(dir: &Path, dns_name: &str) -> (PathBuf, PathBuf) {
        let cert = rcgen::generate_simple_self_signed(vec![dns_name.to_string()]).unwrap();
        let cert_path = dir.join("tls.crt");
        let key_path = dir.join("tls.key");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
        (cert_path, key_path)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::write_pair;
    use super::*;

    #[test]
    fn load_parses_the_leaf_and_tracks_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_pair(dir.path(), "gate.argocd.svc");
        let reloader = Reloader::new(&cert, &key);
        assert!(reloader.changed(), "nothing loaded yet counts as changed");
        assert_eq!(reloader.cert_file(), cert.as_path());

        let loaded = reloader.load().unwrap();
        let leaf = loaded.leaf.unwrap();
        assert_eq!(leaf.dns_names, vec!["gate.argocd.svc"]);
        assert!(leaf.not_after_unix > 0);
        assert_eq!(leaf.subject, leaf.issuer, "self-signed");
        assert!(!reloader.changed());

        std::thread::sleep(std::time::Duration::from_millis(20));
        write_pair(dir.path(), "other.svc");
        let mtime = std::fs::metadata(&cert).unwrap().modified().unwrap();
        let bumped = mtime + std::time::Duration::from_secs(2);
        std::fs::File::open(&cert)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        assert!(reloader.changed());
        let second = reloader.load().unwrap();
        assert_eq!(second.leaf.unwrap().dns_names, vec!["other.svc"]);
        assert!(!reloader.changed());
    }

    #[test]
    fn load_reports_missing_and_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Reloader::new(dir.path().join("none.crt"), dir.path().join("none.key"));
        assert!(matches!(
            missing.load().unwrap_err(),
            LoadError::Read { .. }
        ));

        let empty_cert = dir.path().join("empty.crt");
        std::fs::write(&empty_cert, "").unwrap();
        let (_, key) = write_pair(dir.path(), "x");
        let err = Reloader::new(&empty_cert, &key).load().unwrap_err();
        assert!(matches!(err, LoadError::NoCertificate { .. }), "{err}");

        let (cert, _) = write_pair(dir.path(), "x");
        let empty_key = dir.path().join("empty.key");
        std::fs::write(&empty_key, "").unwrap();
        let err = Reloader::new(&cert, &empty_key).load().unwrap_err();
        assert!(matches!(err, LoadError::NoKey { .. }), "{err}");

        let other = tempfile::tempdir().unwrap();
        let (_, other_key) = write_pair(other.path(), "y");
        let err = Reloader::new(&cert, &other_key).load().unwrap_err();
        assert!(
            matches!(err, LoadError::Tls { .. }),
            "mismatched key: {err}"
        );
        assert!(err.to_string().contains("load webhook certificate"));
    }
}
