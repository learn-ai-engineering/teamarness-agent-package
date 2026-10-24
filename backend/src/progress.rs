//! Durable delivery progress, independent of Message display order.
//!
//! Each Channel acknowledges an examined prefix of one Conversations projection epoch.
//! A trigger is acknowledged before its model/post attempt and is never replayed afterward.
//! Callers run file operations through the SDK blocking-work seam.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use teamarness_package_sdk::storage::{JsonDocumentStore, StorageError};

const FILE: &str = "progress.json";
const SCHEMA_VERSION: u32 = 1;

/// One Channel's acknowledged projection revision, not a Message OrderKey.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub epoch: String,
    pub after: u64,
}

impl Cursor {
    #[must_use]
    pub fn new(epoch: String, after: u64) -> Option<Self> {
        (epoch.len() == 32
            && epoch
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && after <= i64::MAX as u64)
            .then_some(Self { epoch, after })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Document {
    schema_version: u32,
    channels: BTreeMap<String, Cursor>,
}

/// Atomic progress for one Package activation, shared by its resumed harness tasks.
pub struct Progress {
    document: JsonDocumentStore<Document>,
}

impl Progress {
    /// Opens or creates progress. No legacy sort-position conversion is performed.
    ///
    /// # Errors
    /// Returns storage failure or `Corrupt` for an unsupported/invalid document.
    pub fn open(directory: &Path) -> Result<Self, StorageError> {
        std::fs::create_dir_all(directory)?;
        let document = JsonDocumentStore::open_or_create(directory.join(FILE), || Document {
            schema_version: SCHEMA_VERSION,
            channels: BTreeMap::new(),
        })?;
        if document.get().schema_version != SCHEMA_VERSION
            || document
                .get()
                .channels
                .values()
                .any(|cursor| Cursor::new(cursor.epoch.clone(), cursor.after).is_none())
        {
            return Err(StorageError::Corrupt);
        }
        Ok(Self { document })
    }

    #[must_use]
    pub fn cursor(&self, channel: &str) -> Option<&Cursor> {
        self.document.get().channels.get(channel)
    }

    /// Persists a monotonic acknowledgment within the same epoch.
    ///
    /// # Errors
    /// Returns storage failure; refuses regressions or implicit epoch resets as `Corrupt`.
    pub fn advance(&mut self, channel: &str, cursor: Cursor) -> Result<(), StorageError> {
        if Cursor::new(cursor.epoch.clone(), cursor.after).is_none() {
            return Err(StorageError::Corrupt);
        }
        if let Some(previous) = self.cursor(channel) {
            if previous.epoch != cursor.epoch || previous.after > cursor.after {
                return Err(StorageError::Corrupt);
            }
            if previous == &cursor {
                return Ok(());
            }
        }
        self.document.update(|document| {
            document.channels.insert(channel.to_owned(), cursor);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_persists_revisions_and_refuses_implicit_replay() {
        let directory = tempfile::tempdir().unwrap();
        let mut progress = Progress::open(directory.path()).unwrap();
        let cursor = Cursor::new("a".repeat(32), 42).unwrap();
        progress.advance("channel", cursor.clone()).unwrap();
        assert_eq!(
            Progress::open(directory.path()).unwrap().cursor("channel"),
            Some(&cursor)
        );
        assert!(
            progress
                .advance("channel", Cursor::new("a".repeat(32), 41).unwrap())
                .is_err()
        );
        assert!(
            progress
                .advance("channel", Cursor::new("b".repeat(32), 43).unwrap())
                .is_err()
        );
        assert_eq!(progress.cursor("channel"), Some(&cursor));
        assert!(Cursor::new("wrong".into(), 0).is_none());
        assert!(Cursor::new("a".repeat(32), u64::MAX).is_none());
    }

    #[test]
    fn unsupported_progress_fails_closed_without_modification() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE);
        let bytes = br#"{"schemaVersion":99,"channels":{}}"#;
        std::fs::write(&path, bytes).unwrap();
        assert!(Progress::open(directory.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
