//! The private PKI between agents and the manager: the operator keeps a CA
//! in a Secret and issues the manager server and agent client certificates,
//! and both sides build mutual TLS configs from the mounted files.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Datelike as _, Duration, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

pub const CA_COMMON_NAME: &str = "ward-ca";
pub const LEAF_DAYS: i64 = 365;
pub const CA_DAYS: i64 = 3650;
/// Leaves are reissued once fewer than this many days remain.
pub const RENEW_BEFORE_DAYS: i64 = 30;

pub const CERT_FILE: &str = "tls.crt";
pub const KEY_FILE: &str = "tls.key";
pub const CA_FILE: &str = "ca.crt";

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parse {0}: no PEM object")]
    Pem(String),
    #[error("tls config: {0}")]
    Config(String),
}

fn ymd(t: DateTime<Utc>) -> time::OffsetDateTime {
    rcgen::date_time_ymd(
        t.year(),
        u8::try_from(t.month()).unwrap_or(1),
        u8::try_from(t.day()).unwrap_or(1),
    )
}

fn ca_params(now: DateTime<Utc>) -> Result<CertificateParams, TlsError> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.distinguished_name
        .push(DnType::CommonName, CA_COMMON_NAME);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.not_before = ymd(now - Duration::days(1));
    p.not_after = ymd(now + Duration::days(CA_DAYS));
    Ok(p)
}

/// A CA certificate and key, PEM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ca {
    pub cert_pem: String,
    pub key_pem: String,
}

/// A leaf certificate, its key, and its expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after: DateTime<Utc>,
}

impl Ca {
    pub fn generate(now: DateTime<Utc>) -> Result<Self, TlsError> {
        let key = KeyPair::generate()?;
        let cert = ca_params(now)?.self_signed(&key)?;
        Ok(Self {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }

    /// Issues a leaf. `server` adds `serverAuth` and the DNS names, else the
    /// leaf is a client certificate.
    pub fn issue(
        &self,
        common_name: &str,
        dns_names: &[String],
        server: bool,
        now: DateTime<Utc>,
    ) -> Result<Leaf, TlsError> {
        let ca_key = KeyPair::from_pem(&self.key_pem)?;
        let issuer = Issuer::new(ca_params(now)?, ca_key);
        let mut p = CertificateParams::new(if server {
            dns_names.to_vec()
        } else {
            Vec::new()
        })?;
        p.distinguished_name.push(DnType::CommonName, common_name);
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![if server {
            ExtendedKeyUsagePurpose::ServerAuth
        } else {
            ExtendedKeyUsagePurpose::ClientAuth
        }];
        let not_after = now + Duration::days(LEAF_DAYS);
        p.not_before = ymd(now - Duration::days(1));
        p.not_after = ymd(not_after);
        let key = KeyPair::generate()?;
        let cert = p.signed_by(&key, &issuer)?;
        Ok(Leaf {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            not_after,
        })
    }
}

/// Whether a leaf expiring at `not_after` should be reissued.
pub fn needs_renewal(not_after: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    not_after.is_none_or(|t| t - now < Duration::days(RENEW_BEFORE_DAYS))
}

fn read(dir: &Path, name: &str) -> Result<Vec<u8>, TlsError> {
    let path = dir.join(name);
    std::fs::read(&path).map_err(|source| TlsError::Read {
        path: path.display().to_string(),
        source,
    })
}

struct Material {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    roots: Arc<RootCertStore>,
}

fn material(dir: &Path) -> Result<Material, TlsError> {
    let certs: Vec<_> = CertificateDer::pem_slice_iter(&read(dir, CERT_FILE)?)
        .filter_map(Result::ok)
        .collect();
    if certs.is_empty() {
        return Err(TlsError::Pem(CERT_FILE.into()));
    }
    let key = PrivateKeyDer::from_pem_slice(&read(dir, KEY_FILE)?)
        .map_err(|_| TlsError::Pem(KEY_FILE.into()))?;
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(&read(dir, CA_FILE)?).filter_map(Result::ok) {
        roots.add(ca).map_err(|e| TlsError::Config(e.to_string()))?;
    }
    if roots.is_empty() {
        return Err(TlsError::Pem(CA_FILE.into()));
    }
    Ok(Material {
        certs,
        key,
        roots: Arc::new(roots),
    })
}

/// A server config that requires a client certificate from the same CA.
pub fn server_config(dir: &Path) -> Result<Arc<rustls::ServerConfig>, TlsError> {
    let m = material(dir)?;
    let verifier = WebPkiClientVerifier::builder(m.roots)
        .build()
        .map_err(|e| TlsError::Config(e.to_string()))?;
    let cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(m.certs, m.key)
        .map_err(|e| TlsError::Config(e.to_string()))?;
    Ok(Arc::new(cfg))
}

/// A client config that trusts only the CA and presents the client leaf.
pub fn client_config(dir: &Path) -> Result<rustls::ClientConfig, TlsError> {
    let m = material(dir)?;
    rustls::ClientConfig::builder()
        .with_root_certificates(m.roots)
        .with_client_auth_cert(m.certs, m.key)
        .map_err(|e| TlsError::Config(e.to_string()))
}

/// Installs aws-lc-rs as the process-wide rustls provider. Idempotent.
pub fn install_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, leaf: &Leaf, ca: &Ca) {
        std::fs::write(dir.join(CERT_FILE), &leaf.cert_pem).unwrap();
        std::fs::write(dir.join(KEY_FILE), &leaf.key_pem).unwrap();
        std::fs::write(dir.join(CA_FILE), &ca.cert_pem).unwrap();
    }

    #[test]
    fn issues_and_builds_mutual_tls() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let now = Utc::now();
        let ca = Ca::generate(now).unwrap();
        let server = ca
            .issue("ward-manager", &["ward-manager.sec.svc".into()], true, now)
            .unwrap();
        let client = ca.issue("ward-agent", &[], false, now).unwrap();
        assert!(server.not_after > now + Duration::days(300));
        let s = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        write(s.path(), &server, &ca);
        write(c.path(), &client, &ca);
        server_config(s.path()).unwrap();
        client_config(c.path()).unwrap();

        let empty = tempfile::tempdir().unwrap();
        assert!(matches!(
            server_config(empty.path()),
            Err(TlsError::Read { .. })
        ));
        std::fs::write(empty.path().join(CERT_FILE), "junk").unwrap();
        assert!(matches!(client_config(empty.path()), Err(TlsError::Pem(_))));
    }

    #[test]
    fn renewal_window() {
        let now = Utc::now();
        assert!(needs_renewal(None, now));
        assert!(needs_renewal(Some(now + Duration::days(10)), now));
        assert!(!needs_renewal(Some(now + Duration::days(200)), now));
    }
}
