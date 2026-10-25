//! The Steward's Package enablement, in `packages.json` in the Package Data Directory.
//!
//! The document is `{schemaVersion: 1, enabled: [contractIdentity]}`. A missing file means
//! nothing is enabled. Identities the Agent no longer knows are kept, so a catalog that loses a
//! contract and later regains it keeps the Steward's choice. [`crate::access`] decides what an
//! enabled identity means; this store only remembers the choice.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use teamarness_package_sdk::storage::{JsonDocumentStore, StorageError};

const PACKAGES_FILE: &str = "packages.json";
const PACKAGES_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackagesDocument {
    schema_version: u32,
    enabled: Vec<String>,
}

/// The enabled contract identities of one Package Instance.
pub struct PackagesStore {
    document: JsonDocumentStore<PackagesDocument>,
}

impl PackagesStore {
    /// Opens the store, creating an empty document on first use.
    ///
    /// # Errors
    ///
    /// Returns the storage failure; an unreadable document, one with unknown fields or one from
    /// an unknown schema version is `Corrupt`.
    pub fn open(directory: &Path) -> Result<Self, StorageError> {
        fs::create_dir_all(directory)?;
        let document = JsonDocumentStore::open_or_create(directory.join(PACKAGES_FILE), || {
            PackagesDocument {
                schema_version: PACKAGES_SCHEMA_VERSION,
                enabled: Vec::new(),
            }
        })?;
        if document.get().schema_version != PACKAGES_SCHEMA_VERSION {
            return Err(StorageError::Corrupt);
        }
        Ok(Self { document })
    }

    /// The enabled contract identities, including ones the catalog no longer has.
    #[must_use]
    pub fn enabled(&self) -> &[String] {
        &self.document.get().enabled
    }

    /// Enables or disables `identity`, keeping every other identity.
    ///
    /// # Errors
    ///
    /// Returns the write failure; the previous document is kept.
    pub fn set(&mut self, identity: &str, enabled: bool) -> Result<(), StorageError> {
        self.document.update(|document| {
            document.enabled.retain(|existing| existing != identity);
            if enabled {
                document.enabled.push(identity.to_owned());
            }
            document.enabled.sort();
            document.enabled.dedup();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_enables_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let store = PackagesStore::open(directory.path()).unwrap();
        assert!(store.enabled().is_empty());
    }

    #[test]
    fn unknown_identities_survive_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(PACKAGES_FILE),
            br#"{"schemaVersion":1,"enabled":["com.example.gone"]}"#,
        )
        .unwrap();
        let mut store = PackagesStore::open(directory.path()).unwrap();
        store.set("dev.teamarness.exhibits", true).unwrap();
        store.set("dev.teamarness.exhibits", true).unwrap();
        drop(store);

        let mut store = PackagesStore::open(directory.path()).unwrap();
        assert_eq!(
            store.enabled(),
            ["com.example.gone", "dev.teamarness.exhibits"]
        );
        store.set("dev.teamarness.exhibits", false).unwrap();
        drop(store);
        let store = PackagesStore::open(directory.path()).unwrap();
        assert_eq!(store.enabled(), ["com.example.gone"]);
    }

    #[test]
    fn invalid_file_is_storage_failed() {
        for bytes in [
            &b"not json"[..],
            br#"{"schemaVersion":2,"enabled":[]}"#,
            br#"{"schemaVersion":1,"enabled":[],"extra":true}"#,
            br#"{"schemaVersion":1}"#,
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::write(directory.path().join(PACKAGES_FILE), bytes).unwrap();
            assert_eq!(
                PackagesStore::open(directory.path()).err(),
                Some(StorageError::Corrupt),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }
}
