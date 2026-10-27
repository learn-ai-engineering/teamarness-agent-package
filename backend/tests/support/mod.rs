//! External Host metadata/selection fixture. Production Catalog/access/tools are never replaced.
#![allow(dead_code)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use teamarness_package_sdk::BackendError;

pub const CONVERSATIONS: &str = "dev.teamarness.conversations";
pub const EXHIBITS: &str = "dev.teamarness.exhibits";
pub const UNKNOWN: &str = "com.example.survey";

#[derive(Clone, Default)]
pub struct HostCatalog {
    pub documents: Arc<Mutex<BTreeMap<String, Value>>>,
    pub selections: Arc<Mutex<BTreeMap<String, u16>>>,
    pub refusal: Arc<Mutex<Option<&'static str>>>,
    pub corruption: Arc<Mutex<Option<&'static str>>>,
}

pub fn unknown_document() -> Value {
    json!({"schemaVersion":1,"$defs":{
        "label":{"type":"string","minLength":2,"maxLength":12},
        "selection":{"type":"object","additionalProperties":false,"required":["label"],"properties":{"label":{"$ref":"#/$defs/label"}}}
    },"contracts":[{"identity":UNKNOWN,"major":3,"minor":2,"description":"Survey the Workspace.","ports":[
        {"name":"runtime.guide","kind":"query","description":"Usage, not schemas.","request":{"type":"object","additionalProperties":false},"response":{"type":"object","additionalProperties":false,"required":["markdown"],"properties":{"markdown":{"type":"string","maxLength":1024}}}},
        {"name":"sample.read","kind":"query","description":"Read a named sample.","request":{"$ref":"#/$defs/selection"},"response":{"type":"object","additionalProperties":false,"required":["sample"],"properties":{"sample":{"type":"string","maxLength":128}}}},
        {"name":"sample.watch","kind":"query","description":"Live query; not a model function.","request":{"type":"object","additionalProperties":false},"event":{"type":"object","additionalProperties":false}},
        {"name":"samples.changed","kind":"event","description":"Invalidation only.","event":{"type":"object","additionalProperties":false}}
    ]}]})
}

/// Compile through the public SDK, then use the actual canonical projection and validators.
/// The returned bytes are the external Host metadata fixture, not an Agent Catalog substitute.
pub async fn project_document(document: Value) -> Value {
    use teamarness_package_sdk::ports::{PortAccess, PortClient};
    use teamarness_package_sdk::testing::{HarnessBuilder, ScriptedCapability};
    use teamarness_package_sdk::{Empty, Router};

    let identity = document["contracts"][0]["identity"]
        .as_str()
        .unwrap()
        .to_owned();
    let catalog = HostCatalog::default();
    catalog
        .documents
        .lock()
        .unwrap()
        .insert(identity.clone(), document.clone());
    let router = Router::new(|activation| async move {
        let grant = &activation.initialization().capabilities()[0].grant;
        Ok(PortClient::new(
            activation.capabilities().clone(),
            grant.clone(),
        ))
    })
    .request("fixture.project", move |ctx, _: Empty| {
        let identity = identity.clone();
        let expected = document.clone();
        async move {
            let compiled = ctx
                .state()
                .describe(PortAccess::Agent, &identity, &digest(&expected))
                .await?;
            assert!(compiled.warnings.is_empty());
            assert!(compiled.has_guide(&identity));
            assert_eq!(
                compiled.definitions,
                *expected["$defs"].as_object().unwrap()
            );
            compiled
                .validate(&identity, "sample.read", "request", &json!({}))
                .unwrap();
            assert!(
                compiled
                    .validate(&identity, "sample.read", "request", &json!({"extra":1}))
                    .is_err()
            );
            let projection = compiled.project_metadata(&identity, None).unwrap();
            Ok(serde_json::from_slice::<Value>(&projection.bytes).unwrap())
        }
    });
    let fixture = catalog.clone();
    let data = tempfile::tempdir().unwrap();
    let mut harness = HarnessBuilder::new(router.into_backend())
        .data_directory(data.path())
        .capability(
            "teamarness.package-ports",
            ScriptedCapability::new()
                .calls(move |method, payload| fixture.handle(method, &payload).unwrap()),
        )
        .start()
        .await;
    harness.activate().await.unwrap();
    let projected = harness.request("fixture.project", json!({})).await.unwrap();
    harness.shutdown().await.unwrap();
    projected
}

impl HostCatalog {
    pub fn providers() -> Self {
        let catalog = Self::default();
        let exhibits: Value =
            serde_json::from_str(include_str!("../../catalog/exhibits.ports.json")).unwrap();
        let mut conversations: Value =
            serde_json::from_str(include_str!("../../catalog/conversations.ports.json")).unwrap();
        conversations["contracts"][0]["minor"] = json!(3);
        conversations["contracts"][0]["ports"].as_array_mut().unwrap().push(json!({
            "name":"runtime.guide","kind":"query","description":"Use Conversations intentionally.",
            "request":{"type":"object","additionalProperties":false},
            "response":{"type":"object","additionalProperties":false,"required":["markdown"],"properties":{"markdown":{"type":"string","maxLength":1024}}}
        }));
        catalog
            .documents
            .lock()
            .unwrap()
            .insert(CONVERSATIONS.into(), conversations);
        catalog
            .documents
            .lock()
            .unwrap()
            .insert(EXHIBITS.into(), exhibits);
        catalog
    }

    pub fn handle(&self, method: &str, request: &Value) -> Option<Result<Value, BackendError>> {
        let fail = |code| Err(BackendError::new(code, false, json!({})));
        match method {
            "available" => {
                if request["access"] != "agent" {
                    return Some(fail("capability_request_invalid"));
                }
                let documents = self.documents.lock().unwrap();
                let selections = self.selections.lock().unwrap();
                let mut identities: Vec<_> =
                    documents.keys().chain(selections.keys()).cloned().collect();
                identities.sort();
                identities.dedup();
                let contracts: Vec<_> = identities.iter().map(|identity| {
                    let doc = documents.get(identity);
                    let enabled = selections.get(identity).copied();
                    let major = doc.map_or(enabled.unwrap_or(1), |doc| doc["contracts"][0]["major"].as_u64().unwrap() as u16);
                    json!({"contract":identity,"major":major,"minor":doc.map(|doc| doc["contracts"][0]["minor"].clone()),
                        "description":doc.map(|doc| doc["contracts"][0]["description"].clone()),
                        "available":doc.is_some() && enabled.is_none_or(|pin| pin == major),"enabled":enabled.is_some(),
                        "enabledMajor":enabled,"metadataSha256":doc.map(digest)})
                }).collect();
                Some(Ok(json!({"contracts":contracts})))
            }
            "enable" => {
                if let Some(code) = *self.refusal.lock().unwrap() {
                    return Some(fail(code));
                }
                let contract = request["contract"].as_str().unwrap_or_default();
                let major = request["major"].as_u64().unwrap_or(0) as u16;
                let documents = self.documents.lock().unwrap();
                if !documents
                    .get(contract)
                    .is_some_and(|doc| doc["contracts"][0]["major"] == major)
                {
                    return Some(fail("port_unavailable"));
                }
                self.selections
                    .lock()
                    .unwrap()
                    .insert(contract.into(), major);
                Some(Ok(
                    json!({"contract":contract,"major":major,"enabled":true}),
                ))
            }
            "disable" => {
                if let Some(code) = *self.refusal.lock().unwrap() {
                    return Some(fail(code));
                }
                let contract = request["contract"].as_str().unwrap_or_default();
                self.selections.lock().unwrap().remove(contract);
                Some(Ok(json!({"contract":contract,"enabled":false})))
            }
            "describe" => {
                let documents = self.documents.lock().unwrap();
                let Some(doc) = documents.get(request["contract"].as_str().unwrap_or_default())
                else {
                    return Some(fail("port_unavailable"));
                };
                let bytes = serde_json::to_vec(doc).unwrap();
                let offset = request["offset"].as_u64().unwrap() as usize;
                let end = (offset + 97).min(bytes.len()); // Force multiple actual SDK chunks.
                if request["metadataSha256"] != digest(doc) || offset >= bytes.len() {
                    return Some(fail("port_catalog_changed"));
                }
                let mut answer = json!({"metadataSha256":digest(doc),"totalBytes":bytes.len(),"offset":offset,"data":STANDARD.encode(&bytes[offset..end])});
                if end < bytes.len() {
                    answer["nextOffset"] = json!(end);
                }
                match *self.corruption.lock().unwrap() {
                    Some("offset") => answer["offset"] = json!(offset + 1),
                    Some("digest") => {
                        answer["data"] = json!(STANDARD.encode(vec![b' '; end - offset]))
                    }
                    Some("total") => answer["totalBytes"] = json!(1024 * 1024 + 1),
                    Some("base64") => answer["data"] = json!("!"),
                    _ => {}
                }
                Some(Ok(answer))
            }
            _ => None,
        }
    }
}

pub fn digest(document: &Value) -> String {
    Sha256::digest(serde_json::to_vec(document).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
