//! The typed values the host binds to a process generation.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use teamarness_package_protocol::Frame;

use crate::SdkError;

/// The Package the host admitted, and the digests it admitted it by.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageInfo {
    pub identity: String,
    pub version: String,
    pub manifest_digest: String,
    pub backend_digest: String,
}

/// The Workspace this Package Instance is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
}

/// The Installation running this Package.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallationInfo {
    pub identity: String,
    pub display_name: String,
}

/// The Package's own private directory, and the continuity identity it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DataDirectory {
    path: PathBuf,
    continuity_id: String,
}

impl DataDirectory {
    /// The directory the Package may read and write.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The opaque continuity identity: the same value means the same stored state.
    #[must_use]
    pub fn continuity_id(&self) -> &str {
        &self.continuity_id
    }
}

/// One Semantic Capability the host granted for this generation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityGrant {
    pub identity: String,
    pub grant: String,
    pub major: u16,
    pub minor: u16,
}

/// Initialization values bound to the current process generation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Initialization {
    package: PackageInfo,
    workspace: WorkspaceInfo,
    installation: InstallationInfo,
    data_directory: DataDirectory,
    process_generation: String,
    capabilities: Vec<CapabilityGrant>,
}

impl Initialization {
    pub(crate) fn from_frame(frame: &Frame) -> Result<Self, SdkError> {
        serde_json::from_value(frame.as_value()).map_err(|_| SdkError::Protocol("malformed_frame"))
    }

    /// The Package the host admitted.
    #[must_use]
    pub const fn package(&self) -> &PackageInfo {
        &self.package
    }

    /// The Workspace this Package Instance serves.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceInfo {
        &self.workspace
    }

    /// The Installation running this Package.
    #[must_use]
    pub const fn installation(&self) -> &InstallationInfo {
        &self.installation
    }

    /// The Package's own private directory.
    #[must_use]
    pub const fn data_directory(&self) -> &DataDirectory {
        &self.data_directory
    }

    /// The decimal process generation.
    #[must_use]
    pub fn process_generation(&self) -> &str {
        &self.process_generation
    }

    /// Every capability granted for this generation.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityGrant] {
        &self.capabilities
    }

    /// The grant for one capability identity, when the host granted it.
    #[must_use]
    pub fn capability(&self, identity: &str) -> Option<&CapabilityGrant> {
        self.capabilities
            .iter()
            .find(|grant| grant.identity == identity)
    }
}
