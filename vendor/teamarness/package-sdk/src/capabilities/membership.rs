//! The Membership capability: who belongs to the Workspace, and who did at a past instant.

use teamarness_package_protocol::JsonValue;

use super::{
    bool_field, decimal_field, empty, field, grant_for, invalid_request, invalid_response,
    is_lower_hex, string_field,
};
use crate::{BackendError, CapabilityClient, Initialization, object};

/// The capability identity a manifest requires.
pub const IDENTITY: &str = "teamarness.membership";

/// What a Member may do in the Workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    Member,
    Manager,
}

impl MemberRole {
    /// The contract's own string for this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Manager => "manager",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "member" => Some(Self::Member),
            "manager" => Some(Self::Manager),
            _ => None,
        }
    }
}

/// One Member of the Workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub identity: String,
    pub role: MemberRole,
}

/// The Workspace roster at one membership epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipSnapshot {
    pub workspace_id: String,
    pub epoch: u64,
    pub membership_version: String,
    pub members: Vec<Member>,
}

/// Whether an Installation was a Member at an instant, with the host's own caveat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipCheck {
    pub was_member: bool,
    /// The host's reminder that history is informational, never an authorization decision.
    pub warning: String,
}

/// A typed client for `teamarness.membership`.
#[derive(Clone)]
pub struct Membership {
    client: CapabilityClient,
    grant: String,
}

impl Membership {
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

    /// The current roster.
    ///
    /// # Errors
    ///
    /// Returns the host's capability error, or `package.invalid_capability_response`.
    pub async fn current(&self) -> Result<MembershipSnapshot, BackendError> {
        let answer = self.client.call(&self.grant, "current", empty()).await?;
        let members = field(&answer, "members")?
            .as_array()
            .ok_or_else(invalid_response)?
            .iter()
            .map(|member| {
                let role = string_field(member, "role")?;
                Ok(Member {
                    identity: string_field(member, "identity")?,
                    role: MemberRole::parse(&role).ok_or_else(invalid_response)?,
                })
            })
            .collect::<Result<Vec<_>, BackendError>>()?;
        let membership_version = string_field(&answer, "membershipVersion")?;
        if !is_lower_hex(&membership_version, 64) {
            return Err(invalid_response());
        }
        Ok(MembershipSnapshot {
            workspace_id: string_field(&answer, "workspaceId")?,
            epoch: decimal_field(&answer, "epoch")?,
            membership_version,
            members,
        })
    }

    /// Whether an Installation was a Member at a past instant.
    ///
    /// # Errors
    ///
    /// Returns `package.invalid_capability_request` for an identity the contract refuses, the
    /// host's capability error, or `package.invalid_capability_response`.
    pub async fn was_member(
        &self,
        installation: &str,
        at_unix_ms: i64,
    ) -> Result<MembershipCheck, BackendError> {
        if !is_lower_hex(installation, 64) {
            return Err(invalid_request());
        }
        let answer = self
            .client
            .call(
                &self.grant,
                "was_member",
                object([
                    ("identity", JsonValue::String(installation.to_owned())),
                    ("atUnixMs", JsonValue::String(at_unix_ms.to_string())),
                ]),
            )
            .await?;
        Ok(MembershipCheck {
            was_member: bool_field(&answer, "wasMember")?,
            warning: string_field(&answer, "warning")?,
        })
    }
}
