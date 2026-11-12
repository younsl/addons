use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crypto::{KdfParams, Sealed, derive_key, open, seal};
use crate::error::{VltError, VltResult};
use crate::vault::item::Item;

const VAULT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct VaultFile {
    pub version: u32,
    pub kdf: KdfParams,
    #[serde(flatten)]
    pub sealed: Sealed,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct VaultPayload {
    #[serde(default)]
    pub items: Vec<Item>,
}

pub struct Vault {
    path: PathBuf,
    file: VaultFile,
    payload: VaultPayload,
    password: String,
}

impl Vault {
    pub fn create(path: &Path, master_password: &str) -> VltResult<Self> {
        if path.exists() {
            return Err(VltError::AlreadyInitialized);
        }
        if master_password.len() < 8 {
            return Err(VltError::InvalidInput(
                "master password must be at least 8 characters".into(),
            ));
        }
        let kdf = KdfParams::new_random();
        let key = derive_key(master_password, &kdf)?;
        let payload = VaultPayload::default();
        let sealed = seal(&key, &serde_json::to_vec(&payload)?)?;
        let file = VaultFile {
            version: VAULT_VERSION,
            kdf,
            sealed,
        };
        write_atomic(path, &file)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            payload,
            password: master_password.to_string(),
        })
    }

    pub fn open_vault(path: &Path, master_password: &str) -> VltResult<Self> {
        if !path.exists() {
            return Err(VltError::NotInitialized);
        }
        let raw = fs::read(path)?;
        let file: VaultFile = serde_json::from_slice(&raw)?;
        if file.version != VAULT_VERSION {
            return Err(VltError::InvalidInput(format!(
                "unsupported vault version: {}",
                file.version
            )));
        }
        let key = derive_key(master_password, &file.kdf)?;
        let plain = open(&key, &file.sealed)?;
        let payload: VaultPayload = serde_json::from_slice(&plain)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            payload,
            password: master_password.to_string(),
        })
    }

    pub fn items(&self) -> &[Item] {
        &self.payload.items
    }

    pub fn find_item(&self, id: &str) -> VltResult<&Item> {
        self.payload
            .items
            .iter()
            .find(|i| i.id == id)
            .ok_or_else(|| VltError::ItemNotFound(id.to_string()))
    }

    pub fn add_item(&mut self, item: Item) {
        self.payload.items.push(item);
    }

    pub fn update_item<F>(&mut self, id: &str, f: F) -> VltResult<&Item>
    where
        F: FnOnce(&mut Item),
    {
        let it = self
            .payload
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| VltError::ItemNotFound(id.to_string()))?;
        f(it);
        Ok(it)
    }

    pub fn delete_item(&mut self, id: &str) -> VltResult<()> {
        let len_before = self.payload.items.len();
        self.payload.items.retain(|i| i.id != id);
        if self.payload.items.len() == len_before {
            return Err(VltError::ItemNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn persist(&mut self) -> VltResult<()> {
        let key = derive_key(&self.password, &self.file.kdf)?;
        let plain = serde_json::to_vec(&self.payload)?;
        let sealed = seal(&key, &plain)?;
        self.file.sealed = sealed;
        write_atomic(&self.path, &self.file)
    }
}

fn write_atomic(path: &Path, file: &VaultFile) -> VltResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(file)?;
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, master, new_item, random_secret};
    use crate::vault::item::Item;

    #[test]
    fn create_then_open_roundtrip() {
        let dir = TempDir::new();
        let path = dir.vault_path();
        let mut vault = Vault::create(&path, master()).unwrap();
        assert!(vault.items().is_empty());
        vault.add_item(Item::from_new(new_item("Alpha", "")));
        vault.persist().unwrap();

        let reopened = Vault::open_vault(&path, master()).unwrap();
        assert_eq!(reopened.items().len(), 1);
        assert_eq!(reopened.items()[0].title, "Alpha");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn create_creates_missing_parent_dirs() {
        let dir = TempDir::new();
        let path = dir.path().join("a").join("b").join("vault.json");
        Vault::create(&path, master()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn create_rejects_existing_vault() {
        let dir = TempDir::new();
        Vault::create(&dir.vault_path(), master()).unwrap();
        assert!(matches!(
            Vault::create(&dir.vault_path(), master()),
            Err(VltError::AlreadyInitialized)
        ));
    }

    #[test]
    fn create_rejects_short_password() {
        let dir = TempDir::new();
        assert!(matches!(
            Vault::create(&dir.vault_path(), &random_secret(5)),
            Err(VltError::InvalidInput(_))
        ));
        assert!(!dir.vault_path().exists());
    }

    #[test]
    fn open_missing_vault_is_not_initialized() {
        let dir = TempDir::new();
        assert!(matches!(
            Vault::open_vault(&dir.vault_path(), master()),
            Err(VltError::NotInitialized)
        ));
    }

    #[test]
    fn open_with_wrong_password_fails() {
        let dir = TempDir::new();
        Vault::create(&dir.vault_path(), master()).unwrap();
        assert!(matches!(
            Vault::open_vault(&dir.vault_path(), &random_secret(16)),
            Err(VltError::InvalidMasterPassword)
        ));
    }

    #[test]
    fn open_rejects_unknown_version() {
        let dir = TempDir::new();
        let path = dir.vault_path();
        Vault::create(&path, master()).unwrap();
        let mut file: VaultFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        file.version = 99;
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(matches!(
            Vault::open_vault(&path, master()),
            Err(VltError::InvalidInput(msg)) if msg.contains("99")
        ));
    }

    #[test]
    fn open_rejects_corrupt_json() {
        let dir = TempDir::new();
        fs::write(dir.vault_path(), b"not json").unwrap();
        assert!(matches!(
            Vault::open_vault(&dir.vault_path(), master()),
            Err(VltError::Serde(_))
        ));
    }

    #[test]
    fn find_update_and_delete_items() {
        let dir = TempDir::new();
        let mut vault = Vault::create(&dir.vault_path(), master()).unwrap();
        let item = Item::from_new(new_item("Alpha", "g"));
        let id = item.id.clone();
        vault.add_item(item);

        assert_eq!(vault.find_item(&id).unwrap().title, "Alpha");
        let updated = vault
            .update_item(&id, |it| it.title = "Beta".into())
            .unwrap();
        assert_eq!(updated.title, "Beta");

        vault.delete_item(&id).unwrap();
        assert!(vault.items().is_empty());
    }

    #[test]
    fn missing_item_errors() {
        let dir = TempDir::new();
        let mut vault = Vault::create(&dir.vault_path(), master()).unwrap();
        assert!(matches!(
            vault.find_item("x"),
            Err(VltError::ItemNotFound(_))
        ));
        assert!(matches!(
            vault.update_item("x", |_| {}),
            Err(VltError::ItemNotFound(_))
        ));
        assert!(matches!(
            vault.delete_item("x"),
            Err(VltError::ItemNotFound(_))
        ));
    }

    #[test]
    fn payload_without_items_defaults_to_empty() {
        let payload: VaultPayload = serde_json::from_str("{}").unwrap();
        assert!(payload.items.is_empty());
    }
}
