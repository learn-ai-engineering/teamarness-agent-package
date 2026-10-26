//! Generic Package Port adapters. The operation names are language-neutral wire names.
use crate::capabilities::{invalid_request, invalid_response};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use teamarness_package_contract::port_contract::{
    CompiledPortContracts, MAX_BYTES, compile_port_contracts,
};
use teamarness_package_contract::ui_contract::MAX_UI_PAYLOAD_BYTES;
use teamarness_package_protocol::JsonValue;

use crate::{BackendError, CapabilityClient, CapabilitySubscription};

/// Host-authored actor context for one Port dispatch; a caller never supplies it.
///
/// The Host emits the Installation Person for an unbound caller, and the
/// Workspace Agent the calling Package Instance is bound to through
/// `teamarness.agent-binding` otherwise (ADR 0030). `identity` is the actor's
/// current signing key in both cases; an Agent also carries its Principal ID,
/// which a Provider passes as `principal` to a record-log 1.1 append so Core
/// signs with the Agent's key.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PortActor {
    Person { identity: String },
    Agent { principal: String, identity: String },
}

impl PortActor {
    /// The actor's current signing key, whichever kind it is.
    #[must_use]
    pub fn identity(&self) -> &str {
        match self {
            Self::Person { identity } | Self::Agent { identity, .. } => identity,
        }
    }
}

/// Selects declared composition or current-bound-Agent access, never an actor claim.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortAccess {
    Declared,
    Agent,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataChunk {
    metadata_sha256: String,
    total_bytes: usize,
    offset: usize,
    data: String,
    next_offset: Option<usize>,
}

/// A bounded summary; schemas are retrieved separately with `describe`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContractSummary {
    pub contract: String,
    pub major: u16,
    pub minor: Option<u16>,
    pub description: Option<String>,
    pub available: bool,
    pub enabled: bool,
    pub enabled_major: Option<u16>,
    pub metadata_sha256: Option<String>,
}

/// A Package's opaque, generation-scoped capability grant, never a Provider selector.
#[derive(Clone)]
pub struct PortClient {
    capabilities: CapabilityClient,
    grant: String,
}

impl PortClient {
    #[must_use]
    pub fn new(capabilities: CapabilityClient, grant: impl Into<String>) -> Self {
        Self {
            capabilities,
            grant: grant.into(),
        }
    }

    /// Retrieves one bounded summary page. Agent access requires a current binding.
    pub async fn available(
        &self,
        access: PortAccess,
        cursor: Option<&str>,
    ) -> Result<JsonValue, BackendError> {
        let mut request = serde_json::json!({"access":access});
        if let Some(cursor) = cursor {
            request["cursor"] = cursor.into();
        }
        let response = self
            .capabilities
            .call(&self.grant, "available", request)
            .await?;
        if serde_json::to_vec(&response)
            .map_err(|_| invalid_response())?
            .len()
            > MAX_UI_PAYLOAD_BYTES / 8
        {
            return Err(invalid_response());
        }
        if response.get("nextCursor").is_some_and(|v| !v.is_string())
            || response
                .get("contracts")
                .and_then(JsonValue::as_array)
                .is_none_or(|rows| {
                    rows.iter()
                        .any(|row| row.as_object().is_none_or(|fields| fields.len() != 8))
                })
        {
            return Err(invalid_response());
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Page {
            contracts: Vec<ContractSummary>,
            next_cursor: Option<String>,
        }
        let page: Page =
            serde_json::from_value(response.clone()).map_err(|_| invalid_response())?;
        if page.contracts.len() > 32
            || page
                .next_cursor
                .as_ref()
                .is_some_and(|c| c.is_empty() || c.len() > 2048)
            || page.contracts.iter().any(|c| {
                !teamarness_package_contract::identity::is_valid_package_identity(&c.contract)
                    || c.major == 0
                    || c.enabled_major == Some(0)
                    || c.description
                        .as_ref()
                        .is_some_and(|d| d.is_empty() || d.chars().count() > 512)
                    || c.metadata_sha256
                        .as_ref()
                        .is_some_and(|d| !crate::capabilities::is_lower_hex(d, 64))
            })
        {
            return Err(invalid_response());
        }
        Ok(response)
    }

    /// Lists the caller's actual declared dependencies, without requiring an Agent Binding.
    pub async fn declared_contracts(&self) -> Result<Vec<ContractSummary>, BackendError> {
        self.contracts(PortAccess::Declared).await
    }

    /// Lists eligible Agent contracts and retained selections without waking Providers.
    pub async fn agent_contracts(&self) -> Result<Vec<ContractSummary>, BackendError> {
        self.contracts(PortAccess::Agent).await
    }

    /// Durably selects a major for this harness in its granted Workspace.
    pub async fn enable(&self, contract: &str, major: u16) -> Result<(), BackendError> {
        if !teamarness_package_contract::identity::is_valid_package_identity(contract) || major == 0
        {
            return Err(invalid_request());
        }
        let result = self
            .capabilities
            .call(
                &self.grant,
                "enable",
                serde_json::json!({"contract":contract,"major":major}),
            )
            .await?;
        if result != serde_json::json!({"contract":contract,"major":major,"enabled":true}) {
            return Err(invalid_response());
        }
        Ok(())
    }

    /// Removes this harness's selection, including when its Provider is unavailable.
    pub async fn disable(&self, contract: &str) -> Result<(), BackendError> {
        if !teamarness_package_contract::identity::is_valid_package_identity(contract) {
            return Err(invalid_request());
        }
        let result = self
            .capabilities
            .call(
                &self.grant,
                "disable",
                serde_json::json!({"contract":contract}),
            )
            .await?;
        if result != serde_json::json!({"contract":contract,"enabled":false}) {
            return Err(invalid_response());
        }
        Ok(())
    }

    async fn contracts(&self, access: PortAccess) -> Result<Vec<ContractSummary>, BackendError> {
        let mut rows: Vec<ContractSummary> = Vec::new();
        let mut cursor = None::<String>;
        loop {
            let page = self.available(access, cursor.as_deref()).await?;
            let entries: Vec<ContractSummary> = serde_json::from_value(page["contracts"].clone())
                .map_err(|_| invalid_response())?;
            if entries.is_empty() && page.get("nextCursor").is_some() {
                return Err(invalid_response());
            }
            for entry in entries {
                if rows
                    .last()
                    .is_some_and(|last| last.contract >= entry.contract)
                {
                    return Err(invalid_response());
                }
                rows.push(entry);
                if matches!(access, PortAccess::Declared)
                    && rows.len() > teamarness_package_contract::manifest::MAX_CONSUMED_CONTRACTS
                {
                    return Err(invalid_response());
                }
            }
            let Some(next) = page.get("nextCursor").and_then(JsonValue::as_str) else {
                return Ok(rows);
            };
            if cursor.as_deref() == Some(next) {
                return Err(invalid_response());
            }
            cursor = Some(next.to_owned());
        }
    }

    /// Assembles complete metadata, checks every chunk and SHA-256, then compiles its schemas.
    pub async fn describe(
        &self,
        access: PortAccess,
        contract: &str,
        metadata_sha256: &str,
    ) -> Result<CompiledPortContracts, BackendError> {
        if !crate::capabilities::is_lower_hex(metadata_sha256, 64) {
            return Err(invalid_request());
        }
        let mut bytes = Vec::new();
        let mut total = None;
        loop {
            let response = self.capabilities.call(&self.grant, "describe", serde_json::json!({
                "access":access,"contract":contract,"metadataSha256":metadata_sha256,"offset":bytes.len()
            })).await?;
            if response
                .get("nextOffset")
                .is_some_and(|v| v.as_u64().is_none())
            {
                return Err(invalid_response());
            }
            let chunk: MetadataChunk =
                serde_json::from_value(response).map_err(|_| invalid_response())?;
            if chunk.metadata_sha256 != metadata_sha256
                || chunk.total_bytes == 0
                || chunk.total_bytes > MAX_BYTES
                || total.is_some_and(|total| total != chunk.total_bytes)
                || chunk.offset != bytes.len()
                || chunk.data.len() > (MAX_UI_PAYLOAD_BYTES / 4).div_ceil(3) * 4
            {
                return Err(invalid_response());
            }
            total = Some(chunk.total_bytes);
            let decoded = STANDARD
                .decode(&chunk.data)
                .map_err(|_| invalid_response())?;
            if decoded.is_empty()
                || decoded.len() > MAX_UI_PAYLOAD_BYTES / 4
                || bytes.len() + decoded.len() > chunk.total_bytes
            {
                return Err(invalid_response());
            }
            bytes.extend(decoded);
            match chunk.next_offset {
                Some(next) if next == bytes.len() && next < chunk.total_bytes => {}
                None if bytes.len() == chunk.total_bytes => break,
                _ => return Err(invalid_response()),
            }
        }
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if digest != metadata_sha256 {
            return Err(invalid_response());
        }
        let document = tokio::task::spawn_blocking(move || compile_port_contracts(&bytes))
            .await
            .map_err(|_| invalid_response())?
            .map_err(|_| invalid_response())?;
        if document.contracts.len() != 1 || document.contracts[0].identity != contract {
            return Err(invalid_response());
        }
        Ok(document)
    }

    pub async fn command(
        &self,
        access: PortAccess,
        contract: &str,
        major: u16,
        port: &str,
        request: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.invoke(access, contract, major, port, "command", request)
            .await
    }

    pub async fn query(
        &self,
        access: PortAccess,
        contract: &str,
        major: u16,
        port: &str,
        request: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.invoke(access, contract, major, port, "query", request)
            .await
    }

    async fn invoke(
        &self,
        access: PortAccess,
        contract: &str,
        major: u16,
        port: &str,
        kind: &str,
        request: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.capabilities
            .call(
                &self.grant,
                "invoke",
                serde_json::json!({
                    "access":access,"contract":contract,"major":major,"port":port,"kind":kind,"request":request
                }),
            )
            .await
            .map(decode_port_value)
    }

    /// A one-to-one consumer-initiated query stream, not an event subscription.
    pub async fn live_query(
        &self,
        access: PortAccess,
        contract: &str,
        major: u16,
        port: &str,
        request: JsonValue,
    ) -> Result<CapabilitySubscription, BackendError> {
        self.capabilities
            .subscribe(
                &self.grant,
                serde_json::json!({
                    "access":access,"contract":contract,"major":major,"port":port,"kind":"query","request":request
                }),
            )
            .await
            .map(CapabilitySubscription::with_port_values)
    }

    /// Explicitly observes live Provider events; a declaration alone never subscribes.
    pub async fn subscribe_event(
        &self,
        access: PortAccess,
        contract: &str,
        major: u16,
        port: &str,
    ) -> Result<CapabilitySubscription, BackendError> {
        self.capabilities
            .subscribe(
                &self.grant,
                serde_json::json!({"access":access,"contract":contract,"major":major,"port":port,"kind":"event"}),
            )
            .await
            .map(CapabilitySubscription::with_port_values)
    }
}

// A Provider SDK always wraps Port values. In particular, an object equal to
// the reserved transport shape must not be mistaken for an envelope itself.
pub(crate) fn encode_port_value(value: JsonValue) -> JsonValue {
    serde_json::json!({"$teamarnessPortEnvelope":1,"$teamarnessPortValue":value})
}

pub(crate) fn decode_port_value(value: JsonValue) -> JsonValue {
    if let JsonValue::Object(ref fields) = value
        && fields.len() == 2
        && fields.get("$teamarnessPortEnvelope") == Some(&JsonValue::from(1))
        && fields.contains_key("$teamarnessPortValue")
    {
        return fields["$teamarnessPortValue"].clone();
    }
    value
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderEnvelope {
    pub(crate) contract: String,
    pub(crate) major: u16,
    #[serde(rename = "minor")]
    pub(crate) _minor: u16,
    pub(crate) port: String,
    pub(crate) actor: PortActor,
    pub(crate) request: JsonValue,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actor_round_trips_both_kinds_and_rejects_anything_else() {
        let person = json!({"kind":"person","identity":"a".repeat(64)});
        let agent = json!({"kind":"agent","principal":"b".repeat(64),"identity":"c".repeat(64)});
        let decoded: PortActor = serde_json::from_value(person.clone()).unwrap();
        assert_eq!(
            decoded,
            PortActor::Person {
                identity: "a".repeat(64)
            }
        );
        assert_eq!(decoded.identity(), "a".repeat(64));
        assert_eq!(serde_json::to_value(&decoded).unwrap(), person);
        let decoded: PortActor = serde_json::from_value(agent.clone()).unwrap();
        assert_eq!(
            decoded,
            PortActor::Agent {
                principal: "b".repeat(64),
                identity: "c".repeat(64)
            }
        );
        assert_eq!(decoded.identity(), "c".repeat(64));
        assert_eq!(serde_json::to_value(&decoded).unwrap(), agent);
        for rejected in [
            json!({"kind":"person","identity":"a","principal":"b"}),
            json!({"kind":"agent","identity":"a"}),
            json!({"kind":"agent","principal":"b","identity":"a","extra":1}),
            json!({"kind":"service","identity":"a"}),
            json!({"identity":"a"}),
        ] {
            assert!(
                serde_json::from_value::<PortActor>(rejected.clone()).is_err(),
                "{rejected}"
            );
        }
    }
}
