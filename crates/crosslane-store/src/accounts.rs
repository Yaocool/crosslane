//! 账号库读写。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use crosslane_core::account::Account;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

use crate::atomic::{is_missing, read_to_string_with_retry, write_bytes_atomically};

const ACCOUNTS_FILE_MODE: u32 = 0o600;
const COMPRESSION_THRESHOLD: usize = 100;

#[derive(Serialize, Deserialize)]
struct CompressedEnvelope {
    __compressed: bool,
    data: String,
}

#[derive(Serialize, Deserialize)]
struct AccountDelta {
    id: String,
    account: Option<Account>,
}

/// 内存中的账号库，落盘为单个 JSON 数组。
#[derive(Debug, Clone)]
pub struct AccountStore {
    path: PathBuf,
    accounts: Vec<Account>,
    compression_threshold: usize,
    incremental_write: bool,
    dirty: BTreeMap<String, Option<Account>>,
}

impl AccountStore {
    /// 从磁盘加载；文件不存在视为空库，损坏则返回错误。
    pub async fn load(path: &Path) -> Result<Self> {
        let raw = match read_to_string_with_retry(path).await {
            Ok(raw) => raw,
            Err(error) if is_missing(&error) => {
                return Ok(Self {
                    path: path.to_path_buf(),
                    accounts: Vec::new(),
                    compression_threshold: COMPRESSION_THRESHOLD,
                    incremental_write: true,
                    dirty: BTreeMap::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let mut accounts = if raw.trim().is_empty() {
            Vec::new()
        } else {
            decode_accounts(raw.trim()).with_context(|| format!("parse {}", path.display()))?
        };
        apply_deltas(path, &mut accounts).await?;
        Ok(Self {
            path: path.to_path_buf(),
            accounts,
            compression_threshold: COMPRESSION_THRESHOLD,
            incremental_write: true,
            dirty: BTreeMap::new(),
        })
    }

    /// 原子写回磁盘。
    pub async fn save(&mut self) -> Result<()> {
        if self.incremental_write && self.path.exists() && self.dirty.is_empty() {
            return Ok(());
        }
        let compressed_base = if self.incremental_write
            && self.accounts.len() > self.compression_threshold
            && self.path.exists()
        {
            read_to_string_with_retry(&self.path)
                .await
                .is_ok_and(|raw| raw.contains("\"__compressed\""))
        } else {
            false
        };
        if compressed_base && !self.dirty.is_empty() {
            let compact_at = self.compression_threshold.max(1);
            if self.dirty.len() < compact_at {
                let directory = delta_directory(&self.path);
                tokio::fs::create_dir_all(&directory).await?;
                for (id, account) in &self.dirty {
                    let delta = AccountDelta {
                        id: id.clone(),
                        account: account.clone(),
                    };
                    crate::atomic::write_json_atomically(
                        &directory.join(delta_file_name(id)),
                        &delta,
                        Some(ACCOUNTS_FILE_MODE),
                    )
                    .await?;
                }
                self.dirty.clear();
                return Ok(());
            }
        }
        let accounts = self.accounts.clone();
        let threshold = self.compression_threshold;
        let bytes = tokio::task::spawn_blocking(move || encode_accounts(&accounts, threshold))
            .await
            .context("account serialization worker failed")??;
        write_bytes_atomically(&self.path, &bytes, Some(ACCOUNTS_FILE_MODE)).await?;
        clear_deltas(&self.path).await?;
        self.dirty.clear();
        Ok(())
    }

    /// 全部账号。
    pub fn all(&self) -> &[Account] {
        &self.accounts
    }

    /// Applies the configured compression threshold to future saves.
    pub fn set_compression_threshold(&mut self, threshold: usize) {
        self.compression_threshold = threshold;
    }

    pub fn set_incremental_write(&mut self, enabled: bool) {
        self.incremental_write = enabled;
    }

    /// 账号数量。
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// 按 ID 或邮箱查找。
    pub fn find(&self, id_or_email: &str) -> Option<&Account> {
        self.accounts
            .iter()
            .find(|account| account.id == id_or_email || account.email == id_or_email)
    }

    /// 插入账号；ID、邮箱或 Kiro 用户 ID 重复时报错。
    pub fn insert(&mut self, account: Account) -> Result<()> {
        if self
            .accounts
            .iter()
            .any(|existing| existing.id == account.id)
        {
            return Err(anyhow!("account {} already exists", account.id));
        }
        if self
            .accounts
            .iter()
            .any(|existing| existing.email == account.email)
        {
            return Err(anyhow!("account email {} already exists", account.email));
        }
        if let Some(user_id) = account
            .upstream_user_id
            .as_deref()
            .map(str::trim)
            .filter(|user_id| !user_id.is_empty())
        {
            if let Some(existing) = self.accounts.iter().find(|existing| {
                existing.upstream_user_id.as_deref().map(str::trim) == Some(user_id)
            }) {
                return Err(anyhow!(
                    "Kiro identity is already registered as {}",
                    existing.email
                ));
            }
        }
        self.dirty.insert(account.id.clone(), Some(account.clone()));
        self.accounts.push(account);
        Ok(())
    }

    /// 按 ID 或邮箱删除。
    pub fn remove(&mut self, id_or_email: &str) -> Option<Account> {
        let index = self
            .accounts
            .iter()
            .position(|account| account.id == id_or_email || account.email == id_or_email)?;
        let account = self.accounts.remove(index);
        self.dirty.insert(account.id.clone(), None);
        Some(account)
    }

    /// 就地修改，命中返回 true。
    pub fn update<F>(&mut self, id_or_email: &str, mutate: F) -> bool
    where
        F: FnOnce(&mut Account),
    {
        match self
            .accounts
            .iter_mut()
            .find(|account| account.id == id_or_email || account.email == id_or_email)
        {
            Some(account) => {
                mutate(account);
                self.dirty.insert(account.id.clone(), Some(account.clone()));
                true
            }
            None => false,
        }
    }

    /// Replace a runtime snapshot only when its serialized account data changed.
    pub fn replace_if_changed(&mut self, account: Account) -> bool {
        let Some(index) = self
            .accounts
            .iter()
            .position(|stored| stored.id == account.id)
        else {
            return false;
        };
        let unchanged =
            serde_json::to_value(&self.accounts[index]).ok() == serde_json::to_value(&account).ok();
        if unchanged {
            return false;
        }
        self.accounts[index] = account.clone();
        self.dirty.insert(account.id.clone(), Some(account));
        true
    }

    /// 按标签过滤。
    pub fn filter_by_tag(&self, tag: &str) -> Vec<&Account> {
        self.accounts
            .iter()
            .filter(|account| account.tags.iter().any(|value| value == tag))
            .collect()
    }
}

fn delta_directory(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".d");
    PathBuf::from(value)
}

fn delta_file_name(id: &str) -> String {
    let safe = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{safe}.json")
}

async fn apply_deltas(path: &Path, accounts: &mut Vec<Account>) -> Result<()> {
    let directory = delta_directory(path);
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort();
    for delta_path in paths {
        let raw = read_to_string_with_retry(&delta_path).await?;
        let delta: AccountDelta = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", delta_path.display()))?;
        accounts.retain(|account| account.id != delta.id);
        if let Some(account) = delta.account {
            accounts.push(account);
        }
    }
    Ok(())
}

async fn clear_deltas(path: &Path) -> Result<()> {
    let directory = delta_directory(path);
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_file() {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

fn encode_accounts(accounts: &[Account], threshold: usize) -> Result<Vec<u8>> {
    let json = serde_json::to_vec_pretty(accounts)?;
    if threshold == 0 || accounts.len() <= threshold {
        return Ok(json);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&json)?;
    let compressed = encoder.finish()?;
    Ok(serde_json::to_vec_pretty(&CompressedEnvelope {
        __compressed: true,
        data: base64::engine::general_purpose::STANDARD.encode(compressed),
    })?)
}

fn decode_accounts(raw: &str) -> Result<Vec<Account>> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    if value
        .get("__compressed")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return serde_json::from_value(value).map_err(Into::into);
    }
    let envelope: CompressedEnvelope = serde_json::from_value(value)?;
    let compressed = base64::engine::general_purpose::STANDARD.decode(envelope.data)?;
    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut json = Vec::new();
    decoder.read_to_end(&mut json)?;
    serde_json::from_slice(&json).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use crosslane_core::account::{Account, AuthMethod, Credentials};
    use tempfile::tempdir;

    use super::*;

    fn account(id: &str, email: &str) -> Account {
        Account {
            id: id.into(),
            email: email.into(),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "secret".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: 0,
                auth_method: AuthMethod::Idc,
            },
            usage: None,
            subscription: None,
            tags: vec!["prod".into()],
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[tokio::test]
    async fn missing_loads_empty_and_roundtrips() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("accounts.json");
        let mut store = AccountStore::load(&path).await.expect("load empty");
        assert!(store.is_empty());
        store
            .insert(account("acc_00000001", "a@example.com"))
            .expect("insert");
        store.save().await.expect("save");
        let loaded = AccountStore::load(&path).await.expect("reload");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.find("a@example.com").expect("find").id,
            "acc_00000001"
        );
        assert_eq!(loaded.filter_by_tag("prod").len(), 1);
    }

    #[tokio::test]
    async fn duplicate_remove_and_update_behave_consistently() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("accounts.json");
        let mut store = AccountStore::load(&path).await.expect("load");
        store
            .insert(account("acc_00000001", "a@example.com"))
            .expect("insert");
        assert!(store
            .insert(account("acc_00000001", "b@example.com"))
            .is_err());
        assert!(store
            .insert(account("acc_00000002", "a@example.com"))
            .is_err());
        assert!(store.update("a@example.com", |value| value.enabled = false));
        assert!(!store.find("a@example.com").expect("find").enabled);
        assert_eq!(
            store.remove("a@example.com").expect("remove").id,
            "acc_00000001"
        );
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn duplicate_kiro_user_id_is_rejected_even_for_a_different_email() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("accounts.json");
        let mut store = AccountStore::load(&path).await.expect("load");
        let mut first = account("acc_00000001", "a@example.com");
        first.upstream_user_id = Some("kiro-user-1".into());
        store.insert(first).expect("insert first identity");

        let mut duplicate = account("acc_00000002", "b@example.com");
        duplicate.upstream_user_id = Some("kiro-user-1".into());
        let error = store
            .insert(duplicate)
            .expect_err("duplicate Kiro identity must be rejected");
        assert!(error.to_string().contains("a@example.com"));
    }

    #[tokio::test]
    async fn corrupt_file_is_not_silently_treated_as_empty() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("accounts.json");
        tokio::fs::write(&path, "{not json").await.expect("write");
        let error = AccountStore::load(&path).await.expect_err("corrupt");
        assert!(error.to_string().contains("accounts.json"));
    }

    #[tokio::test]
    async fn large_account_sets_use_a_gzip_envelope_and_roundtrip() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("accounts.json");
        let mut store = AccountStore::load(&path).await.expect("load");
        for index in 0..=COMPRESSION_THRESHOLD {
            store
                .insert(account(
                    &format!("acc_{index:08}"),
                    &format!("user-{index}@example.com"),
                ))
                .expect("insert");
        }
        store.save().await.expect("save");
        let raw = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(raw.contains("\"__compressed\": true"));
        let loaded = AccountStore::load(&path).await.expect("reload");
        assert_eq!(loaded.len(), COMPRESSION_THRESHOLD + 1);
    }

    #[tokio::test]
    async fn a_large_store_persists_single_account_changes_as_deltas() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("accounts.json");
        let mut store = AccountStore::load(&path).await.expect("load");
        store.set_compression_threshold(2);
        for index in 0..3 {
            store
                .insert(account(
                    &format!("acc_{index:08}"),
                    &format!("user-{index}@example.com"),
                ))
                .expect("insert");
        }
        store.save().await.expect("base save");
        let base = tokio::fs::read(&path).await.expect("base");

        let mut store = AccountStore::load(&path).await.expect("reload");
        store.set_compression_threshold(2);
        assert!(store.update("acc_00000001", |account| account.label =
            Some("changed".into())));
        store.save().await.expect("delta save");
        assert_eq!(tokio::fs::read(&path).await.expect("same base"), base);
        let delta = delta_directory(&path).join("acc_00000001.json");
        assert!(delta.exists());

        let loaded = AccountStore::load(&path).await.expect("delta reload");
        assert_eq!(
            loaded
                .find("acc_00000001")
                .and_then(|account| account.label.as_deref()),
            Some("changed")
        );
    }
}
