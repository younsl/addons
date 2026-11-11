//! Local session cache: store the master password for `TTL` after a
//! successful unlock so the next launch auto-unlocks without prompting.
//!
//! Two storage formats are supported, picked at write time based on what the
//! environment can offer:
//!
//! 1. **Encrypted** — preferred. The cache payload is encrypted with
//!    ChaCha20-Poly1305 using a per-machine wrap key persisted in the OS
//!    keyring (macOS Keychain / Linux Secret Service / Windows Credential
//!    Manager). A stolen cache file alone is useless without keyring access.
//!
//! 2. **Plaintext fallback** — when the keyring is unavailable (headless
//!    Linux without secret service, locked Keychain that refused access,
//!    etc.) the master is written in clear text. The file is still chmod
//!    0600 and bound to the vault path so it cannot be reused on another
//!    machine or against another vault. Same trust model as
//!    `~/.aws/credentials`.
//!
//! Both formats include `expires_at` and a `vault_path` binding. The TTL
//! defaults to one hour and can be overridden with `VLT_SESSION_TTL_SECONDS`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce, aead::Aead};
use serde::{Deserialize, Serialize};

pub const TTL: Duration = Duration::from_secs(3600);

const KEYRING_SERVICE: &str = "io.younsl.vlt";
const KEYRING_ACCOUNT: &str = "session-wrap-key";
const NONCE_LEN: usize = 12;
const WRAP_KEY_LEN: usize = 32;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    expires_at: i64,
    vault_path: String,
    #[serde(flatten)]
    payload: Payload,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Payload {
    Wrapped {
        nonce_b64: String,
        ciphertext_b64: String,
    },
    Plain {
        master: String,
    },
}

/// Outcome of a cache lookup on launch.
pub enum LoadResult {
    Hit { master: String, encrypted: bool },
    Miss,
}

/// Outcome of a cache write after a successful unlock.
#[derive(Clone, Copy)]
pub enum SaveOutcome {
    /// Cache written and encrypted via the OS keyring.
    Encrypted,
    /// Cache written but the keyring was unavailable, so the master sits in
    /// a 0600 plaintext file.
    Plaintext,
    /// Cache could not be written at all.
    Failed,
}

fn ttl() -> Duration {
    parse_ttl(std::env::var("VLT_SESSION_TTL_SECONDS").ok())
}

fn parse_ttl(raw: Option<String>) -> Duration {
    raw.and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map_or(TTL, Duration::from_secs)
}

fn cache_path() -> Option<PathBuf> {
    cache_path_from(
        std::env::var_os("VLT_SESSION_PATH"),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

fn cache_path_from(
    session: Option<OsString>,
    xdg_cache: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(p) = session.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let base = xdg_cache
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })?;
    Some(base.join("vlt").join("session.json"))
}

type WrapKeyFn = fn() -> Option<[u8; WRAP_KEY_LEN]>;

/// Session cache location plus the source of the keyring wrap key.
pub struct Store {
    path: Option<PathBuf>,
    wrap_key: WrapKeyFn,
}

impl Store {
    pub fn from_env() -> Self {
        Self::new(cache_path(), try_wrap_key)
    }

    pub const fn new(path: Option<PathBuf>, wrap_key: WrapKeyFn) -> Self {
        Self { path, wrap_key }
    }
}

/// Fetch (or create) the per-machine wrap key from the OS keyring. Returns
/// `None` if the keyring is unavailable for any reason.
fn try_wrap_key() -> Option<[u8; WRAP_KEY_LEN]> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).ok()?;
    match entry.get_password() {
        Ok(stored) => B64.decode(stored.as_bytes()).ok()?.try_into().ok(),
        Err(keyring::Error::NoEntry) => {
            let mut k = [0u8; WRAP_KEY_LEN];
            getrandom::fill(&mut k).ok()?;
            entry.set_password(&B64.encode(k)).ok()?;
            Some(k)
        }
        Err(_) => None,
    }
}

fn drop_cache(path: &Path) {
    let _ = std::fs::remove_file(path);
}

impl Store {
    pub fn load(&self, vault_path: &Path) -> LoadResult {
        let Some(path) = self.path.as_deref() else {
            return LoadResult::Miss;
        };
        let Ok(raw) = std::fs::read(path) else {
            return LoadResult::Miss;
        };
        let Ok(cache) = serde_json::from_slice::<CacheFile>(&raw) else {
            // Old/unknown layout — toss it.
            drop_cache(path);
            return LoadResult::Miss;
        };

        let now = chrono::Utc::now().timestamp();
        if cache.expires_at <= now {
            drop_cache(path);
            return LoadResult::Miss;
        }
        if cache.vault_path != vault_path.display().to_string() {
            drop_cache(path);
            return LoadResult::Miss;
        }

        match cache.payload {
            Payload::Plain { master } => LoadResult::Hit {
                master,
                encrypted: false,
            },
            Payload::Wrapped {
                nonce_b64,
                ciphertext_b64,
            } => {
                // We need the wrap key from the keyring. If unavailable now (e.g.
                // user denied a Keychain prompt this session), the wrapped cache
                // is unusable — drop it and treat as a miss so the user is
                // prompted again and a fresh cache is written, possibly using the
                // plaintext fallback.
                let Some(key) = (self.wrap_key)() else {
                    return LoadResult::Miss;
                };
                let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
                let Ok(nonce_bytes) = B64.decode(nonce_b64.as_bytes()) else {
                    drop_cache(path);
                    return LoadResult::Miss;
                };
                if nonce_bytes.len() != NONCE_LEN {
                    drop_cache(path);
                    return LoadResult::Miss;
                }
                let nonce = Nonce::from_slice(&nonce_bytes);
                let Ok(ct) = B64.decode(ciphertext_b64.as_bytes()) else {
                    drop_cache(path);
                    return LoadResult::Miss;
                };
                let Ok(plain) = cipher.decrypt(nonce, ct.as_ref()) else {
                    drop_cache(path);
                    return LoadResult::Miss;
                };
                String::from_utf8(plain).map_or_else(
                    |_| {
                        drop_cache(path);
                        LoadResult::Miss
                    },
                    |master| LoadResult::Hit {
                        master,
                        encrypted: true,
                    },
                )
            }
        }
    }

    pub fn save(&self, master: &str, vault_path: &Path) -> SaveOutcome {
        let Some(path) = self.path.as_deref() else {
            return SaveOutcome::Failed;
        };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return SaveOutcome::Failed;
        }

        let (payload, encrypted) = match (self.wrap_key)() {
            Some(key) => {
                let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
                let mut nonce_bytes = [0u8; NONCE_LEN];
                if getrandom::fill(&mut nonce_bytes).is_err() {
                    return SaveOutcome::Failed;
                }
                let nonce = Nonce::from_slice(&nonce_bytes);
                let Ok(ct) = cipher.encrypt(nonce, master.as_bytes()) else {
                    return SaveOutcome::Failed;
                };
                (
                    Payload::Wrapped {
                        nonce_b64: B64.encode(nonce_bytes),
                        ciphertext_b64: B64.encode(ct),
                    },
                    true,
                )
            }
            None => (
                Payload::Plain {
                    master: master.to_string(),
                },
                false,
            ),
        };

        let cache = CacheFile {
            expires_at: chrono::Utc::now().timestamp() + ttl().as_secs().cast_signed(),
            vault_path: vault_path.display().to_string(),
            payload,
        };
        let Ok(bytes) = serde_json::to_vec(&cache) else {
            return SaveOutcome::Failed;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_err() {
            return SaveOutcome::Failed;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        if std::fs::rename(&tmp, path).is_err() {
            return SaveOutcome::Failed;
        }
        if encrypted {
            SaveOutcome::Encrypted
        } else {
            SaveOutcome::Plaintext
        }
    }

    /// Removes the cache file. The keyring wrap key is left in place — it is
    /// per-machine infrastructure, not per-session.
    pub fn clear(&self) {
        if let Some(path) = self.path.as_deref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, fixed_keyring, no_keyring};

    const MASTER: &str = "session master";

    fn vault_path(dir: &TempDir) -> PathBuf {
        dir.vault_path()
    }

    fn write_cache(path: &Path, expires_at: i64, vault_path: &Path, payload: Payload) {
        let cache = CacheFile {
            expires_at,
            vault_path: vault_path.display().to_string(),
            payload,
        };
        std::fs::write(path, serde_json::to_vec(&cache).unwrap()).unwrap();
    }

    fn future() -> i64 {
        chrono::Utc::now().timestamp() + 600
    }

    fn wrapped(nonce: &[u8], plaintext: &[u8]) -> Payload {
        let key = fixed_keyring().unwrap();
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let ct = cipher.encrypt(Nonce::from_slice(nonce), plaintext).unwrap();
        Payload::Wrapped {
            nonce_b64: B64.encode(nonce),
            ciphertext_b64: B64.encode(ct),
        }
    }

    fn assert_miss(result: &LoadResult) {
        assert!(matches!(result, LoadResult::Miss));
    }

    #[test]
    fn parse_ttl_defaults_and_overrides() {
        assert_eq!(parse_ttl(None), TTL);
        assert_eq!(parse_ttl(Some("abc".into())), TTL);
        assert_eq!(parse_ttl(Some("0".into())), TTL);
        assert_eq!(parse_ttl(Some("90".into())), Duration::from_secs(90));
        assert!(ttl() > Duration::ZERO);
    }

    #[test]
    fn cache_path_prefers_explicit_override() {
        let p = cache_path_from(
            Some("/x/s.json".into()),
            Some("/c".into()),
            Some("/h".into()),
        );
        assert_eq!(p, Some(PathBuf::from("/x/s.json")));
    }

    #[test]
    fn cache_path_uses_xdg_then_home() {
        assert_eq!(
            cache_path_from(Some("".into()), Some("/c".into()), Some("/h".into())),
            Some(PathBuf::from("/c/vlt/session.json"))
        );
        assert_eq!(
            cache_path_from(None, Some("".into()), Some("/h".into())),
            Some(PathBuf::from("/h/.cache/vlt/session.json"))
        );
        assert_eq!(cache_path_from(None, None, Some("".into())), None);
        assert_eq!(cache_path_from(None, None, None), None);
    }

    #[test]
    fn from_env_builds_a_store() {
        let store = Store::from_env();
        assert_eq!(store.path, cache_path());
    }

    #[test]
    fn store_without_path_misses_and_fails() {
        let dir = TempDir::new();
        let store = Store::new(None, no_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
        assert!(matches!(
            store.save(MASTER, &vault_path(&dir)),
            SaveOutcome::Failed
        ));
        store.clear();
    }

    #[test]
    fn plaintext_roundtrip_when_keyring_unavailable() {
        let dir = TempDir::new();
        let store = Store::new(Some(dir.session_path()), no_keyring);
        assert!(matches!(
            store.save(MASTER, &vault_path(&dir)),
            SaveOutcome::Plaintext
        ));
        match store.load(&vault_path(&dir)) {
            LoadResult::Hit { master, encrypted } => {
                assert_eq!(master, MASTER);
                assert!(!encrypted);
            }
            LoadResult::Miss => panic!("expected cache hit"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn saved_cache_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let store = Store::new(Some(dir.session_path()), no_keyring);
        store.save(MASTER, &vault_path(&dir));
        let mode = std::fs::metadata(dir.session_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn encrypted_roundtrip_with_keyring() {
        let dir = TempDir::new();
        let store = Store::new(Some(dir.session_path()), fixed_keyring);
        assert!(matches!(
            store.save(MASTER, &vault_path(&dir)),
            SaveOutcome::Encrypted
        ));
        let raw = std::fs::read_to_string(dir.session_path()).unwrap();
        assert!(!raw.contains(MASTER));
        match store.load(&vault_path(&dir)) {
            LoadResult::Hit { master, encrypted } => {
                assert_eq!(master, MASTER);
                assert!(encrypted);
            }
            LoadResult::Miss => panic!("expected cache hit"),
        }
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = TempDir::new();
        let path = dir.path().join("nested").join("session.json");
        let store = Store::new(Some(path.clone()), no_keyring);
        store.save(MASTER, &vault_path(&dir));
        assert!(path.exists());
    }

    #[test]
    fn save_fails_when_parent_is_a_file() {
        let dir = TempDir::new();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let store = Store::new(Some(blocker.join("session.json")), no_keyring);
        assert!(matches!(
            store.save(MASTER, &vault_path(&dir)),
            SaveOutcome::Failed
        ));
    }

    #[test]
    fn save_fails_when_target_is_a_directory() {
        let dir = TempDir::new();
        let target = dir.path().join("session.json");
        std::fs::create_dir_all(target.join("child")).unwrap();
        let store = Store::new(Some(target), no_keyring);
        assert!(matches!(
            store.save(MASTER, &vault_path(&dir)),
            SaveOutcome::Failed
        ));
    }

    #[test]
    fn save_fails_when_tmp_file_cannot_be_written() {
        let dir = TempDir::new();
        let target = dir.path().join("session.json");
        std::fs::create_dir_all(target.with_extension("json.tmp")).unwrap();
        let store = Store::new(Some(target), no_keyring);
        assert!(matches!(
            store.save(MASTER, &vault_path(&dir)),
            SaveOutcome::Failed
        ));
    }

    #[test]
    fn load_missing_file_is_miss() {
        let dir = TempDir::new();
        let store = Store::new(Some(dir.session_path()), no_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
    }

    #[test]
    fn load_drops_unparseable_cache() {
        let dir = TempDir::new();
        std::fs::write(dir.session_path(), b"{ not json").unwrap();
        let store = Store::new(Some(dir.session_path()), no_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
        assert!(!dir.session_path().exists());
    }

    #[test]
    fn load_drops_expired_cache() {
        let dir = TempDir::new();
        write_cache(
            &dir.session_path(),
            chrono::Utc::now().timestamp() - 1,
            &vault_path(&dir),
            Payload::Plain {
                master: MASTER.into(),
            },
        );
        let store = Store::new(Some(dir.session_path()), no_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
        assert!(!dir.session_path().exists());
    }

    #[test]
    fn load_drops_cache_bound_to_other_vault() {
        let dir = TempDir::new();
        write_cache(
            &dir.session_path(),
            future(),
            Path::new("/elsewhere/vault.json"),
            Payload::Plain {
                master: MASTER.into(),
            },
        );
        let store = Store::new(Some(dir.session_path()), no_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
        assert!(!dir.session_path().exists());
    }

    #[test]
    fn wrapped_cache_without_keyring_is_kept() {
        let dir = TempDir::new();
        write_cache(
            &dir.session_path(),
            future(),
            &vault_path(&dir),
            wrapped(&[1u8; NONCE_LEN], MASTER.as_bytes()),
        );
        let store = Store::new(Some(dir.session_path()), no_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
        assert!(dir.session_path().exists());
    }

    fn assert_wrapped_payload_dropped(payload: Payload) {
        let dir = TempDir::new();
        write_cache(&dir.session_path(), future(), &vault_path(&dir), payload);
        let store = Store::new(Some(dir.session_path()), fixed_keyring);
        assert_miss(&store.load(&vault_path(&dir)));
        assert!(!dir.session_path().exists());
    }

    #[test]
    fn wrapped_cache_with_bad_nonce_encoding_is_dropped() {
        assert_wrapped_payload_dropped(Payload::Wrapped {
            nonce_b64: "!!".into(),
            ciphertext_b64: String::new(),
        });
    }

    #[test]
    fn wrapped_cache_with_short_nonce_is_dropped() {
        assert_wrapped_payload_dropped(Payload::Wrapped {
            nonce_b64: B64.encode([0u8; 4]),
            ciphertext_b64: String::new(),
        });
    }

    #[test]
    fn wrapped_cache_with_bad_ciphertext_encoding_is_dropped() {
        assert_wrapped_payload_dropped(Payload::Wrapped {
            nonce_b64: B64.encode([0u8; NONCE_LEN]),
            ciphertext_b64: "!!".into(),
        });
    }

    #[test]
    fn wrapped_cache_that_fails_to_decrypt_is_dropped() {
        assert_wrapped_payload_dropped(Payload::Wrapped {
            nonce_b64: B64.encode([0u8; NONCE_LEN]),
            ciphertext_b64: B64.encode([0u8; 32]),
        });
    }

    #[test]
    fn wrapped_cache_with_non_utf8_master_is_dropped() {
        assert_wrapped_payload_dropped(wrapped(&[2u8; NONCE_LEN], &[0xff, 0xfe, 0xfd]));
    }

    #[test]
    fn clear_removes_cache_file() {
        let dir = TempDir::new();
        let store = Store::new(Some(dir.session_path()), no_keyring);
        store.save(MASTER, &vault_path(&dir));
        assert!(dir.session_path().exists());
        store.clear();
        assert!(!dir.session_path().exists());
    }
}
