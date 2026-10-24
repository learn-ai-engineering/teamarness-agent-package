//! Generic Package Port adapters. The operation names are language-neutral wire names.
use serde::{Deserialize, Serialize};
use teamarness_package_protocol::JsonValue;

use crate::{BackendError, CapabilityClient, CapabilitySubscription};

/// Host-authored actor context for one Port dispatch; a caller never supplies it.
///
/// The Host emits the Installation Person for an unbound caller, and the
/// Workspace Agent the calling Package Instance is bound to through
/// `teamarness.agent-binding` otherwise (ADR 0027). `identity` is the actor's
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

    /// Lists only exactly declared Ports and their availability.
    pub async fn available(&self) -> Result<JsonValue, BackendError> {
        self.capabilities
            .call(&self.grant, "available", serde_json::json!({}))
            .await
    }

    pub async fn command(
        &self,
        contract: &str,
        port: &str,
        request: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.invoke(contract, port, "command", request).await
    }

    pub async fn query(
        &self,
        contract: &str,
        port: &str,
        request: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.invoke(contract, port, "query", request).await
    }

    async fn invoke(
        &self,
        contract: &str,
        port: &str,
        kind: &str,
        request: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.capabilities
            .call(
                &self.grant,
                "invoke",
                serde_json::json!({
                    "contract":contract,"port":port,"kind":kind,"request":request
                }),
            )
            .await
            .map(decode_port_value)
    }

    /// A one-to-one consumer-initiated query stream, not an event subscription.
    pub async fn live_query(
        &self,
        contract: &str,
        port: &str,
        request: JsonValue,
    ) -> Result<CapabilitySubscription, BackendError> {
        self.capabilities
            .subscribe(
                &self.grant,
                serde_json::json!({
                    "contract":contract,"port":port,"kind":"query","request":request
                }),
            )
            .await
            .map(CapabilitySubscription::with_port_values)
    }

    /// Explicitly observes live Provider events; a declaration alone never subscribes.
    pub async fn subscribe_event(
        &self,
        contract: &str,
        port: &str,
    ) -> Result<CapabilitySubscription, BackendError> {
        self.capabilities
            .subscribe(
                &self.grant,
                serde_json::json!({"contract":contract,"port":port,"kind":"event"}),
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
