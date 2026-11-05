//! Process-wide outbound HTTP settings. Every client forklift builds (proxy
//! upstreams, OSV, deps.dev, OIDC, webhooks, GitLab, object storage) trusts the
//! system roots plus an optional private CA bundle (`FORKLIFT_EXTRA_CA_FILE`),
//! loaded once at startup, so a registry or store behind a private CA verifies
//! without replacing the image's public roots. Proxies come from the standard
//! `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` variables, which
//! reqwest and the S3 connector both read.

use std::sync::OnceLock;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

/// Errors returned while loading the extra CA bundle.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("read extra ca file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse extra ca file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("extra ca file {path} holds no certificates")]
    Empty { path: String },
    #[error("extra ca file {path}: {source}")]
    Certificate {
        path: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("extra ca bundle already installed")]
    AlreadyInstalled,
}

/// Convenience alias for results in this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Private CA certificates trusted alongside the system roots.
pub struct ExtraRoots {
    /// The bundle as read, handed to the S3 connector, which parses PEM itself.
    pem: Vec<u8>,
    certs: Vec<reqwest::Certificate>,
}

impl ExtraRoots {
    /// Reads a PEM bundle. An empty path is `Ok(None)`. A file that is missing,
    /// malformed or holds no certificate is an error, so a mounted bundle that
    /// went wrong fails startup instead of silently trusting nothing extra.
    pub fn load(path: &str) -> Result<Option<Self>> {
        if path.is_empty() {
            return Ok(None);
        }
        let pem = std::fs::read(path).map_err(|source| Error::Read {
            path: path.to_string(),
            source,
        })?;
        Self::parse(path, pem).map(Some)
    }

    fn parse(path: &str, pem: Vec<u8>) -> Result<Self> {
        let ders = CertificateDer::pem_slice_iter(&pem)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|source| Error::Parse {
                path: path.to_string(),
                source,
            })?;
        if ders.is_empty() {
            return Err(Error::Empty {
                path: path.to_string(),
            });
        }
        let certs = ders
            .iter()
            .map(|der| reqwest::Certificate::from_der(der.as_ref()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|source| Error::Certificate {
                path: path.to_string(),
                source,
            })?;
        Ok(Self { pem, certs })
    }

    /// The number of certificates in the bundle.
    pub const fn len(&self) -> usize {
        self.certs.len()
    }

    /// Always false: [`ExtraRoots::load`] rejects an empty bundle.
    pub const fn is_empty(&self) -> bool {
        self.certs.is_empty()
    }
}

static EXTRA_ROOTS: OnceLock<ExtraRoots> = OnceLock::new();

/// Makes `roots` the bundle every later [`client_builder`] and S3 client
/// trusts. Clients built before the call keep the system roots only, so the
/// binary installs it before constructing any.
pub fn install(roots: ExtraRoots) -> Result<()> {
    EXTRA_ROOTS.set(roots).map_err(|_| Error::AlreadyInstalled)
}

/// A `reqwest` client builder that trusts the installed extra roots on top of
/// the system ones. Every outbound client starts here.
pub fn client_builder() -> reqwest::ClientBuilder {
    with_roots(reqwest::Client::builder(), EXTRA_ROOTS.get())
}

fn with_roots(
    builder: reqwest::ClientBuilder,
    roots: Option<&ExtraRoots>,
) -> reqwest::ClientBuilder {
    match roots {
        Some(roots) => builder.tls_certs_merge(roots.certs.iter().cloned()),
        None => builder,
    }
}

/// The installed bundle in PEM form, for clients that take PEM.
pub fn extra_roots_pem() -> Option<&'static [u8]> {
    EXTRA_ROOTS.get().map(|roots| roots.pem.as_slice())
}

/// Reports whether any proxy variable is set, for the startup log. The URL
/// itself is never returned: it may carry credentials.
pub fn proxy_configured() -> bool {
    proxy_configured_in(|key| std::env::var(key).ok())
}

fn proxy_configured_in(lookup: impl Fn(&str) -> Option<String>) -> bool {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|key| lookup(key).is_some_and(|v| !v.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_CAS: &str = "-----BEGIN CERTIFICATE-----
MIIBcDCCARagAwIBAgIUYcspt3ea70i4lu7+B33VKR7kniAwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSZm9ya2xpZnQgdGVzdCBjYSBhMCAXDTI2MTAyMDE1NTczMloY
DzIxMjYwOTI2MTU1NzMyWjAdMRswGQYDVQQDDBJmb3JrbGlmdCB0ZXN0IGNhIGEw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARNefafToHV2XBFImkCR37+Qzz6qy2i
MqZ3tpjkcvBj/LhRmqGWKry+Pf179YswxijSOE1mmyFMJ6d1BKurDRGdozIwMDAd
BgNVHQ4EFgQUrsfo4V72pvJEw6CcrJu8+nUS5KUwDwYDVR0TAQH/BAUwAwEB/zAK
BggqhkjOPQQDAgNIADBFAiAObp2pwU2WAhulh+0wgrE7p6KN48sZqtMyFdjE6lLQ
2wIhAP5J9fVgyxjv+BZVmYY8mmNP5sf7nawEyEj6F/MlhJmo
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
MIIBcTCCARagAwIBAgIUKKbo4+cSXA7gyuw/lCwyyOUcBKwwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSZm9ya2xpZnQgdGVzdCBjYSBiMCAXDTI2MTAyMDE1NTczMloY
DzIxMjYwOTI2MTU1NzMyWjAdMRswGQYDVQQDDBJmb3JrbGlmdCB0ZXN0IGNhIGIw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQ62ANuHhx4Osrvr8LG+CA8UI9agReH
zWZqPh7JQj+OpeyWPiuvtCFN6cNgvJ5Yu70dQaLSA+cWmHM50SYRmU4yozIwMDAd
BgNVHQ4EFgQUrDY+ls3FLa2EblNdvYQxT0HLghEwDwYDVR0TAQH/BAUwAwEB/zAK
BggqhkjOPQQDAgNJADBGAiEA9oSyCY0bCDyjajCGNaQwbztj1RI+59UoPe+CKHXN
eNgCIQDueZI0VOize04VTG2dv5UFzZ667WXzg1B2PhtNagBdNQ==
-----END CERTIFICATE-----
";

    fn write(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), contents).expect("write bundle");
        file
    }

    #[test]
    fn empty_path_loads_nothing() {
        assert!(ExtraRoots::load("").expect("load").is_none());
    }

    #[test]
    fn bundle_loads_every_certificate() {
        let file = write(TWO_CAS);
        let roots = ExtraRoots::load(&file.path().to_string_lossy())
            .expect("load")
            .expect("roots");
        assert_eq!(roots.len(), 2);
        assert!(!roots.is_empty());
        assert_eq!(roots.pem, TWO_CAS.as_bytes());
    }

    #[test]
    fn missing_file_is_an_error() {
        let err = ExtraRoots::load("/nonexistent/forklift-ca.pem")
            .err()
            .expect("error");
        assert!(matches!(err, Error::Read { .. }), "{err}");
    }

    #[test]
    fn file_without_certificates_is_an_error() {
        let file = write("not a certificate\n");
        let err = ExtraRoots::load(&file.path().to_string_lossy())
            .err()
            .expect("error");
        assert!(matches!(err, Error::Empty { .. }), "{err}");
    }

    #[test]
    fn malformed_certificate_is_an_error() {
        let file = write("-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n");
        let err = ExtraRoots::load(&file.path().to_string_lossy())
            .err()
            .expect("error");
        assert!(matches!(err, Error::Parse { .. }), "{err}");
    }

    #[test]
    fn builder_with_roots_builds() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let roots = ExtraRoots::parse("bundle", TWO_CAS.as_bytes().to_vec()).expect("parse");
        assert!(
            with_roots(reqwest::Client::builder(), Some(&roots))
                .build()
                .is_ok()
        );
        assert!(client_builder().build().is_ok());
    }

    #[test]
    fn proxy_detection_ignores_blank_values() {
        assert!(!proxy_configured_in(|_| None));
        assert!(!proxy_configured_in(|_| Some("  ".into())));
        assert!(proxy_configured_in(|key| {
            (key == "https_proxy").then(|| "http://proxy.example.com:3128".into())
        }));
    }

    /// The point of the bundle: a server whose certificate chains to a private
    /// CA verifies once the CA is merged in, and is refused on the system roots
    /// alone.
    #[tokio::test]
    async fn extra_root_trusts_a_private_ca_server() {
        let server = test_tls::private_ca_server().await;
        let roots = ExtraRoots::parse("bundle", server.ca_pem.clone().into_bytes()).expect("parse");

        let trusted = with_roots(reqwest::Client::builder().no_proxy(), Some(&roots))
            .build()
            .expect("client");
        let resp = trusted
            .get(&server.url)
            .send()
            .await
            .expect("trusted request");
        assert_eq!(resp.text().await.expect("body"), "ok");

        let untrusted = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let err = untrusted
            .get(&server.url)
            .send()
            .await
            .expect_err("system roots alone must refuse a private CA");
        assert!(err.is_connect(), "{err}");
    }
}

/// A TLS listener on 127.0.0.1 whose certificate chains to a CA generated for
/// the test, so no key is ever committed. It answers every request with `200`
/// and the body `ok`, enough for an HTTP client or the S3 SDK to prove the
/// handshake verified.
#[cfg(test)]
pub(crate) mod test_tls {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) struct PrivateCaServer {
        pub(crate) ca_pem: String,
        pub(crate) url: String,
    }

    pub(crate) async fn private_ca_server() -> PrivateCaServer {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca params");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "forklift test ca");
        let ca = rcgen::CertifiedIssuer::self_signed(
            ca_params,
            rcgen::KeyPair::generate().expect("ca key"),
        )
        .expect("ca certificate");
        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
            .expect("leaf params")
            .signed_by(&leaf_key, &ca)
            .expect("leaf certificate");

        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
            )
            .expect("server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 8192];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        PrivateCaServer {
            ca_pem: ca.pem(),
            url: format!("https://{addr}"),
        }
    }
}
