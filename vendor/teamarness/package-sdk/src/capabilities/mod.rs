//! Typed clients for the version 1 Semantic Capabilities.
//!
//! Each client turns one grant from [`Initialization`] into methods whose request and response
//! shapes follow `contracts/package-capabilities/v1/*.schema.json` exactly. A response that does
//! not match its schema is `package.invalid_capability_response`: the Package never sees a
//! half-decoded value.

pub mod agent_binding;
pub mod content_views;
pub mod membership;
pub mod principals;
pub mod record_log;
pub mod signing;
pub mod workspace_context;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use teamarness_package_protocol::JsonValue;

use crate::{BackendError, Initialization};

/// An empty bounded details object.
pub(crate) fn empty() -> JsonValue {
    JsonValue::Object(serde_json::Map::new())
}

/// The host granted no such capability to this generation.
pub(crate) fn not_granted() -> BackendError {
    BackendError::new("package.capability_not_granted", false, empty())
}

/// The Package asked for something the capability contract does not allow.
pub(crate) fn invalid_request() -> BackendError {
    BackendError::new("package.invalid_capability_request", false, empty())
}

/// The host answered with something the capability schema does not describe.
pub(crate) fn invalid_response() -> BackendError {
    BackendError::new("package.invalid_capability_response", false, empty())
}

/// The opaque grant for one capability identity.
pub(crate) fn grant_for(
    initialization: &Initialization,
    identity: &str,
) -> Result<String, BackendError> {
    initialization
        .capability(identity)
        .map(|grant| grant.grant.clone())
        .ok_or_else(not_granted)
}

pub(crate) fn field<'a>(value: &'a JsonValue, name: &str) -> Result<&'a JsonValue, BackendError> {
    value.get(name).ok_or_else(invalid_response)
}

pub(crate) fn string_field(value: &JsonValue, name: &str) -> Result<String, BackendError> {
    field(value, name)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(invalid_response)
}

pub(crate) fn bool_field(value: &JsonValue, name: &str) -> Result<bool, BackendError> {
    field(value, name)?.as_bool().ok_or_else(invalid_response)
}

/// A non-negative count the contract carries as a decimal string.
pub(crate) fn decimal_field(value: &JsonValue, name: &str) -> Result<u64, BackendError> {
    let text = string_field(value, name)?;
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_response());
    }
    text.parse().map_err(|_| invalid_response())
}

pub(crate) fn decode_base64(value: &str) -> Result<Vec<u8>, BackendError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| invalid_response())
}

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether a value is exactly `length` lowercase hexadecimal characters.
pub(crate) fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A base64url value the contract describes, decoded and bounded by its own rule.
pub(crate) fn base64_field(value: &JsonValue, name: &str) -> Result<Vec<u8>, BackendError> {
    decode_base64(&string_field(value, name)?)
}
