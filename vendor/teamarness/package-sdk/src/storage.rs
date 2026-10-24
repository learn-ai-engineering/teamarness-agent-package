//! Durable local-file helpers for Packages.
//!
//! These are ordinary synchronous file operations on the Package's own Data Directory, not a
//! host-proxied store, so they are called inside [`crate::blocking`] or `Context::blocking`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why a stored document could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageError {
    Io(io::ErrorKind),
    /// The file exists but does not parse as the document type.
    Corrupt,
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(kind) => write!(formatter, "io: {kind:?}"),
            Self::Corrupt => formatter.write_str("corrupt"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<io::Error> for StorageError {
    fn from(value: io::Error) -> Self {
        Self::Io(value.kind())
    }
}

/// Replaces `path` with `bytes`, or leaves the previous contents untouched.
///
/// The bytes reach the disk before the rename, and on Unix the directory entry is flushed too, so
/// a crash leaves either the old file or the new one. The temporary name is deterministic, so
/// callers serialize writes to one path.
///
/// # Errors
///
/// Returns the first I/O failure. The temporary file is removed on failure when it is a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(path);
    match write_and_rename(&temporary, path, parent, bytes) {
        Ok(()) => Ok(()),
        Err(error) => {
            if fs::symlink_metadata(&temporary).is_ok_and(|metadata| metadata.is_file()) {
                let _ = fs::remove_file(&temporary);
            }
            Err(error)
        }
    }
}

fn write_and_rename(temporary: &Path, path: &Path, parent: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    #[cfg(unix)]
    {
        // The rename itself is durable only once the directory entry is flushed.
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// One JSON document kept in memory and replaced atomically on disk.
///
/// The in-memory value changes only after a successful write, so a failed write leaves the store
/// and the file agreeing with each other.
#[derive(Debug)]
pub struct JsonDocumentStore<T> {
    path: PathBuf,
    value: T,
}

impl<T: Serialize + DeserializeOwned + Clone> JsonDocumentStore<T> {
    /// Opens the document, creating it from `create` when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Corrupt`] when the file exists but does not parse, or an I/O error.
    pub fn open_or_create(
        path: impl Into<PathBuf>,
        create: impl FnOnce() -> T,
    ) -> Result<Self, StorageError> {
        let path = path.into();
        match fs::read(&path) {
            Ok(bytes) => {
                let value = serde_json::from_slice(&bytes).map_err(|_| StorageError::Corrupt)?;
                Ok(Self { path, value })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let value = create();
                write_atomic(&path, &encode(&value)?)?;
                Ok(Self { path, value })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The current value.
    pub const fn get(&self) -> &T {
        &self.value
    }

    /// The document's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `next` and, only if that succeeds, adopts it.
    ///
    /// # Errors
    ///
    /// Returns the write failure; the previous value and file are unchanged.
    pub fn replace(&mut self, next: T) -> Result<(), StorageError> {
        write_atomic(&self.path, &encode(&next)?)?;
        self.value = next;
        Ok(())
    }

    /// Applies `change` to a copy, writes it, and only then adopts it.
    ///
    /// # Errors
    ///
    /// Returns the change's error or the write failure; either way the stored value and the file
    /// are unchanged.
    pub fn update<R>(
        &mut self,
        change: impl FnOnce(&mut T) -> Result<R, StorageError>,
    ) -> Result<R, StorageError> {
        let mut next = self.value.clone();
        let outcome = change(&mut next)?;
        write_atomic(&self.path, &encode(&next)?)?;
        self.value = next;
        Ok(outcome)
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| StorageError::Corrupt)?;
    bytes.push(b'\n');
    Ok(bytes)
}
