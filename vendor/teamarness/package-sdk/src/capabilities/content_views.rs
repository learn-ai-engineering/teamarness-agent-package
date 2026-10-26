//! Content views over files in the Package's own Data Directory.
//!
//! A Package lists the files of a view — a content path, a SHA-256, a size and a source inside
//! its Package Data Directory — and the Host serves exactly those files under
//! `teamarness-content://localhost/<view>/<path>`, verified and with a media type from the
//! shared extension allowlist. Declaring `teamarness.content-views` is also what lets the
//! Package's UI mounts nest a `teamarness-content:` frame.
//!
//! The Host refuses a request with `unsupported_media_type` (an extension outside the
//! allowlist; extensions match lowercase only), `content_too_large` (more than 1,000 files, a
//! declared total over 64 MiB, or a file over 16 MiB) or `invalid_content_path` (a bad or
//! duplicate path, an unlisted entry, or a source that is missing, a symlink, not a regular
//! file or outside the Data Directory). A view ends when it is closed, when the Package opens
//! a 33rd view in the same process generation (the least recently opened ends), or when the
//! generation ends.

use teamarness_package_protocol::JsonValue;

use super::{field, grant_for, invalid_request, invalid_response, is_lower_hex, string_field};
use crate::{BackendError, CapabilityClient, Initialization, object};

pub const IDENTITY: &str = "teamarness.content-views";
/// The most files one view may list.
pub const MAX_FILES: usize = 1_000;

/// Where the Host reads one file's bytes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentSource {
    /// A regular file at this relative path inside the Package Data Directory.
    File { path: String },
}

/// One file a view serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentFile {
    /// The path inside the view, as the view's URLs name it.
    pub path: String,
    /// The lowercase hexadecimal SHA-256 of the file's bytes.
    pub sha256: String,
    pub size: u64,
    pub source: ContentSource,
}

/// An open view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedView {
    /// The opaque view token, which `close` takes.
    pub view: String,
    /// `teamarness-content://localhost/<view>/<entry>`, ready to use as a frame `src`.
    pub url: String,
}

fn encode_file(file: &ContentFile) -> Result<JsonValue, BackendError> {
    if !is_lower_hex(&file.sha256, 64) {
        return Err(invalid_request());
    }
    let ContentSource::File { path } = &file.source;
    Ok(object([
        ("path", JsonValue::String(file.path.clone())),
        ("sha256", JsonValue::String(file.sha256.clone())),
        ("size", JsonValue::from(file.size)),
        (
            "source",
            object([
                ("kind", JsonValue::String("file".into())),
                ("path", JsonValue::String(path.clone())),
            ]),
        ),
    ]))
}

fn decode_opened(value: &JsonValue) -> Result<OpenedView, BackendError> {
    let view = string_field(value, "view")?;
    let url = string_field(value, "url")?;
    if !is_lower_hex(&view, 32)
        || !url.starts_with(&format!("teamarness-content://localhost/{view}/"))
        || value.as_object().is_none_or(|fields| fields.len() != 2)
    {
        return Err(invalid_response());
    }
    Ok(OpenedView { view, url })
}

/// A typed client for `teamarness.content-views` 1.0.
#[derive(Clone)]
pub struct ContentViews {
    client: CapabilityClient,
    grant: String,
}

impl ContentViews {
    /// # Errors
    /// Returns `package.capability_not_granted` if the manifest does not require the family.
    pub fn from_initialization(
        initialization: &Initialization,
        client: CapabilityClient,
    ) -> Result<Self, BackendError> {
        Ok(Self {
            grant: grant_for(initialization, IDENTITY)?,
            client,
        })
    }

    /// Opens a view over `files`, entered at `entry`, which must be one of the listed paths.
    /// # Errors
    /// Returns an invalid-request error for an empty or oversized list or a malformed SHA-256,
    /// one of the Host refusals the module describes, or an invalid-response error for a
    /// malformed answer.
    pub async fn open(
        &self,
        entry: &str,
        files: &[ContentFile],
    ) -> Result<OpenedView, BackendError> {
        if files.is_empty() || files.len() > MAX_FILES {
            return Err(invalid_request());
        }
        let files = files
            .iter()
            .map(encode_file)
            .collect::<Result<Vec<_>, _>>()?;
        let answer = self
            .client
            .call(
                &self.grant,
                "open",
                object([
                    ("entry", JsonValue::String(entry.into())),
                    ("files", JsonValue::Array(files)),
                ]),
            )
            .await?;
        decode_opened(&answer)
    }

    /// Closes one of this generation's views. Returns whether it was still open.
    /// # Errors
    /// Returns an invalid-request error for a malformed view token, a Host error, or an
    /// invalid-response error for a malformed answer.
    pub async fn close(&self, view: &str) -> Result<bool, BackendError> {
        if !is_lower_hex(view, 32) {
            return Err(invalid_request());
        }
        let answer = self
            .client
            .call(
                &self.grant,
                "close",
                object([("view", JsonValue::String(view.into()))]),
            )
            .await?;
        field(&answer, "closed")?
            .as_bool()
            .ok_or_else(invalid_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_files_and_decodes_views_with_bounds() {
        let file = ContentFile {
            path: "index.html".into(),
            sha256: "a".repeat(64),
            size: 12,
            source: ContentSource::File {
                path: "content/sha256/aa".into(),
            },
        };
        assert_eq!(
            encode_file(&file).unwrap(),
            json!({"path":"index.html","sha256":"a".repeat(64),"size":12,"source":{"kind":"file","path":"content/sha256/aa"}})
        );
        let mut upper = file;
        upper.sha256 = "A".repeat(64);
        assert!(encode_file(&upper).is_err());

        let view = "0".repeat(32);
        let url = format!("teamarness-content://localhost/{view}/index.html");
        assert_eq!(
            decode_opened(&json!({"view":view,"url":url})).unwrap(),
            OpenedView {
                view: view.clone(),
                url: url.clone()
            }
        );
        assert!(decode_opened(&json!({"view":"short","url":url})).is_err());
        assert!(
            decode_opened(&json!({"view":view,"url":"https://example.com/index.html"})).is_err()
        );
        assert!(decode_opened(&json!({"view":view,"url":url,"extra":1})).is_err());
    }
}
