//! The Immutable Record Log capability, and a session that never skips a record.
//!
//! The session's rule is commit before credit: a record reaches [`RecordLogSink::ingest`] on the
//! blocking pool, and only a successful ingest returns the credit that lets the host send the
//! next one. A failed ingest closes the stream and resubscribes from the cursor the sink last
//! committed, so the same record is delivered again instead of being lost.

use std::sync::Arc;
use std::time::Duration;

use teamarness_package_protocol::JsonValue;

use super::{
    base64_field, decimal_field, empty, encode_base64, field, grant_for, invalid_request,
    invalid_response, is_lower_hex, string_field,
};
use crate::cancellation::CancellationToken;
use crate::capability::{CapabilitySubscription, StreamCredit};
use crate::{BackendError, CapabilityClient, Initialization, object};

/// The capability identity a manifest requires.
pub const IDENTITY: &str = "teamarness.synchronization.immutable-record-log";

/// The largest canonical payload one record carries, decoded.
const MAX_CANONICAL_BYTES: usize = 262_144;
/// The largest number of sealed records one `open` call may carry.
const MAX_OPEN_INPUTS: usize = 32;
/// The largest decoded total one `open` call may carry.
const MAX_OPEN_BYTES: usize = 512 * 1024;

macro_rules! hex_identity {
    ($name:ident, $length:literal, $what:literal) => {
        #[doc = concat!("A ", $what, ": ", stringify!($length), " lowercase hexadecimal characters.")]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Accepts a value the contract's grammar allows.
            ///
            /// # Errors
            ///
            /// Returns `package.invalid_capability_request` for anything else.
            pub fn parse(value: impl Into<String>) -> Result<Self, BackendError> {
                let value = value.into();
                if is_lower_hex(&value, $length) {
                    Ok(Self(value))
                } else {
                    Err(invalid_request())
                }
            }

            /// The contract's own representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            fn decode(value: &JsonValue, name: &str) -> Result<Self, BackendError> {
                let text = string_field(value, name)?;
                if is_lower_hex(&text, $length) {
                    Ok(Self(text))
                } else {
                    Err(invalid_response())
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

hex_identity!(RecordId, 64, "record identity");
hex_identity!(OrderKey, 82, "total order key");
hex_identity!(Cursor, 50, "log cursor");

/// What one committed `append` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendResult {
    pub record_id: RecordId,
    pub order_key: OrderKey,
    pub author: String,
    /// The base64url signature, as the contract carries it.
    pub signature: String,
    pub sealed_record: Vec<u8>,
}

/// One record the host opened for this Package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedRecord {
    pub record_id: RecordId,
    pub order_key: OrderKey,
    pub author: String,
    pub purpose: String,
    pub signature: String,
    pub canonical: Vec<u8>,
}

/// What became of one sealed record handed to `open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenResult {
    Ready(Box<OpenedRecord>),
    WaitingForHistoryAccess,
    WrongScope,
    Corrupt,
}

impl OpenResult {
    /// Every status the contract defines, in contract order.
    #[must_use]
    pub const fn statuses() -> [&'static str; 4] {
        [
            "ready",
            "waiting_for_history_access",
            "wrong_scope",
            "corrupt",
        ]
    }

    /// The contract's own status string for this result.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ready(_) => "ready",
            Self::WaitingForHistoryAccess => "waiting_for_history_access",
            Self::WrongScope => "wrong_scope",
            Self::Corrupt => "corrupt",
        }
    }
}

/// How much of the Workspace's history this Installation can serve right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    WaitingForHistoryAccess,
    Offline,
    CatchingUp,
    Current,
}

impl Availability {
    /// The contract's own string for this availability.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WaitingForHistoryAccess => "waiting_for_history_access",
            Self::Offline => "offline",
            Self::CatchingUp => "catching_up",
            Self::Current => "current",
        }
    }

    /// Every value the contract defines, in contract order.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [
            Self::WaitingForHistoryAccess,
            Self::Offline,
            Self::CatchingUp,
            Self::Current,
        ]
    }

    fn parse(value: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|entry| entry.as_str() == value)
    }
}

/// One record delivered on the subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordEvent {
    pub cursor: Cursor,
    pub record_id: RecordId,
    pub order_key: OrderKey,
    pub author: String,
    pub purpose: String,
    pub signature: String,
    pub canonical: Vec<u8>,
    pub sealed_record: Vec<u8>,
}

/// What the host reports about synchronization itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncState {
    pub availability: Availability,
    pub pending_record_count: u64,
}

/// A condition the host wants the Package to know about, named by a stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncWarning {
    pub code: String,
}

/// Anything the subscription delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordLogEvent {
    Record(Box<RecordEvent>),
    State(SyncState),
    Warning(SyncWarning),
}

/// A typed client for `teamarness.synchronization.immutable-record-log`.
#[derive(Clone)]
pub struct RecordLog {
    client: CapabilityClient,
    grant: String,
    minor: u16,
}

impl RecordLog {
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
            minor: initialization
                .capability(IDENTITY)
                .ok_or_else(super::not_granted)?
                .minor,
            client,
        })
    }

    /// Appends one record under a Package-chosen purpose.
    ///
    /// # Errors
    ///
    /// Returns `package.invalid_capability_request` for a purpose or payload the contract
    /// refuses, the host's capability error, or `package.invalid_capability_response`.
    pub async fn append(
        &self,
        purpose: &str,
        canonical: &[u8],
    ) -> Result<AppendResult, BackendError> {
        self.append_with_principal(purpose, canonical, None).await
    }

    /// Appends as a locally bound Agent (record-log 1.1 only).
    pub async fn append_with_principal(
        &self,
        purpose: &str,
        canonical: &[u8],
        principal: Option<&str>,
    ) -> Result<AppendResult, BackendError> {
        if let Some(id) = principal
            && (self.minor < 1 || !is_lower_hex(id, 64))
        {
            return Err(invalid_request());
        }
        check_purpose(purpose)?;
        if canonical.is_empty() || canonical.len() > MAX_CANONICAL_BYTES {
            return Err(invalid_request());
        }
        let answer = self
            .client
            .call(&self.grant, "append", {
                let mut fields = serde_json::Map::new();
                fields.insert("purpose".into(), JsonValue::String(purpose.to_owned()));
                fields.insert(
                    "canonical".into(),
                    JsonValue::String(encode_base64(canonical)),
                );
                if let Some(id) = principal {
                    fields.insert("principal".into(), JsonValue::String(id.to_owned()));
                }
                JsonValue::Object(fields)
            })
            .await?;
        if string_field(&answer, "durability")? != "committed" {
            return Err(invalid_response());
        }
        Ok(AppendResult {
            record_id: RecordId::decode(&answer, "recordId")?,
            order_key: OrderKey::decode(&answer, "orderKey")?,
            author: string_field(&answer, "author")?,
            signature: string_field(&answer, "signature")?,
            sealed_record: base64_field(&answer, "sealedRecord")?,
        })
    }

    /// Opens sealed records this Installation received from elsewhere.
    ///
    /// # Errors
    ///
    /// Returns `package.invalid_capability_request` when the batch breaks the contract's bounds
    /// — before any call is made — the host's capability error, or
    /// `package.invalid_capability_response`.
    pub async fn open(&self, sealed_records: &[Vec<u8>]) -> Result<Vec<OpenResult>, BackendError> {
        if sealed_records.is_empty() || sealed_records.len() > MAX_OPEN_INPUTS {
            return Err(invalid_request());
        }
        let mut total = 0_usize;
        for record in sealed_records {
            if record.is_empty() {
                return Err(invalid_request());
            }
            total = total
                .checked_add(record.len())
                .filter(|total| *total <= MAX_OPEN_BYTES)
                .ok_or_else(invalid_request)?;
        }
        let inputs = sealed_records
            .iter()
            .map(|record| JsonValue::String(encode_base64(record)))
            .collect::<Vec<_>>();
        let answer = self
            .client
            .call(
                &self.grant,
                "open",
                object([("sealedRecords", JsonValue::Array(inputs))]),
            )
            .await?;
        field(&answer, "results")?
            .as_array()
            .ok_or_else(invalid_response)?
            .iter()
            .map(decode_open_result)
            .collect()
    }

    /// Subscribes to records after a cursor, or from the beginning when none is given.
    ///
    /// # Errors
    ///
    /// Returns the host's capability error.
    pub async fn subscribe(
        &self,
        after: Option<&Cursor>,
    ) -> Result<RecordLogSubscription, BackendError> {
        let after = after.map_or(JsonValue::Null, |cursor| {
            JsonValue::String(cursor.as_str().to_owned())
        });
        let inner = self
            .client
            .subscribe(
                &self.grant,
                object([
                    ("method", JsonValue::String("subscribe".into())),
                    ("afterCursor", after),
                ]),
            )
            .await?;
        Ok(RecordLogSubscription { inner })
    }
}

/// An open record-log stream.
pub struct RecordLogSubscription {
    inner: CapabilitySubscription,
}

impl RecordLogSubscription {
    /// The next delivery, `None` once the stream ends.
    ///
    /// The host sends nothing further until the delivery is acknowledged.
    pub async fn next(&mut self) -> Option<Result<Delivery, BackendError>> {
        match self.inner.recv_with_credit().await? {
            Ok((payload, credit)) => Some(decode_event(&payload).map(|event| Delivery {
                event,
                credit: Some(credit),
            })),
            Err(error) => Some(Err(error)),
        }
    }

    /// Closes the stream.
    pub async fn close(&mut self) {
        self.inner.close().await;
    }
}

/// One event, and the credit it is holding.
pub struct Delivery {
    event: RecordLogEvent,
    credit: Option<StreamCredit>,
}

impl Delivery {
    /// What the host delivered.
    #[must_use]
    pub const fn event(&self) -> &RecordLogEvent {
        &self.event
    }

    /// Takes the event, leaving the credit unreleased.
    #[must_use]
    pub fn into_event(self) -> RecordLogEvent {
        self.event
    }

    /// Accepts the event, letting the host send the next one.
    pub async fn acknowledge(mut self) {
        if let Some(credit) = self.credit.take() {
            credit.release().await;
        }
    }
}

/// Where a session commits what it receives.
///
/// Every method that can be slow runs on the blocking pool, so a synchronous store is the
/// ordinary implementation.
pub trait RecordLogSink: Send + Sync + 'static {
    /// The cursor of the last record this sink committed.
    ///
    /// # Errors
    ///
    /// Returns a Package-owned error; the session reports it and retries.
    fn committed_cursor(&self) -> Result<Option<Cursor>, BackendError>;

    /// Commits one record together with its cursor, in one transaction.
    ///
    /// # Errors
    ///
    /// Returns a Package-owned error; the session redelivers the record.
    fn ingest(&self, record: &RecordEvent) -> Result<(), BackendError>;

    /// Clears the stored cursor, after the host reported it invalid.
    ///
    /// # Errors
    ///
    /// Returns a Package-owned error; the session reports it and retries.
    fn reset_cursor(&self) -> Result<(), BackendError>;

    /// The host's availability changed.
    fn on_state(&self, _state: &SyncState) {}

    /// The host reported a condition worth surfacing.
    fn on_warning(&self, _warning: &SyncWarning) {}

    /// The session failed and will retry.
    fn on_session_error(&self, _error: &BackendError) {}
}

/// How long a session waits before resubscribing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOptions {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Runs the record log session until the token is cancelled.
///
/// The loop reads the sink's committed cursor, subscribes after it, and credits each delivery
/// only once the sink has taken it. Any failure closes the stream, reports itself, and
/// resubscribes from the committed cursor after a doubling backoff. A `cursor_invalid` close
/// clears the cursor and replays from the beginning at once, which record identity makes
/// idempotent; a second one in a row is reported instead.
pub async fn run_session(
    log: RecordLog,
    sink: Arc<dyn RecordLogSink>,
    cancellation: CancellationToken,
    options: SessionOptions,
) {
    let mut backoff = options.initial_backoff;
    loop {
        if cancellation.is_cancelled() {
            return;
        }
        let cursor = {
            let sink = sink.clone();
            crate::blocking(&cancellation, move |_| sink.committed_cursor()).await
        };
        let cursor = match cursor {
            Ok(cursor) => cursor,
            Err(error) => {
                if cancellation.is_cancelled() {
                    return;
                }
                sink.on_session_error(&error);
                if !back_off(&cancellation, &mut backoff, options).await {
                    return;
                }
                continue;
            }
        };
        let started_from_cursor = cursor.is_some();
        let mut subscription = match log.subscribe(cursor.as_ref()).await {
            Ok(subscription) => subscription,
            Err(error) => {
                if cancellation.is_cancelled() {
                    return;
                }
                sink.on_session_error(&error);
                if !back_off(&cancellation, &mut backoff, options).await {
                    return;
                }
                continue;
            }
        };
        let mut progressed = false;
        let outcome = deliver(&mut subscription, &sink, &cancellation, &mut progressed).await;
        subscription.close().await;
        if progressed {
            backoff = options.initial_backoff;
        }
        match outcome {
            Outcome::Cancelled => return,
            Outcome::Ended => {
                if !back_off(&cancellation, &mut backoff, options).await {
                    return;
                }
            }
            Outcome::CursorInvalid => {
                let reset = {
                    let sink = sink.clone();
                    crate::blocking(&cancellation, move |_| sink.reset_cursor()).await
                };
                if let Err(error) = reset {
                    if cancellation.is_cancelled() {
                        return;
                    }
                    sink.on_session_error(&error);
                    if !back_off(&cancellation, &mut backoff, options).await {
                        return;
                    }
                    continue;
                }
                if started_from_cursor {
                    // The stored cursor belonged to another journal. Replay from the beginning
                    // immediately; ingest is idempotent by record identity.
                    backoff = options.initial_backoff;
                    continue;
                }
                sink.on_session_error(&BackendError::new("cursor_invalid", false, empty()));
                if !back_off(&cancellation, &mut backoff, options).await {
                    return;
                }
            }
            Outcome::Failed(error) => {
                sink.on_session_error(&error);
                if !back_off(&cancellation, &mut backoff, options).await {
                    return;
                }
            }
        }
    }
}

/// Why one subscription ended.
enum Outcome {
    Cancelled,
    Ended,
    CursorInvalid,
    Failed(BackendError),
}

async fn deliver(
    subscription: &mut RecordLogSubscription,
    sink: &Arc<dyn RecordLogSink>,
    cancellation: &CancellationToken,
    progressed: &mut bool,
) -> Outcome {
    loop {
        let delivery = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Outcome::Cancelled,
            delivery = subscription.next() => delivery,
        };
        let Some(delivery) = delivery else {
            return Outcome::Ended;
        };
        let delivery = match delivery {
            Ok(delivery) => delivery,
            // The host's own `cursor_invalid` is the one close the session answers itself.
            Err(error) if error.code() == "cursor_invalid" => return Outcome::CursorInvalid,
            Err(error) => return Outcome::Failed(error),
        };
        match delivery.event() {
            RecordLogEvent::Record(record) => {
                let record = record.clone();
                let sink = sink.clone();
                let ingested = crate::blocking(cancellation, move |_| sink.ingest(&record)).await;
                if let Err(error) = ingested {
                    if cancellation.is_cancelled() {
                        return Outcome::Cancelled;
                    }
                    // The credit is never released, so this record is delivered again.
                    return Outcome::Failed(error);
                }
                *progressed = true;
            }
            RecordLogEvent::State(state) => sink.on_state(state),
            RecordLogEvent::Warning(warning) => sink.on_warning(warning),
        }
        delivery.acknowledge().await;
    }
}

/// Sleeps the current backoff, then doubles it. Returns `false` when cancelled.
async fn back_off(
    cancellation: &CancellationToken,
    backoff: &mut Duration,
    options: SessionOptions,
) -> bool {
    let waited = *backoff;
    *backoff = (*backoff * 2).min(options.max_backoff);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => false,
        () = tokio::time::sleep(waited) => true,
    }
}

fn decode_event(payload: &JsonValue) -> Result<RecordLogEvent, BackendError> {
    match string_field(payload, "kind")?.as_str() {
        "record" => Ok(RecordLogEvent::Record(Box::new(RecordEvent {
            cursor: Cursor::decode(payload, "cursor")?,
            record_id: RecordId::decode(payload, "recordId")?,
            order_key: OrderKey::decode(payload, "orderKey")?,
            author: string_field(payload, "author")?,
            purpose: string_field(payload, "purpose")?,
            signature: string_field(payload, "signature")?,
            canonical: base64_field(payload, "canonical")?,
            sealed_record: base64_field(payload, "sealedRecord")?,
        }))),
        "state" => Ok(RecordLogEvent::State(SyncState {
            availability: Availability::parse(&string_field(payload, "availability")?)
                .ok_or_else(invalid_response)?,
            pending_record_count: decimal_field(payload, "pendingRecordCount")?,
        })),
        "warning" => Ok(RecordLogEvent::Warning(SyncWarning {
            code: string_field(payload, "code")?,
        })),
        _ => Err(invalid_response()),
    }
}

fn decode_open_result(value: &JsonValue) -> Result<OpenResult, BackendError> {
    match string_field(value, "status")?.as_str() {
        "ready" => Ok(OpenResult::Ready(Box::new(OpenedRecord {
            record_id: RecordId::decode(value, "recordId")?,
            order_key: OrderKey::decode(value, "orderKey")?,
            author: string_field(value, "author")?,
            purpose: string_field(value, "purpose")?,
            signature: string_field(value, "signature")?,
            canonical: base64_field(value, "canonical")?,
        }))),
        "waiting_for_history_access" => Ok(OpenResult::WaitingForHistoryAccess),
        "wrong_scope" => Ok(OpenResult::WrongScope),
        "corrupt" => Ok(OpenResult::Corrupt),
        _ => Err(invalid_response()),
    }
}

/// The contract's purpose grammar: 1 to 64 printable ASCII characters.
fn check_purpose(purpose: &str) -> Result<(), BackendError> {
    if (1..=64).contains(&purpose.len())
        && purpose.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        Ok(())
    } else {
        Err(invalid_request())
    }
}
