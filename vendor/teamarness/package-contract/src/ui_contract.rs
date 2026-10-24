//! The Package UI contract profile the Package Host admits.
//!
//! # Rule names
//!
//! | Rule | What it holds |
//! |---|---|
//! | `ui_contract.too_large` | the contract exceeds 1 MiB |
//! | `ui_contract.malformed` | not strict JSON, or an unknown or missing field |
//! | `ui_contract.schema_version` | `schemaVersion` is not `1` |
//! | `ui_contract.too_many_operations` | more than 128 per direction or 256 in total |
//! | `ui_contract.operation_name` | an operation name breaks the grammar |
//! | `ui_contract.too_many_nodes` | the contract has more than 16,384 schema nodes |
//! | `ui_contract.schema_too_deep` | a schema nests deeper than 64 |
//! | `ui_contract.dynamic_reference` | a schema uses `$dynamicRef`, `$dynamicAnchor`, `$recursiveRef` or `$recursiveAnchor` |
//! | `ui_contract.reference_not_local` | a `$ref` is not a `#/$defs/<name>` pointer |
//! | `ui_contract.reference_missing` | a `$ref` names a definition that does not exist |
//! | `ui_contract.reference_cycle` | the definitions reference each other in a cycle |
//! | `ui_contract.schema_invalid` | a schema is not a valid Draft 2020-12 schema |
//! | `ui_contract.unknown_operation` | the operation is not declared in this contract |
//! | `ui_contract.payload_too_large` | the payload exceeds 256 KiB or nests deeper than 64 |
//! | `ui_contract.payload_invalid` | the payload does not validate against its schema |

use std::collections::{BTreeMap, BTreeSet};

use jsonschema::{Draft, Validator};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::{ContractError, ContractErrorCode};
use crate::json::strict_json;

/// The largest UI payload the Host forwards in either direction.
pub const MAX_UI_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_CONTRACT_BYTES: usize = 1024 * 1024;
const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_SCHEMA_NODES: usize = 16_384;
const MAX_OPERATIONS_PER_DIRECTION: usize = 128;
const MAX_OPERATIONS: usize = 256;

/// A compiled, admitted UI contract.
pub struct CompiledUiContract {
    requests: BTreeMap<String, DirectionalRequest>,
    subscriptions: BTreeMap<String, DirectionalSubscription>,
}

impl std::fmt::Debug for CompiledUiContract {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledUiContract")
            .field("requests", &self.requests.keys().collect::<Vec<_>>())
            .field(
                "subscriptions",
                &self.subscriptions.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

struct DirectionalRequest {
    request: Validator,
    response: Validator,
}

struct DirectionalSubscription {
    request: Validator,
    event: Validator,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContractDocument {
    schema_version: u16,
    #[serde(rename = "$defs")]
    definitions: Map<String, Value>,
    requests: BTreeMap<String, RequestDescriptor>,
    subscriptions: BTreeMap<String, SubscriptionDescriptor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestDescriptor {
    request: Value,
    response: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionDescriptor {
    request: Value,
    event: Value,
}

fn violation(rule: &'static str) -> ContractError {
    ContractError::new(ContractErrorCode::UiContractViolation, rule)
}

impl CompiledUiContract {
    /// Validates a UI request payload.
    ///
    /// # Errors
    ///
    /// Returns the rule the payload or operation breaks.
    pub fn validate_request(&self, operation: &str, payload: &Value) -> Result<(), ContractError> {
        let entry = self.requests.get(operation).ok_or_else(unknown_operation)?;
        validate_payload(&entry.request, payload)
    }

    /// Validates a UI response payload.
    ///
    /// # Errors
    ///
    /// Returns the rule the payload or operation breaks.
    pub fn validate_response(&self, operation: &str, payload: &Value) -> Result<(), ContractError> {
        let entry = self.requests.get(operation).ok_or_else(unknown_operation)?;
        validate_payload(&entry.response, payload)
    }

    /// Validates a subscription request payload.
    ///
    /// # Errors
    ///
    /// Returns the rule the payload or operation breaks.
    pub fn validate_subscription(
        &self,
        operation: &str,
        payload: &Value,
    ) -> Result<(), ContractError> {
        let entry = self
            .subscriptions
            .get(operation)
            .ok_or_else(unknown_operation)?;
        validate_payload(&entry.request, payload)
    }

    /// Validates a subscription event payload.
    ///
    /// # Errors
    ///
    /// Returns the rule the payload or operation breaks.
    pub fn validate_event(&self, operation: &str, payload: &Value) -> Result<(), ContractError> {
        let entry = self
            .subscriptions
            .get(operation)
            .ok_or_else(unknown_operation)?;
        validate_payload(&entry.event, payload)
    }

    /// The declared request operation names, in sorted order.
    pub fn request_operations(&self) -> impl Iterator<Item = &str> {
        self.requests.keys().map(String::as_str)
    }

    /// The declared subscription operation names, in sorted order.
    pub fn subscription_operations(&self) -> impl Iterator<Item = &str> {
        self.subscriptions.keys().map(String::as_str)
    }
}

fn unknown_operation() -> ContractError {
    violation("ui_contract.unknown_operation")
}

/// Compiles a UI contract document against the admitted profile.
///
/// # Errors
///
/// Returns the first profile rule the contract breaks.
pub fn compile_ui_contract(bytes: &[u8]) -> Result<CompiledUiContract, ContractError> {
    if bytes.len() > MAX_CONTRACT_BYTES {
        return Err(violation("ui_contract.too_large"));
    }
    let value = strict_json(bytes).map_err(|_| violation("ui_contract.malformed"))?;
    let document: ContractDocument =
        serde_json::from_value(value).map_err(|_| violation("ui_contract.malformed"))?;
    if document.schema_version != 1 {
        return Err(violation("ui_contract.schema_version").at("/schemaVersion"));
    }
    if document.requests.len() > MAX_OPERATIONS_PER_DIRECTION
        || document.subscriptions.len() > MAX_OPERATIONS_PER_DIRECTION
        || document.requests.len() + document.subscriptions.len() > MAX_OPERATIONS
    {
        return Err(violation("ui_contract.too_many_operations"));
    }
    for name in document
        .requests
        .keys()
        .chain(document.subscriptions.keys())
    {
        if !valid_operation_name(name) {
            return Err(violation("ui_contract.operation_name").at(format!("/requests/{name}")));
        }
    }

    let mut node_count = 0;
    let definition_names: BTreeSet<_> = document.definitions.keys().cloned().collect();
    let mut definition_edges = BTreeMap::new();
    for (name, schema) in &document.definitions {
        let mut references = BTreeSet::new();
        walk_schema(
            schema,
            0,
            &mut node_count,
            &definition_names,
            &mut references,
            &format!("/$defs/{name}"),
        )?;
        definition_edges.insert(name.clone(), references);
    }
    for (name, descriptor) in &document.requests {
        for (key, schema) in [
            ("request", &descriptor.request),
            ("response", &descriptor.response),
        ] {
            walk_schema(
                schema,
                0,
                &mut node_count,
                &definition_names,
                &mut BTreeSet::new(),
                &format!("/requests/{name}/{key}"),
            )?;
        }
    }
    for (name, descriptor) in &document.subscriptions {
        for (key, schema) in [
            ("request", &descriptor.request),
            ("event", &descriptor.event),
        ] {
            walk_schema(
                schema,
                0,
                &mut node_count,
                &definition_names,
                &mut BTreeSet::new(),
                &format!("/subscriptions/{name}/{key}"),
            )?;
        }
    }
    reject_reference_cycles(&definition_edges)?;

    let definitions = Value::Object(document.definitions);
    let requests = document
        .requests
        .into_iter()
        .map(|(name, descriptor)| {
            let pointer = format!("/requests/{name}");
            Ok((
                name,
                DirectionalRequest {
                    request: compile_schema(&definitions, descriptor.request, &pointer)?,
                    response: compile_schema(&definitions, descriptor.response, &pointer)?,
                },
            ))
        })
        .collect::<Result<_, ContractError>>()?;
    let subscriptions = document
        .subscriptions
        .into_iter()
        .map(|(name, descriptor)| {
            let pointer = format!("/subscriptions/{name}");
            Ok((
                name,
                DirectionalSubscription {
                    request: compile_schema(&definitions, descriptor.request, &pointer)?,
                    event: compile_schema(&definitions, descriptor.event, &pointer)?,
                },
            ))
        })
        .collect::<Result<_, ContractError>>()?;
    Ok(CompiledUiContract {
        requests,
        subscriptions,
    })
}

fn compile_schema(
    definitions: &Value,
    schema: Value,
    pointer: &str,
) -> Result<Validator, ContractError> {
    let wrapped = json!({"$defs": definitions, "allOf": [schema]});
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&wrapped)
        .map_err(|_| violation("ui_contract.schema_invalid").at(pointer.to_owned()))
}

/// Returns whether `name` matches the contract operation-name grammar.
#[must_use]
pub fn valid_operation_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 || !name.is_ascii() {
        return false;
    }
    name.split('.').all(|segment| {
        !segment.is_empty()
            && segment.as_bytes()[0].is_ascii_lowercase()
            && segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    })
}

fn walk_schema(
    value: &Value,
    depth: usize,
    node_count: &mut usize,
    definitions: &BTreeSet<String>,
    references: &mut BTreeSet<String>,
    pointer: &str,
) -> Result<(), ContractError> {
    *node_count += 1;
    if *node_count > MAX_SCHEMA_NODES {
        return Err(violation("ui_contract.too_many_nodes"));
    }
    if matches!(value, Value::Object(_) | Value::Array(_)) && depth > MAX_SCHEMA_DEPTH {
        return Err(violation("ui_contract.schema_too_deep").at(pointer.to_owned()));
    }
    match value {
        Value::Object(object) => {
            for forbidden in [
                "$dynamicRef",
                "$dynamicAnchor",
                "$recursiveRef",
                "$recursiveAnchor",
            ] {
                if object.contains_key(forbidden) {
                    return Err(violation("ui_contract.dynamic_reference")
                        .at(format!("{pointer}/{forbidden}")));
                }
            }
            if let Some(reference) = object.get("$ref") {
                let reference_pointer = format!("{pointer}/$ref");
                let text = reference.as_str().ok_or_else(|| {
                    violation("ui_contract.reference_not_local").at(reference_pointer.clone())
                })?;
                let target = definition_reference(text)
                    .map_err(|error| error.at(reference_pointer.clone()))?;
                if !definitions.contains(&target) {
                    return Err(
                        violation("ui_contract.reference_missing").at(reference_pointer.clone())
                    );
                }
                references.insert(target);
            }
            for (key, child) in object {
                walk_schema(
                    child,
                    depth + 1,
                    node_count,
                    definitions,
                    references,
                    &format!("{pointer}/{}", escape_pointer_token(key)),
                )?;
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                walk_schema(
                    child,
                    depth + 1,
                    node_count,
                    definitions,
                    references,
                    &format!("{pointer}/{index}"),
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn definition_reference(reference: &str) -> Result<String, ContractError> {
    let encoded = reference
        .strip_prefix("#/$defs/")
        .ok_or_else(|| violation("ui_contract.reference_not_local"))?;
    if encoded.is_empty() || encoded.contains('/') {
        return Err(violation("ui_contract.reference_not_local"));
    }
    let mut decoded = String::new();
    let mut chars = encoded.chars();
    while let Some(character) = chars.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            _ => return Err(violation("ui_contract.reference_not_local")),
        }
    }
    Ok(decoded)
}

fn reject_reference_cycles(
    references: &BTreeMap<String, BTreeSet<String>>,
) -> Result<(), ContractError> {
    fn visit(
        node: &str,
        references: &BTreeMap<String, BTreeSet<String>>,
        active: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> bool {
        if active.contains(node) {
            return false;
        }
        if complete.contains(node) {
            return true;
        }
        active.insert(node.to_owned());
        if references.get(node).is_some_and(|targets| {
            targets
                .iter()
                .any(|target| !visit(target, references, active, complete))
        }) {
            return false;
        }
        active.remove(node);
        complete.insert(node.to_owned());
        true
    }
    let mut active = BTreeSet::new();
    let mut complete = BTreeSet::new();
    for node in references.keys() {
        if !visit(node, references, &mut active, &mut complete) {
            return Err(violation("ui_contract.reference_cycle").at(format!("/$defs/{node}")));
        }
    }
    Ok(())
}

fn validate_payload(validator: &Validator, payload: &Value) -> Result<(), ContractError> {
    let bytes =
        serde_json::to_vec(payload).map_err(|_| violation("ui_contract.payload_invalid"))?;
    if bytes.len() > MAX_UI_PAYLOAD_BYTES || value_depth(payload, 0)? > MAX_SCHEMA_DEPTH {
        return Err(violation("ui_contract.payload_too_large"));
    }
    validator
        .validate(payload)
        .map_err(|_| violation("ui_contract.payload_invalid"))
}

fn value_depth(value: &Value, depth: usize) -> Result<usize, ContractError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(violation("ui_contract.payload_too_large"));
    }
    let child_depth = match value {
        Value::Array(values) => values
            .iter()
            .map(|child| value_depth(child, depth + 1))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max(),
        Value::Object(values) => values
            .values()
            .map(|child| value_depth(child, depth + 1))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max(),
        _ => None,
    };
    Ok(child_depth.unwrap_or(depth))
}
