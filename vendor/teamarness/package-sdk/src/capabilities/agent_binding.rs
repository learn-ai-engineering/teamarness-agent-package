//! A Package Instance's own Agent Binding: which Workspace Agent it acts as.
//!
//! The grant fixes the Package and Workspace, so no call names a Package; a
//! Package can only read, set and clear its own binding. The Host answers
//! `agent_not_stewarded` when this Installation does not steward the Agent and
//! `agent_not_found` when the Principal is not a current Workspace Agent.

use teamarness_package_protocol::JsonValue;

use super::{
    bool_field, field, grant_for, invalid_request, invalid_response, is_lower_hex, string_field,
};
use crate::{BackendError, CapabilityClient, Initialization, object};

pub const IDENTITY: &str = "teamarness.agent-binding";

/// The Agent a Package Instance is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundAgent {
    pub principal_id: String,
    /// The Agent's current signing key, which record authors resolve to.
    pub identity: String,
    pub name: Option<String>,
}

/// A Workspace Agent this Installation stewards, and whether this Package is bound to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCandidate {
    pub principal_id: String,
    pub identity: String,
    pub name: Option<String>,
    pub bound: bool,
}

fn decode_agent(value: &JsonValue) -> Result<BoundAgent, BackendError> {
    let principal_id = string_field(value, "principalId")?;
    let identity = string_field(value, "identity")?;
    if !is_lower_hex(&principal_id, 64) || !is_lower_hex(&identity, 64) {
        return Err(invalid_response());
    }
    let name = match field(value, "name")? {
        JsonValue::Null => None,
        JsonValue::String(name) if !name.trim().is_empty() && name.len() <= 256 => {
            Some(name.clone())
        }
        _ => return Err(invalid_response()),
    };
    Ok(BoundAgent {
        principal_id,
        identity,
        name,
    })
}

fn decode_binding(value: &JsonValue) -> Result<Option<BoundAgent>, BackendError> {
    match field(value, "agent")? {
        JsonValue::Null => Ok(None),
        agent => decode_agent(agent).map(Some),
    }
}

fn decode_candidates(value: &JsonValue) -> Result<Vec<AgentCandidate>, BackendError> {
    let rows = field(value, "agents")?
        .as_array()
        .ok_or_else(invalid_response)?;
    if rows.len() > 1024 {
        return Err(invalid_response());
    }
    rows.iter()
        .map(|row| {
            let agent = decode_agent(row)?;
            Ok(AgentCandidate {
                principal_id: agent.principal_id,
                identity: agent.identity,
                name: agent.name,
                bound: bool_field(row, "bound")?,
            })
        })
        .collect()
}

/// A typed client for `teamarness.agent-binding` 1.0.
#[derive(Clone)]
pub struct AgentBinding {
    client: CapabilityClient,
    grant: String,
}

impl AgentBinding {
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

    /// This Package Instance's bound Agent, or `None` when unbound or when the
    /// bound Agent is no longer a current Workspace Agent.
    /// # Errors
    /// Returns a Host error or an invalid-response error for a malformed answer.
    pub async fn current(&self) -> Result<Option<BoundAgent>, BackendError> {
        decode_binding(
            &self
                .client
                .call(&self.grant, "current", super::empty())
                .await?,
        )
    }

    /// The Workspace Agents this Installation stewards, each marked with
    /// whether this Package Instance is bound to it. Other Persons' Agents are
    /// never listed.
    /// # Errors
    /// Returns a Host error or an invalid-response error for a malformed answer.
    pub async fn candidates(&self) -> Result<Vec<AgentCandidate>, BackendError> {
        decode_candidates(
            &self
                .client
                .call(&self.grant, "candidates", super::empty())
                .await?,
        )
    }

    /// Binds this Package Instance to one Agent, replacing any previous binding,
    /// and returns the bound Agent.
    /// # Errors
    /// Returns `agent_not_stewarded` when this Installation does not steward the
    /// Agent, `agent_not_found` when it is not a current Workspace Agent, or an
    /// invalid-request error for a malformed Principal ID.
    pub async fn bind(&self, principal_id: &str) -> Result<BoundAgent, BackendError> {
        if !is_lower_hex(principal_id, 64) {
            return Err(invalid_request());
        }
        let answer = self
            .client
            .call(
                &self.grant,
                "bind",
                object([("principalId", JsonValue::String(principal_id.into()))]),
            )
            .await?;
        decode_binding(&answer)?.ok_or_else(invalid_response)
    }

    /// Clears this Package Instance's binding. Idempotent.
    /// # Errors
    /// Returns a Host error.
    pub async fn unbind(&self) -> Result<(), BackendError> {
        let answer = self
            .client
            .call(&self.grant, "unbind", super::empty())
            .await?;
        match decode_binding(&answer)? {
            None => Ok(()),
            Some(_) => Err(invalid_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_binding_and_candidates_with_bounds() {
        assert_eq!(decode_binding(&json!({"agent":null})).unwrap(), None);
        let agent = json!({"principalId":"a".repeat(64),"identity":"b".repeat(64),"name":"Scout"});
        assert_eq!(
            decode_binding(&json!({"agent":agent})).unwrap(),
            Some(BoundAgent {
                principal_id: "a".repeat(64),
                identity: "b".repeat(64),
                name: Some("Scout".into()),
            })
        );
        assert!(decode_binding(&json!({})).is_err());
        assert!(
            decode_binding(
                &json!({"agent":{"principalId":"short","identity":"b".repeat(64),"name":null}})
            )
            .is_err()
        );
        assert!(
            decode_binding(&json!({"agent":{"principalId":"a".repeat(64),"identity":"b".repeat(64),"name":" "}}))
                .is_err()
        );
        let mut row = agent.clone();
        row["bound"] = json!(true);
        let candidates = decode_candidates(&json!({"agents":[row]})).unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].bound);
        assert_eq!(candidates[0].name.as_deref(), Some("Scout"));
        assert!(decode_candidates(&json!({"agents":[agent]})).is_err());
        let many = vec![
            json!({"principalId":"a".repeat(64),"identity":"b".repeat(64),"name":null,"bound":false});
            1025
        ];
        assert!(decode_candidates(&json!({"agents":many})).is_err());
    }
}
