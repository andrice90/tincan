//! Profile directory layout: `tincan.db`, `identity.key`, `blobs/`, `tincan.lock`.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tincan_core::keys::Identity;
use tincan_core::storage::Storage;

/// SQLite file name.
pub(crate) const DB_FILE: &str = "tincan.db";
/// Secret key file name (32 bytes).
pub(crate) const KEY_FILE: &str = "identity.key";
/// Blob store directory.
pub(crate) const BLOBS_DIR: &str = "blobs";
/// Lock file name.
pub(crate) const LOCK_FILE: &str = "tincan.lock";

/// kv key of the display name (UTF-8).
const NAME_KEY: &str = "app/display_name";

/// User-facing error when the profile is open elsewhere.
pub(crate) const ALREADY_RUNNING: &str = "Tincan is already running with this profile.";

/// The exclusively held lock file. Released when dropped.
#[derive(Debug)]
pub(crate) struct ProfileLock {
    _file: File,
}

/// Create the profile directory and take its lock.
pub(crate) fn lock(dir: &Path) -> Result<ProfileLock, String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        tracing::error!(dir = %dir.display(), error = %e, "creating the profile directory failed");
        "Couldn't create the Tincan profile folder. Check that the disk is writable.".to_string()
    })?;
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(|e| {
        tracing::error!(path = %path.display(), error = %e, "opening the lock file failed");
        // A sharing violation also lands here on Windows.
        ALREADY_RUNNING.to_string()
    })?;
    match file.try_lock() {
        Ok(()) => Ok(ProfileLock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(ALREADY_RUNNING.to_string()),
        Err(TryLockError::Error(e)) => {
            tracing::error!(path = %path.display(), error = %e, "locking the profile failed");
            Err("Couldn't lock the Tincan profile folder.".to_string())
        }
    }
}

/// Load `identity.key`, or generate and store a new one on first start.
pub(crate) fn identity(dir: &Path) -> Result<Identity, String> {
    let path: PathBuf = dir.join(KEY_FILE);
    match File::open(&path) {
        Ok(mut f) => {
            let mut buf = Vec::with_capacity(33);
            f.read_to_end(&mut buf).map_err(|e| {
                tracing::error!(error = %e, "reading the identity key failed");
                "Couldn't read your Tincan identity.".to_string()
            })?;
            let bytes: [u8; 32] = buf
                .as_slice()
                .try_into()
                .map_err(|_| "Your Tincan identity file is damaged. Move it out of the profile folder to start fresh.".to_string())?;
            Ok(Identity::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = Identity::generate();
            let tmp = dir.join(format!("{KEY_FILE}.tmp"));
            let write = || -> std::io::Result<()> {
                let mut f = File::create(&tmp)?;
                f.write_all(&id.to_bytes())?;
                f.sync_all()?;
                std::fs::rename(&tmp, &path)
            };
            write().map_err(|e| {
                tracing::error!(error = %e, "writing the identity key failed");
                "Couldn't save your Tincan identity. Check that the disk is writable.".to_string()
            })?;
            tracing::info!(user = %id.user().short(), "generated a new identity");
            Ok(id)
        }
        Err(e) => {
            tracing::error!(error = %e, "opening the identity key failed");
            Err("Couldn't read your Tincan identity.".to_string())
        }
    }
}

/// The stored display name.
pub(crate) fn load_name(storage: &dyn Storage) -> Option<String> {
    match storage.get_kv(NAME_KEY) {
        Ok(Some(v)) => String::from_utf8(v).ok().filter(|s| !s.trim().is_empty()),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "reading the display name failed");
            None
        }
    }
}

/// Persist the display name.
pub(crate) fn save_name(storage: &dyn Storage, name: &str) -> Result<(), String> {
    storage.set_kv(NAME_KEY, name.as_bytes()).map_err(|e| {
        tracing::error!(error = %e, "saving the display name failed");
        "Couldn't save your name. Try again.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let l = lock(dir.path()).unwrap();
        assert_eq!(lock(dir.path()).unwrap_err(), ALREADY_RUNNING);
        drop(l);
        assert!(lock(dir.path()).is_ok());
    }

    #[test]
    fn identity_is_generated_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = identity(dir.path()).unwrap();
        let b = identity(dir.path()).unwrap();
        assert_eq!(a.user(), b.user());
        std::fs::write(dir.path().join(KEY_FILE), b"short").unwrap();
        assert!(identity(dir.path()).is_err());
    }

    #[test]
    fn name_round_trip() {
        let st = tincan_core::storage::MemStorage::new();
        assert_eq!(load_name(&st), None);
        save_name(&st, "Sam").unwrap();
        assert_eq!(load_name(&st).as_deref(), Some("Sam"));
    }
}
