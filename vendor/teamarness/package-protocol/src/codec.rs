use std::{collections::BTreeMap, error::Error, fmt};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};

use crate::{MAX_DECODED_FRAME_BYTES, MAX_FRAME_BYTES, MAX_WRITER_BYTES};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum FrameKind {
    Initialize,
    Initialized,
    Request,
    Response,
    Error,
    Cancel,
    StreamOpen,
    Event,
    Credit,
    StreamClose,
    Ping,
    Pong,
    Shutdown,
    ShutdownAck,
}

impl FrameKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Initialized => "initialized",
            Self::Request => "request",
            Self::Response => "response",
            Self::Error => "error",
            Self::Cancel => "cancel",
            Self::StreamOpen => "stream_open",
            Self::Event => "event",
            Self::Credit => "credit",
            Self::StreamClose => "stream_close",
            Self::Ping => "ping",
            Self::Pong => "pong",
            Self::Shutdown => "shutdown",
            Self::ShutdownAck => "shutdown_ack",
        }
    }

    fn parse(value: &str) -> Result<Self, ProtocolError> {
        match value {
            "initialize" => Ok(Self::Initialize),
            "initialized" => Ok(Self::Initialized),
            "request" => Ok(Self::Request),
            "response" => Ok(Self::Response),
            "error" => Ok(Self::Error),
            "cancel" => Ok(Self::Cancel),
            "stream_open" => Ok(Self::StreamOpen),
            "event" => Ok(Self::Event),
            "credit" => Ok(Self::Credit),
            "stream_close" => Ok(Self::StreamClose),
            "ping" => Ok(Self::Ping),
            "pong" => Ok(Self::Pong),
            "shutdown" => Ok(Self::Shutdown),
            "shutdown_ack" => Ok(Self::ShutdownAck),
            _ => Err(ProtocolError::new("unknown_kind")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    kind: FrameKind,
    fields: Map<String, Value>,
}

impl Frame {
    pub fn kind(&self) -> FrameKind {
        self.kind
    }

    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    pub fn as_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }
}

impl TryFrom<Value> for Frame {
    type Error = ProtocolError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let payload =
            serde_json::to_vec(&value).map_err(|_| ProtocolError::new("malformed_frame"))?;
        decode_payload(&payload)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolError {
    code: &'static str,
}

impl ProtocolError {
    pub(crate) const fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl Error for ProtocolError {}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffered: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.buffered.len().saturating_add(bytes.len()) > MAX_WRITER_BYTES {
            return Err(ProtocolError::new("overloaded"));
        }
        self.buffered.extend_from_slice(bytes);
        self.validate_available_prefix()
    }

    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        self.validate_available_prefix()?;
        let Some(prefix) = self.buffered.get(..4) else {
            return Ok(None);
        };
        let length = u32::from_be_bytes(prefix.try_into().expect("four-byte slice")) as usize;
        let total = length + 4;
        if self.buffered.len() < total {
            return Ok(None);
        }
        let remaining = self.buffered.split_off(total);
        let encoded = std::mem::replace(&mut self.buffered, remaining);
        decode_frame(&encoded).map(Some)
    }

    fn validate_available_prefix(&self) -> Result<(), ProtocolError> {
        let Some(prefix) = self.buffered.get(..4) else {
            return Ok(());
        };
        let length = u32::from_be_bytes(prefix.try_into().expect("four-byte slice")) as usize;
        if length == 0 {
            return Err(ProtocolError::new("zero_length"));
        }
        if length > MAX_FRAME_BYTES {
            return Err(ProtocolError::new("frame_too_large"));
        }
        Ok(())
    }
}

pub fn decode_frame(bytes: &[u8]) -> Result<Frame, ProtocolError> {
    let prefix: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| ProtocolError::new("truncated_frame"))?
        .try_into()
        .expect("four-byte slice");
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Err(ProtocolError::new("zero_length"));
    }
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError::new("frame_too_large"));
    }
    if bytes.len() != length + 4 {
        return Err(ProtocolError::new("truncated_frame"));
    }
    decode_payload(&bytes[4..])
}

pub fn decode_payload(payload: &[u8]) -> Result<Frame, ProtocolError> {
    if payload.is_empty() {
        return Err(ProtocolError::new("zero_length"));
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::new("frame_too_large"));
    }
    std::str::from_utf8(payload).map_err(|_| ProtocolError::new("invalid_utf8"))?;

    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    let value = StrictValue::deserialize(&mut deserializer).map_err(|error| {
        if error.to_string().contains("duplicate key") {
            ProtocolError::new("duplicate_key")
        } else {
            ProtocolError::new("malformed_json")
        }
    })?;
    deserializer
        .end()
        .map_err(|_| ProtocolError::new("malformed_json"))?;
    let Value::Object(fields) = value.0 else {
        return Err(ProtocolError::new("malformed_frame"));
    };
    let decoded_size = serde_json::to_vec(&Value::Object(fields.clone()))
        .map_err(|_| ProtocolError::new("malformed_frame"))?
        .len();
    if decoded_size > MAX_DECODED_FRAME_BYTES {
        return Err(ProtocolError::new("frame_too_large"));
    }
    let kind = fields
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
    let kind = FrameKind::parse(kind)?;
    validate_frame(kind, &fields)?;
    Ok(Frame { kind, fields })
}

fn validate_frame(kind: FrameKind, fields: &Map<String, Value>) -> Result<(), ProtocolError> {
    let allowed: &[&str] = match kind {
        FrameKind::Initialize => &[
            "kind",
            "nonce",
            "package",
            "uiContractDigest",
            "processGeneration",
            "workspace",
            "installation",
            "dataDirectory",
            "protocol",
            "capabilities",
            "limits",
        ],
        FrameKind::Initialized => &[
            "kind",
            "nonce",
            "package",
            "uiContractDigest",
            "processGeneration",
            "protocol",
        ],
        FrameKind::Request => &[
            "kind",
            "id",
            "operation",
            "grant",
            "mountId",
            "deadlineUnixMs",
            "payload",
        ],
        FrameKind::Response => &["kind", "id", "payload"],
        FrameKind::Error => &[
            "kind",
            "id",
            "code",
            "retryable",
            "outcomeUncertain",
            "details",
        ],
        FrameKind::Cancel => &["kind", "id", "targetId"],
        FrameKind::StreamOpen => &["kind", "id", "topic", "mountId", "initialCredit", "payload"],
        FrameKind::Event => &["kind", "streamId", "sequence", "payload"],
        FrameKind::Credit => &["kind", "streamId", "amount"],
        FrameKind::StreamClose => &["kind", "streamId", "code"],
        FrameKind::Ping | FrameKind::Pong => &["kind", "nonce"],
        FrameKind::Shutdown => &["kind", "reason"],
        FrameKind::ShutdownAck => &["kind"],
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ProtocolError::new("malformed_frame"));
    }

    match kind {
        FrameKind::Initialize => {
            base64url(required_str(fields, "nonce")?)?;
            decimal_string(fields.get("processGeneration"))?;
            digest(required_str(fields, "uiContractDigest")?)?;
            validate_package(required_object(fields, "package")?)?;
            validate_version(required_object(fields, "protocol")?)?;
            let data = required_object(fields, "dataDirectory")?;
            required_str(data, "path")?;
            base64url(required_str(data, "continuityId")?)?;
            required_object(fields, "workspace")?;
            required_object(fields, "installation")?;
            fields
                .get("capabilities")
                .and_then(Value::as_array)
                .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
            required_object(fields, "limits")?;
        }
        FrameKind::Initialized => {
            base64url(required_str(fields, "nonce")?)?;
            decimal_string(fields.get("processGeneration"))?;
            digest(required_str(fields, "uiContractDigest")?)?;
            validate_package(required_object(fields, "package")?)?;
            validate_version(required_object(fields, "protocol")?)?;
        }
        FrameKind::Request => {
            identifier(required_str(fields, "id")?)?;
            token(required_str(fields, "operation")?)?;
            decimal_string(fields.get("deadlineUnixMs"))?;
            required_object(fields, "payload")?;
            for field in ["grant", "mountId"] {
                if let Some(value) = fields.get(field) {
                    base64url(
                        value
                            .as_str()
                            .ok_or_else(|| ProtocolError::new("invalid_base64url"))?,
                    )?;
                }
            }
        }
        FrameKind::Response => {
            identifier(required_str(fields, "id")?)?;
            required_object(fields, "payload")?;
        }
        FrameKind::Error => {
            identifier(required_str(fields, "id")?)?;
            token(required_str(fields, "code")?)?;
            required_bool(fields, "retryable")?;
            if fields
                .get("outcomeUncertain")
                .is_some_and(|value| !value.is_boolean())
                || fields
                    .get("details")
                    .is_some_and(|value| !value.is_object())
            {
                return Err(ProtocolError::new("malformed_frame"));
            }
        }
        FrameKind::Cancel => {
            identifier(required_str(fields, "id")?)?;
            identifier(required_str(fields, "targetId")?)?;
        }
        FrameKind::StreamOpen => {
            identifier(required_str(fields, "id")?)?;
            token(required_str(fields, "topic")?)?;
            required_object(fields, "payload")?;
            fields
                .get("initialCredit")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
            if let Some(value) = fields.get("mountId") {
                base64url(
                    value
                        .as_str()
                        .ok_or_else(|| ProtocolError::new("invalid_base64url"))?,
                )?;
            }
        }
        FrameKind::Event => {
            identifier(required_str(fields, "streamId")?)?;
            decimal_string(fields.get("sequence"))?;
            required_object(fields, "payload")?;
        }
        FrameKind::Credit => {
            identifier(required_str(fields, "streamId")?)?;
            fields
                .get("amount")
                .and_then(Value::as_u64)
                .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
        }
        FrameKind::StreamClose => {
            identifier(required_str(fields, "streamId")?)?;
            if let Some(value) = fields.get("code") {
                token(
                    value
                        .as_str()
                        .ok_or_else(|| ProtocolError::new("malformed_frame"))?,
                )?;
            }
        }
        FrameKind::Ping | FrameKind::Pong => base64url(required_str(fields, "nonce")?)?,
        FrameKind::Shutdown => token(required_str(fields, "reason")?)?,
        FrameKind::ShutdownAck => {}
    }
    Ok(())
}

fn required_str<'a>(fields: &'a Map<String, Value>, name: &str) -> Result<&'a str, ProtocolError> {
    fields
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))
}

fn required_object<'a>(
    fields: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Map<String, Value>, ProtocolError> {
    fields
        .get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))
}

fn required_bool(fields: &Map<String, Value>, name: &str) -> Result<(), ProtocolError> {
    fields
        .get(name)
        .and_then(Value::as_bool)
        .map(drop)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))
}

fn base64url(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty() || URL_SAFE_NO_PAD.decode(value).is_err() {
        return Err(ProtocolError::new("invalid_base64url"));
    }
    Ok(())
}

fn decimal_string(value: Option<&Value>) -> Result<(), ProtocolError> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::new("invalid_decimal"))?;
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ProtocolError::new("invalid_decimal"));
    }
    Ok(())
}

fn identifier(value: &str) -> Result<(), ProtocolError> {
    let valid_prefix = value.starts_with("h:") || value.starts_with("p:");
    if !valid_prefix
        || value.len() <= 2
        || value.len() > 128
        || !value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
    {
        return Err(ProtocolError::new("invalid_id_namespace"));
    }
    Ok(())
}

fn token(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
    {
        return Err(ProtocolError::new("malformed_frame"));
    }
    Ok(())
}

fn digest(value: &str) -> Result<(), ProtocolError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ProtocolError::new("malformed_frame"));
    }
    Ok(())
}

fn validate_package(value: &Map<String, Value>) -> Result<(), ProtocolError> {
    for field in ["identity", "version"] {
        required_str(value, field)?;
    }
    digest(required_str(value, "manifestDigest")?)?;
    digest(required_str(value, "backendDigest")?)
}

fn validate_version(value: &Map<String, Value>) -> Result<(), ProtocolError> {
    value
        .get("major")
        .and_then(Value::as_u64)
        .filter(|major| *major > 0)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
    value
        .get("minor")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProtocolError::new("malformed_frame"))?;
    Ok(())
}

pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    let payload =
        serde_json::to_vec(&frame.fields).map_err(|_| ProtocolError::new("malformed_frame"))?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::new("frame_too_large"));
    }
    let length = u32::try_from(payload.len()).expect("frame limit fits u32");
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format_args!(
                    "duplicate key `{key}`"
                )));
            }
            values.insert(key, map.next_value::<StrictValue>()?.0);
        }
        Ok(StrictValue(Value::Object(values.into_iter().collect())))
    }
}
