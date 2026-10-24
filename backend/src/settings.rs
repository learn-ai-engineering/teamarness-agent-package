//! Package-owned configuration in the Package Data Directory.
//!
//! Two files: `settings.json`, an ordinary JSON document, and `api-key`, the raw key bytes in an
//! owner-only (`0600`) file. The key is read back only for the model client; no settings read,
//! status, event or error ever carries it, and the store answers `has_key` instead.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use teamarness_package_sdk::storage::{JsonDocumentStore, StorageError, write_atomic};

const SETTINGS_FILE: &str = "settings.json";
const KEY_FILE: &str = "api-key";
const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// The default OpenAI-compatible endpoint the Steward can keep or replace.
pub const DEFAULT_ENDPOINT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// How many prior Messages a reply sees when the Steward has not said otherwise.
pub const DEFAULT_CONTEXT_MESSAGES: u32 = 20;
pub const MAX_CONTEXT_MESSAGES: u32 = 64;
pub const MAX_ENDPOINT_BASE_URL: usize = 2048;
pub const MAX_MODEL: usize = 256;
pub const MAX_SYSTEM_PROMPT: usize = 16_384;
pub const MAX_API_KEY: usize = 4096;

/// What the Steward configures. This is the exact UI-contract `settings` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub endpoint_base_url: String,
    pub model: String,
    pub system_prompt: String,
    pub context_messages: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            endpoint_base_url: DEFAULT_ENDPOINT_BASE_URL.to_owned(),
            model: String::new(),
            system_prompt: String::new(),
            context_messages: DEFAULT_CONTEXT_MESSAGES,
        }
    }
}

impl Settings {
    /// Checks every field against the contract bounds and returns the first offending field.
    ///
    /// # Errors
    ///
    /// Returns the camelCase field name the UI contract uses.
    pub fn validate(&self) -> Result<(), &'static str> {
        let url = self.endpoint_base_url.trim();
        if url.is_empty()
            || url.len() > MAX_ENDPOINT_BASE_URL
            || !(url.starts_with("https://") || url.starts_with("http://"))
            || url
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err("endpointBaseUrl");
        }
        if self.model.len() > MAX_MODEL
            || self.model.trim() != self.model
            || self.model.chars().any(char::is_control)
        {
            return Err("model");
        }
        if self.system_prompt.len() > MAX_SYSTEM_PROMPT {
            return Err("systemPrompt");
        }
        if self.context_messages > MAX_CONTEXT_MESSAGES {
            return Err("contextMessages");
        }
        Ok(())
    }

    /// Trims what the Steward typed so the stored document is canonical.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.endpoint_base_url = self
            .endpoint_base_url
            .trim()
            .trim_end_matches('/')
            .to_owned();
        self
    }

    /// Whether the endpoint and model are present. The key is checked separately.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.endpoint_base_url.is_empty() && !self.model.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsDocument {
    schema_version: u32,
    #[serde(flatten)]
    settings: Settings,
}

/// Why the API key the Steward typed cannot be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// Empty after trimming, over the bound, or carrying control characters.
    Invalid,
    Io(io::ErrorKind),
}

impl From<io::Error> for KeyError {
    fn from(value: io::Error) -> Self {
        Self::Io(value.kind())
    }
}

/// The settings document and the key file of one Package Instance.
pub struct SettingsStore {
    document: JsonDocumentStore<SettingsDocument>,
    key_path: PathBuf,
}

impl SettingsStore {
    /// Opens the store, creating a default document on first use.
    ///
    /// # Errors
    ///
    /// Returns the storage failure; a document from an unknown schema version is `Corrupt`.
    pub fn open(directory: &Path) -> Result<Self, StorageError> {
        fs::create_dir_all(directory)?;
        let document = JsonDocumentStore::open_or_create(directory.join(SETTINGS_FILE), || {
            SettingsDocument {
                schema_version: SETTINGS_SCHEMA_VERSION,
                settings: Settings::default(),
            }
        })?;
        if document.get().schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(StorageError::Corrupt);
        }
        Ok(Self {
            document,
            key_path: directory.join(KEY_FILE),
        })
    }

    /// The current settings. Never includes the key.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.document.get().settings
    }

    /// Replaces the settings document; the caller has validated `settings`.
    ///
    /// # Errors
    ///
    /// Returns the write failure; the previous document is kept.
    pub fn replace(&mut self, settings: Settings) -> Result<(), StorageError> {
        self.document.replace(SettingsDocument {
            schema_version: SETTINGS_SCHEMA_VERSION,
            settings,
        })
    }

    /// Whether a non-empty key file exists.
    #[must_use]
    pub fn has_key(&self) -> bool {
        fs::metadata(&self.key_path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    }

    /// Whether the endpoint, model and key are all present.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.settings().is_complete() && self.has_key()
    }

    /// Stores the key in an owner-only file. The key is never kept in memory by the store.
    ///
    /// # Errors
    ///
    /// Returns `Invalid` for a key the bounds refuse, or the I/O failure.
    pub fn set_key(&self, api_key: &str) -> Result<(), KeyError> {
        let key = api_key.trim();
        if key.is_empty() || key.len() > MAX_API_KEY || key.chars().any(char::is_control) {
            return Err(KeyError::Invalid);
        }
        write_owner_only(&self.key_path, key.as_bytes())?;
        Ok(())
    }

    /// Removes the key file. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns the I/O failure other than the file already being absent.
    pub fn clear_key(&self) -> io::Result<()> {
        match fs::remove_file(&self.key_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Reads the key for the model client, or `None` when no key is stored.
    ///
    /// This is the only reader. Callers hold the value for one request and never put it in a
    /// response, a status, a log line or an error detail.
    ///
    /// # Errors
    ///
    /// Returns the I/O failure.
    pub fn api_key(&self) -> io::Result<Option<String>> {
        match fs::read(&self.key_path) {
            Ok(bytes) => {
                let key = String::from_utf8(bytes)
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                let key = key.trim();
                Ok((!key.is_empty()).then(|| key.to_owned()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// Writes `bytes` to `path` atomically, with the file owner-only from its first byte.
///
/// `write_atomic` opens its deterministic `<path>.tmp` with the default mode, so the temporary
/// file is created here first with `0600`; `write_atomic` then reuses it, and the rename keeps
/// the mode. The final `set_permissions` covers a pre-existing temporary file.
fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(&temporary)?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    }
    write_atomic(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_incomplete_and_validation_names_the_field() {
        let settings = Settings::default();
        assert!(settings.validate().is_ok());
        assert!(!settings.is_complete());
        let mut bad = settings.clone();
        bad.endpoint_base_url = "ftp://x".into();
        assert_eq!(bad.validate(), Err("endpointBaseUrl"));
        let mut bad = settings.clone();
        bad.model = "a\tb".into();
        assert_eq!(bad.validate(), Err("model"));
        let mut bad = settings.clone();
        bad.system_prompt = "x".repeat(MAX_SYSTEM_PROMPT + 1);
        assert_eq!(bad.validate(), Err("systemPrompt"));
        let mut bad = settings;
        bad.context_messages = MAX_CONTEXT_MESSAGES + 1;
        assert_eq!(bad.validate(), Err("contextMessages"));
        let trimmed = Settings {
            endpoint_base_url: " https://example.test/v1/ ".into(),
            ..Settings::default()
        }
        .normalized();
        assert_eq!(trimmed.endpoint_base_url, "https://example.test/v1");
    }

    #[test]
    fn key_file_round_trip_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let store = SettingsStore::open(directory.path()).unwrap();
        assert!(!store.has_key());
        assert_eq!(store.api_key().unwrap(), None);
        assert_eq!(store.set_key("  "), Err(KeyError::Invalid));
        assert_eq!(store.set_key("a\u{7}b"), Err(KeyError::Invalid));
        assert_eq!(
            store.set_key(&"k".repeat(MAX_API_KEY + 1)),
            Err(KeyError::Invalid)
        );
        store.set_key(" sk-test ").unwrap();
        assert!(store.has_key());
        assert_eq!(store.api_key().unwrap().as_deref(), Some("sk-test"));
        store.clear_key().unwrap();
        store.clear_key().unwrap();
        assert!(!store.has_key());
    }

    #[test]
    fn unknown_schema_version_is_corrupt() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(SETTINGS_FILE),
            br#"{"schemaVersion":2,"endpointBaseUrl":"https://x","model":"","systemPrompt":"","contextMessages":1}"#,
        )
        .unwrap();
        assert_eq!(
            SettingsStore::open(directory.path()).err(),
            Some(StorageError::Corrupt)
        );
    }
}
