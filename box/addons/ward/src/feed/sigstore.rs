//! Keyless verification of a cosign bundle, as Falco does for its rules:
//! the database is signed in CI with a short-lived Fulcio certificate bound
//! to the workflow's GitHub OIDC identity, and the signature is recorded in
//! the Rekor transparency log. There is no long-lived signing key.
//!
//! A bundle verifies when
//! 1. the certificate chains to a Fulcio CA of the trusted root at the time
//!    Rekor recorded the entry, and is a code signing certificate,
//! 2. the certificate's OIDC issuer and subject (SAN) equal the expected
//!    identity,
//! 3. the signature over the database verifies with the certificate key,
//! 4. Rekor's signed entry timestamp verifies with a trusted log key, and
//!    the logged entry names this database's hash, signature, and
//!    certificate.
//!
//! The certificate's embedded SCT is not checked, the Rekor entry already
//! binds the certificate to a public log.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use x509_cert::der::{Decode as _, Encode as _};

use crate::signature::sha256_hex;

/// The Sigstore public good trusted root, from the `sigstore/root-signing`
/// TUF repository.
const PUBLIC_GOOD_ROOT: &str = include_str!("sigstore/trusted_root.json");
/// `id-kp-codeSigning`, DER value bytes.
const CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];
/// Fulcio's OIDC issuer extensions: the raw string form and the DER
/// `UTF8String` form that replaced it.
const OID_ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";
const OID_ISSUER_V2: &str = "1.3.6.1.4.1.57264.1.8";
const OID_SAN: &str = "2.5.29.17";

#[derive(Debug, thiserror::Error)]
pub enum SigstoreError {
    #[error("bundle: {0}")]
    Bundle(String),
    #[error("certificate: {0}")]
    Certificate(String),
    #[error("identity: expected {expected}, got {actual}")]
    Identity { expected: String, actual: String },
    #[error("signature over the database does not verify")]
    Signature,
    #[error("transparency log: {0}")]
    Log(String),
}

/// Who must have signed: the OIDC issuer and the certificate subject, for
/// GitHub Actions the workflow URL with its ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
}

/// What a verified bundle proves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub subject: String,
    pub log_index: i64,
    pub integrated_time: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(
    clippy::struct_field_names,
    reason = "field names follow the cosign bundle JSON"
)]
struct Bundle {
    base64_signature: String,
    /// Base64 of the PEM certificate.
    cert: String,
    rekor_bundle: RekorBundle,
}

#[derive(Deserialize)]
struct RekorBundle {
    #[serde(rename = "SignedEntryTimestamp")]
    signed_entry_timestamp: String,
    #[serde(rename = "Payload")]
    payload: RekorPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RekorPayload {
    body: String,
    integrated_time: i64,
    log_index: i64,
    #[serde(rename = "logID")]
    log_id: String,
}

#[derive(Deserialize)]
struct HashedRekord {
    kind: String,
    spec: HashedRekordSpec,
}

#[derive(Deserialize)]
struct HashedRekordSpec {
    data: HashedRekordData,
    signature: HashedRekordSignature,
}

#[derive(Deserialize)]
struct HashedRekordData {
    hash: HashedRekordHash,
}

#[derive(Deserialize)]
struct HashedRekordHash {
    algorithm: String,
    value: String,
}

#[derive(Deserialize)]
struct HashedRekordSignature {
    content: String,
    #[serde(rename = "publicKey")]
    public_key: HashedRekordKey,
}

#[derive(Deserialize)]
struct HashedRekordKey {
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrustedRoot {
    tlogs: Vec<Tlog>,
    certificate_authorities: Vec<CertificateAuthority>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tlog {
    public_key: PublicKey,
    log_id: LogId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicKey {
    raw_bytes: String,
    key_details: String,
    valid_for: Validity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogId {
    key_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CertificateAuthority {
    cert_chain: CertChain,
    valid_for: Validity,
}

#[derive(Deserialize)]
struct CertChain {
    certificates: Vec<RawCert>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCert {
    raw_bytes: String,
}

#[derive(Deserialize)]
struct Validity {
    start: DateTime<Utc>,
    #[serde(default)]
    end: Option<DateTime<Utc>>,
}

impl Validity {
    fn contains(&self, t: DateTime<Utc>) -> bool {
        self.start <= t && self.end.is_none_or(|e| t <= e)
    }
}

fn b64(s: &str, what: &str) -> Result<Vec<u8>, SigstoreError> {
    B64.decode(s.trim())
        .map_err(|e| SigstoreError::Bundle(format!("{what} is not base64: {e}")))
}

/// The DER of the first certificate in PEM text.
fn pem_cert(pem: &[u8]) -> Result<Vec<u8>, SigstoreError> {
    use rustls_pki_types::pem::PemObject as _;
    rustls_pki_types::CertificateDer::from_pem_slice(pem)
        .map(|c| c.as_ref().to_vec())
        .map_err(|e| SigstoreError::Certificate(format!("not PEM: {e}")))
}

/// The SAN URI or email of a Fulcio certificate and its OIDC issuer.
fn identity_of(cert: &x509_cert::Certificate) -> Result<(String, String), SigstoreError> {
    let mut subject = None;
    let mut issuer_v1 = None;
    let mut issuer_v2 = None;
    for ext in cert.tbs_certificate.extensions.iter().flatten() {
        let value = ext.extn_value.as_bytes();
        match ext.extn_id.to_string().as_str() {
            OID_SAN => {
                let san = x509_cert::ext::pkix::SubjectAltName::from_der(value)
                    .map_err(|e| SigstoreError::Certificate(format!("SAN: {e}")))?;
                subject = san.0.into_iter().find_map(|n| match n {
                    x509_cert::ext::pkix::name::GeneralName::UniformResourceIdentifier(u) => {
                        Some(u.to_string())
                    }
                    x509_cert::ext::pkix::name::GeneralName::Rfc822Name(e) => Some(e.to_string()),
                    _ => None,
                });
            }
            OID_ISSUER_V1 => issuer_v1 = String::from_utf8(value.to_vec()).ok(),
            OID_ISSUER_V2 => {
                issuer_v2 = x509_cert::der::asn1::Utf8StringRef::from_der(value)
                    .ok()
                    .map(|s| s.as_str().to_string());
            }
            _ => {}
        }
    }
    let subject = subject.ok_or_else(|| SigstoreError::Certificate("no SAN identity".into()))?;
    let issuer = issuer_v2
        .or(issuer_v1)
        .ok_or_else(|| SigstoreError::Certificate("no OIDC issuer extension".into()))?;
    Ok((subject, issuer))
}

/// The uncompressed P-256 point of a certificate's key.
fn cert_point(cert: &x509_cert::Certificate) -> Result<Vec<u8>, SigstoreError> {
    let spki = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|e| SigstoreError::Certificate(e.to_string()))?;
    crate::content::p256_point(&spki)
        .map(<[u8]>::to_vec)
        .map_err(|_| SigstoreError::Certificate("key is not ECDSA P-256".into()))
}

fn verify_p256(point: &[u8], data: &[u8], sig: &[u8]) -> bool {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(data, sig)
        .is_ok()
}

/// The JSON Rekor signs for a signed entry timestamp: the payload's keys
/// in canonical (sorted) order, no whitespace.
fn canonical_payload(p: &RekorPayload) -> Vec<u8> {
    let body = serde_json::to_string(&p.body).unwrap_or_default();
    let log_id = serde_json::to_string(&p.log_id).unwrap_or_default();
    format!(
        r#"{{"body":{body},"integratedTime":{},"logID":{log_id},"logIndex":{}}}"#,
        p.integrated_time, p.log_index
    )
    .into_bytes()
}

struct Root {
    tlogs: Vec<Tlog>,
    cas: Vec<CertificateAuthority>,
}

fn public_good() -> Result<Root, SigstoreError> {
    let t: TrustedRoot = serde_json::from_str(PUBLIC_GOOD_ROOT)
        .map_err(|e| SigstoreError::Bundle(format!("trusted root: {e}")))?;
    Ok(Root {
        tlogs: t.tlogs,
        cas: t.certificate_authorities,
    })
}

/// Verifies `bundle` (cosign `sign-blob --bundle` output) over `blob`
/// against the Sigstore public good instance.
pub fn verify(blob: &[u8], bundle: &[u8], expected: &Identity) -> Result<Verified, SigstoreError> {
    verify_with(&public_good()?, blob, bundle, expected)
}

fn verify_with(
    root: &Root,
    blob: &[u8],
    bundle: &[u8],
    expected: &Identity,
) -> Result<Verified, SigstoreError> {
    let b: Bundle = serde_json::from_slice(bundle)
        .map_err(|e| SigstoreError::Bundle(format!("not a cosign bundle: {e}")))?;
    let payload = &b.rekor_bundle.payload;
    let integrated = DateTime::from_timestamp(payload.integrated_time, 0)
        .ok_or_else(|| SigstoreError::Log("integratedTime out of range".into()))?;

    // 4. The transparency log entry and its signed entry timestamp.
    let log_id =
        hex_decode(&payload.log_id).ok_or_else(|| SigstoreError::Log("logID is not hex".into()))?;
    let tlog = root
        .tlogs
        .iter()
        .find(|t| b64(&t.log_id.key_id, "log id").is_ok_and(|k| k == log_id))
        .ok_or_else(|| SigstoreError::Log("entry is from an untrusted log".into()))?;
    if tlog.public_key.key_details != "PKIX_ECDSA_P256_SHA_256"
        || !tlog.public_key.valid_for.contains(integrated)
    {
        return Err(SigstoreError::Log(
            "log key is not valid for this entry".into(),
        ));
    }
    let log_key = b64(&tlog.public_key.raw_bytes, "log key")?;
    let log_point = crate::content::p256_point(&log_key)
        .map_err(|_| SigstoreError::Log("log key is not ECDSA P-256".into()))?;
    let set = b64(
        &b.rekor_bundle.signed_entry_timestamp,
        "signed entry timestamp",
    )?;
    if !verify_p256(log_point, &canonical_payload(payload), &set) {
        return Err(SigstoreError::Log(
            "signed entry timestamp does not verify".into(),
        ));
    }

    // 1. The certificate chains to Fulcio at the time of the entry.
    let cert_pem = b64(&b.cert, "certificate")?;
    let leaf_der = pem_cert(&cert_pem)?;
    let leaf = x509_cert::Certificate::from_der(&leaf_der)
        .map_err(|e| SigstoreError::Certificate(e.to_string()))?;
    let at = rustls_pki_types::UnixTime::since_unix_epoch(Duration::from_secs(
        u64::try_from(payload.integrated_time).unwrap_or(0),
    ));
    let mut why = String::from("no Fulcio CA is valid at the entry time");
    let chained = root
        .cas
        .iter()
        .filter(|ca| ca.valid_for.contains(integrated))
        .any(|ca| match chains(&leaf_der, ca, at) {
            Ok(()) => true,
            Err(e) => {
                why = e;
                false
            }
        });
    if !chained {
        return Err(SigstoreError::Certificate(format!(
            "does not chain to a trusted Fulcio CA at the entry time: {why}"
        )));
    }

    // 2. The identity.
    let (subject, issuer) = identity_of(&leaf)?;
    if issuer != expected.issuer {
        return Err(SigstoreError::Identity {
            expected: expected.issuer.clone(),
            actual: issuer,
        });
    }
    if subject != expected.subject {
        return Err(SigstoreError::Identity {
            expected: expected.subject.clone(),
            actual: subject,
        });
    }

    // 3. The signature over the database.
    let sig = b64(&b.base64_signature, "signature")?;
    if !verify_p256(&cert_point(&leaf)?, blob, &sig) {
        return Err(SigstoreError::Signature);
    }

    // 4. The logged entry is about this database, signature, and certificate.
    let body: HashedRekord = serde_json::from_slice(&b64(&payload.body, "log entry")?)
        .map_err(|e| SigstoreError::Log(format!("entry is not hashedrekord: {e}")))?;
    let logged_cert = b64(
        &body.spec.signature.public_key.content,
        "logged certificate",
    )
    .and_then(|pem| pem_cert(&pem))?;
    if body.kind != "hashedrekord"
        || body.spec.data.hash.algorithm != "sha256"
        || body.spec.data.hash.value != sha256_hex(blob)
        || b64(&body.spec.signature.content, "logged signature")? != sig
        || logged_cert != leaf_der
    {
        return Err(SigstoreError::Log(
            "entry does not match this database, signature, and certificate".into(),
        ));
    }
    Ok(Verified {
        subject,
        log_index: payload.log_index,
        integrated_time: integrated,
    })
}

/// Whether `leaf` is a code signing certificate issued through `ca`'s chain
/// (intermediates first, the root last) and valid at `at`.
fn chains(
    leaf: &[u8],
    ca: &CertificateAuthority,
    at: rustls_pki_types::UnixTime,
) -> Result<(), String> {
    let certs = ca
        .cert_chain
        .certificates
        .iter()
        .map(|c| b64(&c.raw_bytes, "CA certificate").map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let (root, intermediates) = certs.split_last().ok_or("empty CA chain")?;
    let root_der = rustls_pki_types::CertificateDer::from(root.as_slice());
    let anchor = webpki::anchor_from_trusted_cert(&root_der).map_err(|e| e.to_string())?;
    let intermediates: Vec<rustls_pki_types::CertificateDer<'_>> = intermediates
        .iter()
        .map(|c| rustls_pki_types::CertificateDer::from(c.as_slice()))
        .collect();
    let leaf_der = rustls_pki_types::CertificateDer::from(leaf);
    let cert = webpki::EndEntityCert::try_from(&leaf_der).map_err(|e| e.to_string())?;
    cert.verify_for_usage(
        webpki::ALL_VERIFICATION_ALGS,
        &[anchor],
        &intermediates,
        at,
        webpki::KeyUsage::required(CODE_SIGNING),
        None,
        None,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, SanType,
    };

    use super::*;

    const ISSUER: &str = "https://token.actions.githubusercontent.com";
    const SUBJECT: &str =
        "https://github.com/younsl/addons/.github/workflows/ward-db.yml@refs/heads/main";

    fn spki_of(point: &[u8]) -> Vec<u8> {
        let mut spki = vec![
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        spki.extend_from_slice(point);
        spki
    }

    /// A stand-in for Fulcio and Rekor, and a bundle they would produce.
    struct Fixture {
        root: Root,
        blob: Vec<u8>,
        bundle: serde_json::Value,
        log_key: EcdsaKeyPair,
    }

    fn ca_params(cn: &str) -> CertificateParams {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.distinguished_name
            .push(rcgen::DnType::CommonName, cn.to_string());
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        p
    }

    fn fixture(subject: &str) -> Fixture {
        let root_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        let root = ca_params("sigstore").self_signed(&root_key).unwrap();
        let root_issuer = rcgen::Issuer::new(ca_params("sigstore"), root_key);
        let inter_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        // Fulcio's intermediate carries the code signing EKU, which webpki
        // checks along the whole path.
        let mut inter_params = ca_params("sigstore-intermediate");
        inter_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::CodeSigning];
        let inter = inter_params.signed_by(&inter_key, &root_issuer).unwrap();
        let mut issuer_params = ca_params("sigstore-intermediate");
        issuer_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::CodeSigning];
        let inter_issuer = rcgen::Issuer::new(issuer_params, inter_key);

        let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut lp = CertificateParams::new(Vec::<String>::new()).unwrap();
        lp.subject_alt_names = vec![SanType::URI(subject.try_into().unwrap())];
        lp.extended_key_usages = vec![ExtendedKeyUsagePurpose::CodeSigning];
        lp.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        let issuer_der = x509_cert::der::asn1::Utf8StringRef::new(ISSUER)
            .unwrap()
            .to_der()
            .unwrap();
        lp.custom_extensions = vec![
            CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 57264, 1, 8], issuer_der),
            CustomExtension::from_oid_content(
                &[1, 3, 6, 1, 4, 1, 57264, 1, 1],
                ISSUER.as_bytes().to_vec(),
            ),
        ];
        let leaf = lp.signed_by(&leaf_key, &inter_issuer).unwrap();
        let leaf_pem = leaf.pem();

        let blob = b"ward database bytes".to_vec();
        let signer =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &leaf_key.serialize_der())
                .unwrap();
        let rng = SystemRandom::new();
        let sig = signer.sign(&rng, &blob).unwrap().as_ref().to_vec();

        let log_key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
        let log_spki = spki_of(log_key.public_key().as_ref());
        let log_id = sha256_hex(&log_spki);
        let entry = serde_json::json!({
            "apiVersion": "0.0.1",
            "kind": "hashedrekord",
            "spec": {
                "data": {"hash": {"algorithm": "sha256", "value": sha256_hex(&blob)}},
                "signature": {
                    "content": B64.encode(&sig),
                    "publicKey": {"content": B64.encode(leaf_pem.as_bytes())},
                },
            },
        });
        let mut fx = Fixture {
            root: Root {
                tlogs: vec![Tlog {
                    public_key: PublicKey {
                        raw_bytes: B64.encode(&log_spki),
                        key_details: "PKIX_ECDSA_P256_SHA_256".into(),
                        valid_for: Validity {
                            start: Utc::now() - chrono::Duration::days(1),
                            end: None,
                        },
                    },
                    log_id: LogId {
                        key_id: B64.encode(hex_decode(&log_id).unwrap()),
                    },
                }],
                cas: vec![CertificateAuthority {
                    cert_chain: CertChain {
                        certificates: vec![
                            RawCert {
                                raw_bytes: B64.encode(inter.der()),
                            },
                            RawCert {
                                raw_bytes: B64.encode(root.der()),
                            },
                        ],
                    },
                    valid_for: Validity {
                        start: Utc::now() - chrono::Duration::days(1),
                        end: None,
                    },
                }],
            },
            bundle: serde_json::json!({
                "base64Signature": B64.encode(&sig),
                "cert": B64.encode(leaf_pem.as_bytes()),
                "rekorBundle": {
                    "SignedEntryTimestamp": "",
                    "Payload": {
                        "body": B64.encode(entry.to_string()),
                        "integratedTime": Utc::now().timestamp(),
                        "logIndex": 42,
                        "logID": log_id,
                    },
                },
            }),
            blob,
            log_key,
        };
        fx.sign_entry();
        fx
    }

    impl Fixture {
        fn sign_entry(&mut self) {
            let p: RekorPayload =
                serde_json::from_value(self.bundle["rekorBundle"]["Payload"].clone()).unwrap();
            let set = self
                .log_key
                .sign(&SystemRandom::new(), &canonical_payload(&p))
                .unwrap();
            self.bundle["rekorBundle"]["SignedEntryTimestamp"] =
                serde_json::json!(B64.encode(set.as_ref()));
        }

        fn verify(&self, blob: &[u8], subject: &str) -> Result<Verified, SigstoreError> {
            verify_with(
                &self.root,
                blob,
                self.bundle.to_string().as_bytes(),
                &Identity {
                    issuer: ISSUER.into(),
                    subject: subject.into(),
                },
            )
        }
    }

    #[test]
    fn a_ci_signed_bundle_verifies() {
        let fx = fixture(SUBJECT);
        let v = fx.verify(&fx.blob, SUBJECT).unwrap();
        assert_eq!(v.subject, SUBJECT);
        assert_eq!(v.log_index, 42);
    }

    #[test]
    fn every_broken_link_fails() {
        let fx = fixture(SUBJECT);
        assert!(matches!(
            fx.verify(b"tampered", SUBJECT),
            Err(SigstoreError::Signature)
        ));
        assert!(matches!(
            fx.verify(
                &fx.blob,
                "https://github.com/evil/fork/.github/workflows/x.yml@refs/heads/main"
            ),
            Err(SigstoreError::Identity { .. })
        ));

        let mut other_issuer = fixture(SUBJECT);
        other_issuer.root.cas.clear();
        assert!(matches!(
            other_issuer.verify(&other_issuer.blob, SUBJECT),
            Err(SigstoreError::Certificate(_))
        ));

        let mut forged = fixture(SUBJECT);
        forged.bundle["rekorBundle"]["Payload"]["logIndex"] = serde_json::json!(43);
        assert!(matches!(
            forged.verify(&forged.blob, SUBJECT),
            Err(SigstoreError::Log(_))
        ));

        let mut wrong_entry = fixture(SUBJECT);
        let entry = serde_json::json!({
            "kind": "hashedrekord",
            "spec": {
                "data": {"hash": {"algorithm": "sha256", "value": sha256_hex(b"other")}},
                "signature": {
                    "content": wrong_entry.bundle["base64Signature"].clone(),
                    "publicKey": {"content": wrong_entry.bundle["cert"].clone()},
                },
            },
        });
        wrong_entry.bundle["rekorBundle"]["Payload"]["body"] =
            serde_json::json!(B64.encode(entry.to_string()));
        wrong_entry.sign_entry();
        assert!(matches!(
            wrong_entry.verify(&wrong_entry.blob, SUBJECT),
            Err(SigstoreError::Log(_))
        ));

        let mut untrusted_log = fixture(SUBJECT);
        untrusted_log.root.tlogs.clear();
        assert!(matches!(
            untrusted_log.verify(&untrusted_log.blob, SUBJECT),
            Err(SigstoreError::Log(_))
        ));

        let mut old = fixture(SUBJECT);
        old.bundle["rekorBundle"]["Payload"]["integratedTime"] =
            serde_json::json!((Utc::now() - chrono::Duration::days(3)).timestamp());
        old.sign_entry();
        assert!(
            old.verify(&old.blob, SUBJECT).is_err(),
            "outside every validity window"
        );

        assert!(
            verify_with(
                &fx.root,
                &fx.blob,
                b"{}",
                &Identity {
                    issuer: ISSUER.into(),
                    subject: SUBJECT.into(),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn the_public_good_root_parses() {
        let root = public_good().unwrap();
        assert!(
            root.tlogs
                .iter()
                .any(|t| t.public_key.key_details == "PKIX_ECDSA_P256_SHA_256")
        );
        assert!(root.cas.iter().any(|c| c.valid_for.end.is_none()));
        assert_eq!(hex_decode("0aff"), Some(vec![0x0a, 0xff]));
        assert_eq!(hex_decode("0"), None);
    }
}
