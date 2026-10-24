//! Closed, bounded provided Port Contracts. Admission and the author CLI use the same compiler.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::{ContractError, ContractErrorCode};
use crate::identity::is_valid_package_identity;
use crate::json::strict_json;
use crate::ui_contract::{CompiledUiContract, compile_ui_contract, valid_operation_name};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_CONTRACTS: usize = 32;
const MAX_PORTS: usize = 256;

fn violation(rule: &'static str) -> ContractError {
    ContractError::new(ContractErrorCode::PortContractViolation, rule)
}

/// One of the three supported Port kinds. A streaming query is still a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    Command,
    Query,
    Event,
}

/// A described Port with its exact JSON Schema payload declarations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Port {
    pub name: String,
    pub kind: PortKind,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
}

/// One version of an independently named Port Contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvidedContract {
    pub identity: String,
    pub major: u16,
    pub minor: u16,
    pub description: String,
    pub ports: Vec<Port>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Document {
    schema_version: u16,
    #[serde(rename = "$defs")]
    definitions: Map<String, Value>,
    contracts: Vec<ProvidedContract>,
}

/// A compiled document and bounded metadata suitable for a read-only catalog.
#[derive(Debug)]
pub struct CompiledPortContracts {
    pub contracts: Vec<ProvidedContract>,
    pub definitions: Map<String, Value>,
    pub warnings: Vec<String>,
    validators: Vec<CompiledUiContract>,
}

impl CompiledPortContracts {
    /// Validates one declared request, response or event using the admission-compiled schema.
    /// # Errors
    /// Returns an error when the contract, port or payload is invalid.
    pub fn validate(
        &self,
        contract: &str,
        port: &str,
        direction: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        let index = self
            .contracts
            .iter()
            .position(|entry| entry.identity == contract)
            .ok_or_else(|| violation("port_contract.unknown_port"))?;
        let descriptor = self.contracts[index]
            .ports
            .iter()
            .find(|entry| entry.name == port)
            .ok_or_else(|| violation("port_contract.unknown_port"))?;
        let validator = &self.validators[index];
        let result = match (descriptor.kind, direction) {
            (PortKind::Event, "event") | (PortKind::Query, "event")
                if descriptor.event.is_some() =>
            {
                validator.validate_event(port, payload)
            }
            (PortKind::Command | PortKind::Query, "request") => {
                if descriptor.event.is_some() {
                    validator.validate_subscription(port, payload)
                } else {
                    validator.validate_request(port, payload)
                }
            }
            (PortKind::Command | PortKind::Query, "response") if descriptor.response.is_some() => {
                validator.validate_response(port, payload)
            }
            _ => return Err(violation("port_contract.unknown_direction")),
        };
        result.map_err(|_| violation("port_contract.payload_invalid"))
    }
}

/// Compiles all declared schemas using the UI contract's closed JSON Schema profile.
/// # Errors
/// Returns the first declaration or schema violation.
pub fn compile_port_contracts(bytes: &[u8]) -> Result<CompiledPortContracts, ContractError> {
    if bytes.len() > MAX_BYTES {
        return Err(violation("port_contract.too_large"));
    }
    let value = strict_json(bytes).map_err(|_| violation("port_contract.malformed"))?;
    let document: Document =
        serde_json::from_value(value).map_err(|_| violation("port_contract.malformed"))?;
    if document.schema_version != 1 {
        return Err(violation("port_contract.schema_version"));
    }
    if document.contracts.len() > MAX_CONTRACTS
        || document
            .contracts
            .iter()
            .map(|contract| contract.ports.len())
            .sum::<usize>()
            > MAX_PORTS
    {
        return Err(violation("port_contract.too_many_ports"));
    }
    let mut identities = BTreeSet::new();
    let mut validators = Vec::new();
    let mut warnings = Vec::new();
    for contract in &document.contracts {
        if !is_valid_package_identity(&contract.identity)
            || contract.major == 0
            || !identities.insert(&contract.identity)
        {
            return Err(violation("port_contract.identity"));
        }
        description(&contract.description)?;
        let mut names = BTreeSet::new();
        let mut requests = Map::new();
        let mut subscriptions = Map::new();
        for port in &contract.ports {
            if !valid_operation_name(&port.name) || !names.insert(&port.name) {
                return Err(violation("port_contract.port_name"));
            }
            description(&port.description)?;
            match port.kind {
                PortKind::Command | PortKind::Query => {
                    let Some(request) = &port.request else {
                        return Err(violation("port_contract.port_shape"));
                    };
                    match (&port.response, &port.event) {
                        (Some(response), None) => {
                            requests.insert(
                                port.name.clone(),
                                json!({"request":request,"response":response}),
                            );
                        }
                        (None, Some(event)) if port.kind == PortKind::Query => {
                            subscriptions.insert(
                                port.name.clone(),
                                json!({"request":request,"event":event}),
                            );
                        }
                        _ => return Err(violation("port_contract.port_shape")),
                    }
                }
                PortKind::Event => {
                    if port.request.is_some() || port.response.is_some() || port.event.is_none() {
                        return Err(violation("port_contract.port_shape"));
                    }
                    subscriptions.insert(
                        port.name.clone(),
                        json!({"request":true,"event":port.event}),
                    );
                }
            }
            // English grammar is advice, never a condition for admission.
            let last = port.name.rsplit('.').next().unwrap_or(&port.name);
            let suspicious = match port.kind {
                PortKind::Command => last.ends_with("ed"),
                PortKind::Query => last.starts_with("get_") || last.starts_with("read_"),
                PortKind::Event => !last.ends_with("ed"),
            };
            if suspicious {
                warnings.push(format!(
                    "port_contract.grammar: {}/{}",
                    contract.identity, port.name
                ));
            }
        }
        let compiled = compile_ui_contract(&serde_json::to_vec(&json!({"schemaVersion":1,"$defs":document.definitions,"requests":requests,"subscriptions":subscriptions})).map_err(|_| violation("port_contract.schema_invalid"))?)
            .map_err(|error| violation(match error.rule {
                "ui_contract.too_many_nodes" => "port_contract.too_many_nodes",
                "ui_contract.schema_too_deep" => "port_contract.schema_too_deep",
                "ui_contract.dynamic_reference" => "port_contract.dynamic_reference",
                "ui_contract.reference_not_local" => "port_contract.reference_not_local",
                "ui_contract.reference_missing" => "port_contract.reference_missing",
                "ui_contract.reference_cycle" => "port_contract.reference_cycle",
                _ => "port_contract.schema_invalid",
            }))?;
        validators.push(compiled);
    }
    if validators.is_empty() {
        compile_ui_contract(&serde_json::to_vec(&json!({"schemaVersion":1,"$defs":document.definitions,"requests":{},"subscriptions":{}})).map_err(|_| violation("port_contract.schema_invalid"))?)
            .map_err(|_| violation("port_contract.schema_invalid"))?;
    }
    Ok(CompiledPortContracts {
        contracts: document.contracts,
        definitions: document.definitions,
        warnings,
        validators,
    })
}

fn description(value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > 512
        || value.chars().any(char::is_control)
    {
        return Err(violation("port_contract.description"));
    }
    Ok(())
}

/// Conservative additive-minor comparison over two independently parsed documents.
/// Existing port kinds and schemas must remain identical; descriptions may change.
/// # Errors
/// Returns a compatibility violation when an old port is removed or altered.
pub fn check_additive_minor(
    old: &ProvidedContract,
    new: &ProvidedContract,
) -> Result<(), ContractError> {
    // A standalone contract has no document definitions to compare. Fail closed
    // rather than accepting an unchanged $ref whose meaning changed elsewhere.
    if old.ports.iter().chain(&new.ports).any(|port| {
        [&port.request, &port.response, &port.event]
            .into_iter()
            .flatten()
            .any(contains_reference)
    }) {
        return Err(violation("port_contract.incompatible_minor"));
    }
    compare_port_shapes(old, new)
}

/// Compares versions with their independently compiled document definitions.
/// Existing referenced definitions must be identical; unrelated new definitions may be added.
/// # Errors
/// Returns an incompatibility when an existing Port or its referenced schema changes.
pub fn check_additive_minor_documents(
    old: &CompiledPortContracts,
    new: &CompiledPortContracts,
    identity: &str,
) -> Result<(), ContractError> {
    let old_contract = old
        .contracts
        .iter()
        .find(|entry| entry.identity == identity)
        .ok_or_else(|| violation("port_contract.incompatible_minor"))?;
    let new_contract = new
        .contracts
        .iter()
        .find(|entry| entry.identity == identity)
        .ok_or_else(|| violation("port_contract.incompatible_minor"))?;
    compare_port_shapes(old_contract, new_contract)?;
    let old_defs = Value::Object(old.definitions.clone());
    let new_defs = Value::Object(new.definitions.clone());
    let mut pending = Vec::new();
    for port in &old_contract.ports {
        for schema in [&port.request, &port.response, &port.event]
            .into_iter()
            .flatten()
        {
            collect_references(schema, &mut pending);
        }
    }
    let mut checked = BTreeSet::new();
    while let Some(reference) = pending.pop() {
        if !checked.insert(reference.clone()) {
            continue;
        }
        let pointer = reference
            .strip_prefix("#/$defs/")
            .map(|encoded| format!("/{encoded}"))
            .ok_or_else(|| violation("port_contract.incompatible_minor"))?;
        let before = old_defs
            .pointer(&pointer)
            .ok_or_else(|| violation("port_contract.incompatible_minor"))?;
        if Some(before) != new_defs.pointer(&pointer) {
            return Err(violation("port_contract.incompatible_minor"));
        }
        collect_references(before, &mut pending);
    }
    Ok(())
}

fn contains_reference(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key("$ref") || fields.values().any(contains_reference)
        }
        Value::Array(items) => items.iter().any(contains_reference),
        _ => false,
    }
}

fn collect_references(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = fields.get("$ref").and_then(Value::as_str) {
                output.push(reference.to_owned());
            }
            for value in fields.values() {
                collect_references(value, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_references(item, output);
            }
        }
        _ => {}
    }
}

fn compare_port_shapes(
    old: &ProvidedContract,
    new: &ProvidedContract,
) -> Result<(), ContractError> {
    if old.identity != new.identity
        || old.major != new.major
        || old.minor >= new.minor
        || old.ports.iter().any(|port| {
            !new.ports.iter().any(|next| {
                next.name == port.name
                    && next.kind == port.kind
                    && next.request == port.request
                    && next.response == port.response
                    && next.event == port.event
            })
        })
    {
        return Err(violation("port_contract.incompatible_minor"));
    }
    Ok(())
}
