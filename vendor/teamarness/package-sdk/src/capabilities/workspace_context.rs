//! The Workspace context capability: who and where this Package Instance is.

use teamarness_package_protocol::JsonValue;

use super::{empty, grant_for, string_field};
use crate::{BackendError, CapabilityClient, Initialization};

/// The capability identity a manifest requires.
pub const IDENTITY: &str = "teamarness.workspace-context";

/// What the host knows about this Instance's Workspace and Installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceContextInfo {
    pub workspace_id: String,
    pub workspace_name: String,
    pub installation_identity: String,
    pub installation_display_name: String,
}

/// A typed client for `teamarness.workspace-context`.
#[derive(Clone)]
pub struct WorkspaceContext {
    client: CapabilityClient,
    grant: String,
}

impl WorkspaceContext {
    /// Binds the client to the grant the host gave this generation.
    ///
    /// # Errors
    ///
    /// Returns `package.capability_not_granted` when the manifest did not require it.
    pub fn from_initialization(
        initialization: &Initialization,
        client: CapabilityClient,
    ) -> Result<Self, BackendError> {
        Ok(Self {
            grant: grant_for(initialization, IDENTITY)?,
            client,
        })
    }

    /// Reads the Workspace and Installation this generation is bound to.
    ///
    /// # Errors
    ///
    /// Returns the host's capability error, or `package.invalid_capability_response`.
    pub async fn get(&self) -> Result<WorkspaceContextInfo, BackendError> {
        let answer: JsonValue = self.client.call(&self.grant, "get", empty()).await?;
        Ok(WorkspaceContextInfo {
            workspace_id: string_field(&answer, "workspaceId")?,
            workspace_name: string_field(&answer, "workspaceName")?,
            installation_identity: string_field(&answer, "installationIdentity")?,
            installation_display_name: string_field(&answer, "installationDisplayName")?,
        })
    }
}
