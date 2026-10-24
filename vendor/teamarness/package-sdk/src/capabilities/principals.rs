//! Current Workspace Principals, independently of membership authority.

use teamarness_package_protocol::JsonValue;

use super::{
    decimal_field, field, grant_for, invalid_request, invalid_response, is_lower_hex, string_field,
};
use crate::capability::CapabilitySubscription;
use crate::{BackendError, CapabilityClient, Initialization, object};

pub const IDENTITY: &str = "teamarness.principals";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalKind {
    Person,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub principal_id: String,
    pub kind: PrincipalKind,
    pub steward_principal_id: Option<String>,
    pub keys: Vec<String>,
    pub name: Option<String>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalSnapshot {
    pub workspace_id: String,
    /// Durable profile invalidation cursor; not a membership epoch or record cursor.
    pub cursor: u64,
    pub principals: Vec<Principal>,
}

fn decode_principal(value: &JsonValue) -> Result<Principal, BackendError> {
    let principal_id = string_field(value, "principalId")?;
    let fingerprint = string_field(value, "fingerprint")?;
    if !is_lower_hex(&principal_id, 64) || fingerprint.is_empty() || fingerprint.len() > 64 {
        return Err(invalid_response());
    }
    let kind = match string_field(value, "kind")?.as_str() {
        "person" => PrincipalKind::Person,
        "agent" => PrincipalKind::Agent,
        _ => return Err(invalid_response()),
    };
    let steward_principal_id = match field(value, "stewardPrincipalId")? {
        JsonValue::Null if kind == PrincipalKind::Person => None,
        JsonValue::String(id) if kind == PrincipalKind::Agent && is_lower_hex(id, 64) => {
            Some(id.clone())
        }
        _ => return Err(invalid_response()),
    };
    let keys = field(value, "keys")?
        .as_array()
        .ok_or_else(invalid_response)?;
    if keys.len() != 1 {
        return Err(invalid_response());
    }
    let keys = keys
        .iter()
        .map(|key| {
            let key = key.as_str().ok_or_else(invalid_response)?;
            if !is_lower_hex(key, 64) {
                return Err(invalid_response());
            }
            Ok(key.to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let name = match field(value, "name")? {
        JsonValue::Null => None,
        JsonValue::String(name) if !name.trim().is_empty() && name.len() <= 256 => {
            Some(name.clone())
        }
        _ => return Err(invalid_response()),
    };
    Ok(Principal {
        principal_id,
        kind,
        steward_principal_id,
        keys,
        name,
        fingerprint,
    })
}

fn decode_snapshot(value: &JsonValue) -> Result<PrincipalSnapshot, BackendError> {
    let workspace_id = string_field(value, "workspaceId")?;
    if !is_lower_hex(&workspace_id, 32) {
        return Err(invalid_response());
    }
    let principals = field(value, "principals")?
        .as_array()
        .ok_or_else(invalid_response)?
        .iter()
        .map(decode_principal)
        .collect::<Result<Vec<_>, _>>()?;
    if principals.len() > 1024 {
        return Err(invalid_response());
    }
    Ok(PrincipalSnapshot {
        workspace_id,
        cursor: decimal_field(value, "cursor")?,
        principals,
    })
}

#[derive(Clone)]
pub struct Principals {
    client: CapabilityClient,
    grant: String,
}

impl Principals {
    /// # Errors
    /// Returns `package.capability_not_granted` if the manifest does not require names.
    pub fn from_initialization(
        initialization: &Initialization,
        client: CapabilityClient,
    ) -> Result<Self, BackendError> {
        Ok(Self {
            grant: grant_for(initialization, IDENTITY)?,
            client,
        })
    }

    /// # Errors
    /// Returns a Host error or an invalid-response error for a malformed snapshot.
    pub async fn current(&self) -> Result<PrincipalSnapshot, BackendError> {
        decode_snapshot(
            &self
                .client
                .call(&self.grant, "current", super::empty())
                .await?,
        )
    }

    /// Resolves a current Principal by signing key.
    pub async fn lookup(&self, identity: &str) -> Result<Option<Principal>, BackendError> {
        self.lookup_field("identity", identity).await
    }

    /// Resolves a current Principal by its Workspace-local ID.
    pub async fn lookup_id(&self, principal_id: &str) -> Result<Option<Principal>, BackendError> {
        self.lookup_field("principalId", principal_id).await
    }

    async fn lookup_field(
        &self,
        field_name: &'static str,
        value: &str,
    ) -> Result<Option<Principal>, BackendError> {
        if !is_lower_hex(value, 64) {
            return Err(invalid_request());
        }
        let result = self
            .client
            .call(
                &self.grant,
                "lookup",
                object([(field_name, JsonValue::String(value.into()))]),
            )
            .await?;
        match field(&result, "principal")? {
            JsonValue::Null => Ok(None),
            value => decode_principal(value).map(Some),
        }
    }

    /// Resumes after a durable profile cursor; each notification carries a fresh snapshot.
    /// # Errors
    /// Returns a Host stream error when subscription cannot be opened.
    pub async fn subscribe(
        &self,
        after_cursor: u64,
    ) -> Result<PrincipalSubscription, BackendError> {
        let inner = self
            .client
            .subscribe(
                &self.grant,
                object([
                    ("method", JsonValue::String("subscribe".into())),
                    ("afterCursor", JsonValue::String(after_cursor.to_string())),
                ]),
            )
            .await?;
        Ok(PrincipalSubscription { inner })
    }
}

pub struct PrincipalSubscription {
    inner: CapabilitySubscription,
}

impl PrincipalSubscription {
    /// The next coalesced snapshot, or `None` after closure.
    pub async fn next(&mut self) -> Option<Result<PrincipalSnapshot, BackendError>> {
        self.inner
            .recv()
            .await
            .map(|value| value.and_then(|value| decode_snapshot(&value)))
    }

    pub async fn close(&mut self) {
        self.inner.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_person_and_agent_rows_with_steward_constraints() {
        let row = json!({"principalId":"a".repeat(64), "kind":"person", "stewardPrincipalId":null,
            "keys":["b".repeat(64)], "name":null, "fingerprint":"abcd1234"});
        let snapshot = json!({"workspaceId":"c".repeat(32), "cursor":"3", "principals":[row]});
        assert_eq!(
            decode_snapshot(&snapshot).unwrap().principals[0].kind,
            PrincipalKind::Person
        );
        let mut agent = snapshot;
        agent["principals"][0]["kind"] = json!("agent");
        assert!(decode_snapshot(&agent).is_err());
        agent["principals"][0]["stewardPrincipalId"] = json!("d".repeat(64));
        assert_eq!(
            decode_snapshot(&agent).unwrap().principals[0].kind,
            PrincipalKind::Agent
        );
        agent["principals"][0]["keys"] = json!([]);
        assert!(decode_snapshot(&agent).is_err());
    }
}
