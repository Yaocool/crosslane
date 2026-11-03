//! Encrypted, owner-only daemon credentials. Reads are request-scoped, so atomic
//! replacement takes effect without a watcher or process restart.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use crosslane_core::kiro_admin::KiroAdminCredentials;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[cfg(target_os = "macos")]
mod macos_acl;

const MAGIC: &[u8] = b"CLKIRO01";
const MAX_BYTES: u64 = 65536;

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "state",
    content = "credentials",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StoredCredentials {
    Disabled,
    Environment,
    Configured(Box<KiroAdminCredentials>),
}

/// Publication is the live commit point. Directory sync separately confirms
/// crash durability; a sync failure must not imply the published record reverted.
#[derive(Debug, Clone, Copy)]
pub struct WriteStatus {
    pub durable: bool,
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    directory: PathBuf,
}

impl CredentialStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            directory: data_dir.join(".secrets"),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.directory.join("kiro-admin.enc")
    }

    pub async fn load(&self) -> Result<Option<StoredCredentials>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load_sync())
            .await
            .context("join private credential read")?
    }

    pub async fn save(&self, value: StoredCredentials) -> Result<WriteStatus> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_sync(&value))
            .await
            .context("join private credential write")?
    }

    /// Persist explicit source selection. Missing managed records must never
    /// silently reactivate old environment credentials.
    pub async fn use_environment(&self) -> Result<WriteStatus> {
        self.save(StoredCredentials::Environment).await
    }

    fn check_directory(&self) -> Result<bool> {
        let metadata = match std::fs::symlink_metadata(&self.directory) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("inspect private credential directory"),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions.
            let owner = unsafe { libc::geteuid() };
            if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o7077 != 0 {
                bail!("private credential directory must be owned by the daemon user, mode 0700, and not be a symlink");
            }
            #[cfg(target_os = "macos")]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let directory = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&self.directory)
                    .context("open private credential directory")?;
                macos_acl::ensure_private(&directory)?;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            bail!("private credential storage requires Unix file permissions");
        }
        Ok(true)
    }

    fn ensure_directory(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.directory)
                .context("create private credential directory")?;
        }
        self.check_directory()?;
        Ok(())
    }

    fn lock(&self) -> Result<File> {
        let file = private_file(&self.directory.join("kiro-admin.lock"), true, true)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                // SAFETY: file owns a live descriptor; closing it releases flock.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            }
        }
        Ok(file)
    }

    fn key(&self, create: bool) -> Result<Zeroizing<Vec<u8>>> {
        let path = self.directory.join("kiro-admin.key");
        if create {
            match private_file(&path, false, false) {
                Ok(_) => {}
                Err(error) if missing(&error) => {
                    let mut key = Zeroizing::new(vec![0u8; 32]);
                    SystemRandom::new()
                        .fill(&mut key)
                        .map_err(|_| anyhow!("credential key generation failed"))?;
                    // Writers hold the stable lock; publish a fully synced key.
                    self.write_atomic(&path, &key)?;
                }
                Err(error) => return Err(error),
            }
        }
        let key = read_private(&path)?;
        if key.len() != 32 {
            bail!("private credential encryption key is invalid");
        }
        Ok(key)
    }

    fn load_sync(&self) -> Result<Option<StoredCredentials>> {
        if !self.check_directory()? {
            return Ok(None);
        }
        let mut bytes = match read_private(&self.path()) {
            Ok(bytes) => bytes,
            Err(error) if missing(&error) => {
                match private_file(&self.directory.join("kiro-admin.key"), false, false) {
                    Ok(_) => bail!("private credential record is missing; select the credential source explicitly"),
                    Err(error) if missing(&error) => return Ok(None),
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        if !bytes.starts_with(MAGIC) || bytes.len() < MAGIC.len() + 12 + aead::AES_256_GCM.tag_len()
        {
            bail!("private credential record format is invalid");
        }
        let key = self.key(false)?;
        let cipher = cipher(&key)?;
        let nonce: [u8; 12] = bytes[MAGIC.len()..MAGIC.len() + 12]
            .try_into()
            .expect("checked nonce length");
        let plaintext = cipher
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(MAGIC),
                &mut bytes[MAGIC.len() + 12..],
            )
            .map_err(|_| anyhow!("private credential record authentication failed"))?;
        serde_json::from_slice(plaintext)
            .map(Some)
            .map_err(|_| anyhow!("private credential record contents are invalid"))
    }

    fn save_sync(&self, value: &StoredCredentials) -> Result<WriteStatus> {
        self.save_sync_with(value, sync_directory)
    }

    fn save_sync_with(
        &self,
        value: &StoredCredentials,
        sync: impl Fn(&Path) -> std::io::Result<()>,
    ) -> Result<WriteStatus> {
        self.ensure_directory()?;
        let _lock = self.lock()?;
        let key = self.key(true)?;
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow!("credential nonce generation failed"))?;
        let mut plaintext = Zeroizing::new(
            serde_json::to_vec(value).map_err(|_| anyhow!("credential encoding failed"))?,
        );
        if plaintext.len() > MAX_BYTES as usize - 64 {
            bail!("credential input exceeds the size limit");
        }
        cipher(&key)?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(MAGIC),
                &mut *plaintext,
            )
            .map_err(|_| anyhow!("credential encryption failed"))?;
        let mut envelope = Vec::with_capacity(MAGIC.len() + 12 + plaintext.len());
        envelope.extend_from_slice(MAGIC);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&plaintext);
        self.write_atomic_with(&self.path(), &envelope, sync)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<WriteStatus> {
        self.write_atomic_with(path, bytes, sync_directory)
    }

    fn write_atomic_with(
        &self,
        path: &Path,
        bytes: &[u8],
        sync: impl Fn(&Path) -> std::io::Result<()>,
    ) -> Result<WriteStatus> {
        match private_file(path, false, false) {
            Ok(_) => {}
            Err(error) if missing(&error) => {}
            Err(error) => return Err(error),
        }
        let temporary = self
            .directory
            .join(format!(".kiro-admin-{}.tmp", uuid::Uuid::new_v4()));
        let mut created = false;
        let outcome = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            }
            let mut file = options
                .open(&temporary)
                .context("create private credential temporary file")?;
            created = true;
            file.write_all(bytes)
                .context("write encrypted credential record")?;
            file.sync_all().context("sync private credential file")?;
            std::fs::rename(&temporary, path).context("publish private credential file")?;
            Ok(WriteStatus {
                durable: sync(&self.directory).is_ok(),
            })
        })();
        if created {
            let _ = std::fs::remove_file(temporary);
        }
        outcome
    }
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn cipher(key: &[u8]) -> Result<LessSafeKey> {
    UnboundKey::new(&aead::AES_256_GCM, key)
        .map(LessSafeKey::new)
        .map_err(|_| anyhow!("invalid private credential encryption key"))
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn private_file(path: &Path, writable: bool, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(writable).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path).context("open private credential file")?;
        let metadata = file.metadata()?;
        // Atomic replacement can unlink an already-open generation. Its fd
        // remains safe to read; only additional hard links are disallowed.
        // SAFETY: geteuid has no preconditions.
        let owner = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.uid() != owner
            || metadata.mode() & 0o7077 != 0
            || metadata.nlink() > 1
        {
            bail!("private credential files must be regular, owned by the daemon user, mode 0600, and not be links");
        }
        #[cfg(target_os = "macos")]
        macos_acl::ensure_private(&file)?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, options);
        bail!("private credential storage requires Unix file permissions")
    }
}

fn read_private(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let file = private_file(path, false, false)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read private credential file")?;
    if bytes.len() > MAX_BYTES as usize {
        bail!("private credential file exceeds the size limit");
    }
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn settings(secret: &str) -> StoredCredentials {
        StoredCredentials::Configured(Box::new(
            serde_json::from_value(serde_json::json!({
                "access_key_id":"AKIA000000000EXAMPLE", "secret_access_key":secret,
            }))
            .unwrap(),
        ))
    }

    #[test]
    fn directory_sync_failure_reports_a_live_commit_without_claiming_durability() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        assert!(store.save_sync(&settings("first-fixture")).unwrap().durable);
        let result = store
            .save_sync_with(&settings("second-fixture"), |_| {
                Err(std::io::Error::other("injected directory sync failure"))
            })
            .unwrap();
        assert!(!result.durable);
        let Some(StoredCredentials::Configured(value)) = store.load_sync().unwrap() else {
            panic!("missing published credential");
        };
        assert_eq!(value.secret_access_key, "second-fixture");
        assert!(
            store
                .save_sync(&StoredCredentials::Disabled)
                .unwrap()
                .durable
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn extended_acls_cannot_bypass_private_mode_bits() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        store.save(settings("private-acl-fixture")).await.unwrap();
        for path in [
            store.directory.clone(),
            store.path(),
            store.directory.join("kiro-admin.key"),
        ] {
            let permissions = if path.is_dir() {
                "list,search,readattr,readextattr,readsecurity"
            } else {
                "read,readattr,readextattr,readsecurity"
            };
            assert!(std::process::Command::new("/bin/chmod")
                .args(["+a", &format!("everyone allow {permissions}")])
                .arg(&path)
                .status()
                .unwrap()
                .success());
            let expected = if path.is_dir() { 0o700 } else { 0o600 };
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                expected
            );
            assert!(
                store.load().await.is_err(),
                "extended ACL must not be ignored"
            );
            assert!(std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(&path)
                .status()
                .unwrap()
                .success());
            assert!(store.load().await.is_ok());
            assert!(std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone deny writeextattr"])
                .arg(&path)
                .status()
                .unwrap()
                .success());
            assert!(
                store.load().await.is_ok(),
                "restrictive ACLs must remain supported"
            );
            assert!(std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(&path)
                .status()
                .unwrap()
                .success());
        }
    }

    #[tokio::test]
    async fn credentials_are_encrypted_private_durable_and_atomically_replaceable() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        assert!(store.load().await.unwrap().is_none());
        assert!(!store.directory.exists());
        store.save(settings("private-fixture-first")).await.unwrap();
        let bytes = std::fs::read(store.path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("private-fixture-first"));
        assert!(!String::from_utf8_lossy(&bytes).contains("AKIA000000000EXAMPLE"));
        assert_eq!(
            std::fs::metadata(&store.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for entry in std::fs::read_dir(&store.directory).unwrap() {
            assert_eq!(
                entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let fresh = CredentialStore::new(directory.path());
        let Some(StoredCredentials::Configured(value)) = fresh.load().await.unwrap() else {
            panic!("missing credentials");
        };
        assert_eq!(value.secret_access_key, "private-fixture-first");
        assert!(!format!("{value:?}").contains("private-fixture-first"));
        fresh
            .save(settings("private-fixture-second"))
            .await
            .unwrap();
        assert_ne!(bytes, std::fs::read(store.path()).unwrap());
        let Some(StoredCredentials::Configured(value)) = store.load().await.unwrap() else {
            panic!("missing credentials");
        };
        assert_eq!(value.secret_access_key, "private-fixture-second");
        assert!(!directory.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn clear_is_a_durable_disabled_override_until_environment_is_explicitly_selected() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        store.save(settings("private-fixture")).await.unwrap();
        store.save(StoredCredentials::Disabled).await.unwrap();
        assert!(matches!(
            store.load().await.unwrap(),
            Some(StoredCredentials::Disabled)
        ));
        store.use_environment().await.unwrap();
        assert!(matches!(
            store.load().await.unwrap(),
            Some(StoredCredentials::Environment)
        ));
        std::fs::remove_file(store.path()).unwrap();
        assert!(store.load().await.is_err());
        store.use_environment().await.unwrap();
        assert!(store.directory.join("kiro-admin.key").is_file());
    }

    #[tokio::test]
    async fn tampered_records_and_missing_keys_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        store.save(settings("private-fixture")).await.unwrap();
        let mut bytes = std::fs::read(store.path()).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(store.path(), bytes).unwrap();
        assert!(store.load().await.is_err());
        store.save(settings("private-fixture")).await.unwrap();
        std::fs::remove_file(store.directory.join("kiro-admin.key")).unwrap();
        assert!(store.load().await.is_err());
        store.save(settings("replacement-fixture")).await.unwrap();
        assert!(store.load().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn permissive_modes_and_symbolic_links_are_rejected_without_touching_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        store.save(settings("private-fixture")).await.unwrap();
        for path in [
            store.path(),
            store.directory.join("kiro-admin.key"),
            store.directory.clone(),
        ] {
            let original = std::fs::metadata(&path).unwrap().permissions();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(store.load().await.is_err());
            std::fs::set_permissions(&path, original).unwrap();
        }
        let outside = directory.path().join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(store.path()).unwrap();
        symlink(&outside, store.path()).unwrap();
        assert!(store.load().await.is_err());
        assert!(store.save(settings("replacement-fixture")).await.is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        std::fs::remove_file(store.path()).unwrap();
        std::fs::hard_link(&outside, store.path()).unwrap();
        assert!(store.load().await.is_err());
        assert!(store.save(settings("replacement-fixture")).await.is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        let other = tempfile::tempdir().unwrap();
        symlink(&store.directory, other.path().join(".secrets")).unwrap();
        assert!(CredentialStore::new(other.path()).load().await.is_err());
    }

    #[tokio::test]
    async fn concurrent_updates_always_publish_a_complete_decryptable_record() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path());
        store.save(settings("private-fixture")).await.unwrap();
        let mut tasks = Vec::new();
        for number in 0..8 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .save(settings(&format!("private-fixture-{number}")))
                    .await
                    .unwrap();
                assert!(matches!(
                    store.load().await.unwrap(),
                    Some(StoredCredentials::Configured(_))
                ));
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(store.load().await.unwrap().is_some());
    }
}
