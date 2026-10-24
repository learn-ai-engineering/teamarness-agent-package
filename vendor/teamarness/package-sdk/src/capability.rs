use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teamarness_package_protocol::{INITIAL_STREAM_CREDIT, JsonValue};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::priority_writer::{PriorityWriter, SharedState};
use crate::{BackendError, object};

/// How long a capability call waits for the Host's answer.
///
/// One second longer than the Host's own call deadline, so a Host that is
/// answering slowly reports its own `deadline_exceeded` first.
pub(crate) const CALL_DEADLINE: Duration = Duration::from_secs(11);

pub(crate) type PendingCapabilities = Arc<Mutex<BTreeMap<String, PendingCapability>>>;

pub(crate) struct PendingCapability {
    pub(crate) sender: oneshot::Sender<Result<JsonValue, BackendError>>,
    pub(crate) command: bool,
}

pub(crate) type CapabilityStreams = Arc<Mutex<BTreeMap<String, CapabilityStreamRoute>>>;

pub(crate) struct CapabilityStreamRoute {
    pub(crate) sender: mpsc::Sender<Result<JsonValue, BackendError>>,
}

/// A cloneable client for generation-scoped Semantic Capability calls.
#[derive(Clone)]
pub struct CapabilityClient {
    writer: Arc<PriorityWriter>,
    pending: PendingCapabilities,
    next_id: Arc<AtomicU64>,
    streams: CapabilityStreams,
    state: SharedState,
}

/// A credit-controlled stream opened through a Semantic Capability.
pub struct CapabilitySubscription {
    id: String,
    receiver: mpsc::Receiver<Result<JsonValue, BackendError>>,
    writer: Arc<PriorityWriter>,
    streams: CapabilityStreams,
    state: SharedState,
    closed: bool,
    port_values: bool,
}

impl CapabilitySubscription {
    pub(crate) fn with_port_values(mut self) -> Self {
        self.port_values = true;
        self
    }

    /// Receives one event, returning `None` after the host closes the stream.
    pub async fn recv(&mut self) -> Option<Result<JsonValue, BackendError>> {
        let event = self.receiver.recv().await;
        if matches!(event, Some(Ok(_))) {
            let _ = self
                .writer
                .control(object([
                    ("kind", JsonValue::String("credit".into())),
                    ("streamId", JsonValue::String(self.id.clone())),
                    ("amount", JsonValue::from(1_u64)),
                ]))
                .await;
        }
        event.map(|result| {
            result.map(|value| {
                if self.port_values {
                    crate::ports::decode_port_value(value)
                } else {
                    value
                }
            })
        })
    }

    /// Receives one event and the credit it consumed, so the caller decides when to ask for more.
    ///
    /// The host sends no further event until the returned [`StreamCredit`] is released, which is
    /// what lets a consumer commit an event before accepting the next one.
    pub async fn recv_with_credit(
        &mut self,
    ) -> Option<Result<(JsonValue, StreamCredit), BackendError>> {
        match self.receiver.recv().await? {
            Ok(payload) => Some(Ok((
                if self.port_values {
                    crate::ports::decode_port_value(payload)
                } else {
                    payload
                },
                StreamCredit {
                    id: self.id.clone(),
                    writer: self.writer.clone(),
                },
            ))),
            Err(error) => Some(Err(error)),
        }
    }

    /// Closes the stream and releases its registry entry.
    ///
    /// The stream is retired before the entry goes, so events the host wrote
    /// against credit it still held are ignored rather than read as events for
    /// a stream that does not exist.
    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.state.lock().await.retire_stream(&self.id);
        self.streams.lock().await.remove(&self.id);
        let _ = self
            .writer
            .control(object([
                ("kind", JsonValue::String("stream_close".into())),
                ("streamId", JsonValue::String(self.id.clone())),
                ("code", JsonValue::String("closed".into())),
            ]))
            .await;
    }
}

/// One event's worth of credit, still owed to the host.
///
/// Dropping it without releasing it keeps the window closed, which is how a consumer applies
/// back pressure while it commits what it has already received.
#[must_use = "the host sends no further event until this credit is released"]
pub struct StreamCredit {
    id: String,
    writer: Arc<PriorityWriter>,
}

impl StreamCredit {
    /// Returns the credit, allowing the host to send one more event.
    pub async fn release(self) {
        let _ = self
            .writer
            .control(object([
                ("kind", JsonValue::String("credit".into())),
                ("streamId", JsonValue::String(self.id)),
                ("amount", JsonValue::from(1_u64)),
            ]))
            .await;
    }
}

impl Drop for CapabilitySubscription {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let id = self.id.clone();
        let streams = self.streams.clone();
        let writer = self.writer.clone();
        let state = self.state.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                state.lock().await.retire_stream(&id);
                streams.lock().await.remove(&id);
                let _ = writer
                    .control(object([
                        ("kind", JsonValue::String("stream_close".into())),
                        ("streamId", JsonValue::String(id)),
                        ("code", JsonValue::String("dropped".into())),
                    ]))
                    .await;
            });
        }
    }
}

impl std::fmt::Debug for CapabilityClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapabilityClient")
            .finish_non_exhaustive()
    }
}

impl CapabilityClient {
    pub(crate) fn new(
        writer: Arc<PriorityWriter>,
        pending: PendingCapabilities,
        streams: CapabilityStreams,
        state: SharedState,
    ) -> Self {
        Self {
            writer,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            streams,
            state,
        }
    }

    /// Opens the generic capability stream with the supplied opaque grant.
    pub async fn subscribe(
        &self,
        grant: &str,
        payload: JsonValue,
    ) -> Result<CapabilitySubscription, BackendError> {
        let JsonValue::Object(mut payload) = payload else {
            return Err(invalid_request());
        };
        if payload
            .insert("grant".into(), JsonValue::String(grant.into()))
            .is_some()
        {
            return Err(invalid_request());
        }
        let id = format!(
            "p:cap-stream-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        // The route holds exactly the credit window the contract grants, so a
        // conforming host can never overrun it.
        let (sender, receiver) = mpsc::channel(INITIAL_STREAM_CREDIT);
        self.streams
            .lock()
            .await
            .insert(id.clone(), CapabilityStreamRoute { sender });
        let value = object([
            ("kind", JsonValue::String("stream_open".into())),
            ("id", JsonValue::String(id.clone())),
            ("topic", JsonValue::String("capability.stream".into())),
            (
                "initialCredit",
                JsonValue::from(INITIAL_STREAM_CREDIT as u64),
            ),
            ("payload", JsonValue::Object(payload)),
        ]);
        if self.writer.control(value).await.is_err() {
            self.streams.lock().await.remove(&id);
            return Err(unavailable());
        }
        Ok(CapabilitySubscription {
            id,
            receiver,
            writer: self.writer.clone(),
            streams: self.streams.clone(),
            state: self.state.clone(),
            closed: false,
            port_values: false,
        })
    }

    /// Routes one already-classified event to its stream.
    ///
    /// `ContractState` decides whether an event is legal, late or fatal before
    /// it reaches here, including its sequence. A route this side has already
    /// dropped is a race the classifier tolerated, so the event is discarded
    /// rather than reclassified. A full route is the one condition left, and a
    /// conforming host cannot produce it inside the credit window.
    pub(crate) async fn event(
        streams: &CapabilityStreams,
        id: &str,
        payload: JsonValue,
    ) -> Result<(), &'static str> {
        let mut routes = streams.lock().await;
        let Some(route) = routes.get(id) else {
            return Ok(());
        };
        if route.sender.try_send(Ok(payload)).is_err() {
            let route = routes.remove(id).expect("stream was present");
            // Preserve already accepted events, then report the local overflow.
            // Waiting for queue space on the reader would stall the entire session.
            tokio::spawn(async move {
                let _ = route
                    .sender
                    .send(Err(BackendError::new("event_lagged", false, object([]))))
                    .await;
            });
            return Err("event_lagged");
        }
        Ok(())
    }

    pub(crate) async fn close_stream(
        streams: &CapabilityStreams,
        id: &str,
        error: Option<BackendError>,
    ) {
        if let Some(route) = streams.lock().await.remove(id)
            && let Some(error) = error
        {
            // The terminal follows accepted events, but a full stream must not
            // suspend the session reader (or another stream's close).
            tokio::spawn(async move {
                let _ = route.sender.send(Err(error)).await;
            });
        }
    }

    /// Calls one method through an opaque grant supplied during initialization.
    pub async fn call(
        &self,
        grant: &str,
        method: &str,
        payload: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        let mut input = match payload {
            JsonValue::Object(input) => input,
            _ => {
                return Err(BackendError::new(
                    "capability_request_invalid",
                    false,
                    JsonValue::Object(Default::default()),
                ));
            }
        };
        if input
            .insert("method".into(), JsonValue::String(method.into()))
            .is_some()
        {
            return Err(BackendError::new(
                "capability_request_invalid",
                false,
                JsonValue::Object(Default::default()),
            ));
        }
        let command =
            method == "invoke" && input.get("kind").and_then(JsonValue::as_str) == Some("command");
        let id = format!("p:{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let deadline = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_add(10_000)
            .to_string();
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(id.clone(), PendingCapability { sender, command });
        let value = object([
            ("kind", JsonValue::String("request".into())),
            ("id", JsonValue::String(id.clone())),
            ("operation", JsonValue::String("capability.call".into())),
            ("grant", JsonValue::String(grant.into())),
            ("deadlineUnixMs", JsonValue::String(deadline)),
            ("payload", JsonValue::Object(input)),
        ]);
        if self.writer.data(value).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(unavailable());
        }
        match tokio::time::timeout(CALL_DEADLINE, receiver).await {
            Ok(answer) => answer.unwrap_or_else(|_| Err(unavailable_after_enqueue(command))),
            Err(_) => {
                // The caller stops waiting here, so the request is retired
                // here: the host always answers, and its late first terminal
                // frame is then ignored instead of read as an answer to a
                // request this side never made.
                self.pending.lock().await.remove(&id);
                self.state.lock().await.retire_request(&id);
                Err(BackendError::new(
                    if command {
                        "outcome_unknown"
                    } else {
                        "deadline_exceeded"
                    },
                    !command,
                    JsonValue::Object(Default::default()),
                )
                .with_outcome_uncertain(command))
            }
        }
    }
}

/// Fails every capability call and stream that is still waiting.
///
/// Called once the dispatcher has returned: nothing will answer them now, and a
/// Package task holding a client must not wait forever.
pub(crate) async fn fail_all(pending: &PendingCapabilities, streams: &CapabilityStreams) {
    let waiting = std::mem::take(&mut *pending.lock().await);
    for (_, pending) in waiting {
        let _ = pending
            .sender
            .send(Err(unavailable_after_enqueue(pending.command)));
    }
    let routes = std::mem::take(&mut *streams.lock().await);
    for (_, route) in routes {
        // A retained full receiver must not hold process shutdown open. The
        // terminal follows accepted events if that Consumer later drains them.
        tokio::spawn(async move {
            let _ = route.sender.send(Err(unavailable())).await;
        });
    }
}

fn invalid_request() -> BackendError {
    BackendError::new(
        "capability_request_invalid",
        false,
        JsonValue::Object(Default::default()),
    )
}

fn unavailable() -> BackendError {
    BackendError::new(
        "host_unavailable",
        true,
        JsonValue::Object(Default::default()),
    )
}

fn unavailable_after_enqueue(command: bool) -> BackendError {
    if command {
        BackendError::new(
            "outcome_unknown",
            false,
            JsonValue::Object(Default::default()),
        )
        .with_outcome_uncertain(true)
    } else {
        unavailable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_does_not_wait_for_a_full_retained_stream() {
        let (sender, mut receiver) = mpsc::channel(INITIAL_STREAM_CREDIT);
        for sequence in 0..INITIAL_STREAM_CREDIT {
            sender
                .try_send(Ok(serde_json::json!({"sequence":sequence})))
                .unwrap();
        }
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let streams = Arc::new(Mutex::new(BTreeMap::from([(
            "p:full".to_owned(),
            CapabilityStreamRoute { sender },
        )])));
        tokio::time::timeout(Duration::from_millis(100), fail_all(&pending, &streams))
            .await
            .expect("shutdown cannot depend on a Consumer draining the queue");
        for sequence in 0..INITIAL_STREAM_CREDIT {
            assert_eq!(
                receiver.recv().await.unwrap().unwrap()["sequence"],
                sequence
            );
        }
        assert_eq!(
            receiver.recv().await.unwrap().unwrap_err().code(),
            "host_unavailable"
        );
    }

    #[tokio::test]
    async fn session_loss_after_command_enqueue_is_uncertain_but_query_loss_is_retryable() {
        let (writer, _control, mut data) = PriorityWriter::channels();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let streams = Arc::new(Mutex::new(BTreeMap::new()));
        let client = CapabilityClient::new(
            Arc::new(writer),
            pending.clone(),
            streams.clone(),
            Arc::new(Mutex::new(
                teamarness_package_protocol::ContractState::default(),
            )),
        );
        let command = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .call(
                        "Z3JhbnQx",
                        "invoke",
                        serde_json::json!({
                            "contract":"fixture.messages", "port":"message.post",
                            "kind":"command", "request":{}
                        }),
                    )
                    .await
            }
        });
        data.recv().await.expect("the command entered the writer");
        let query = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .call(
                        "Z3JhbnQx",
                        "invoke",
                        serde_json::json!({
                            "contract":"fixture.messages", "port":"message.list",
                            "kind":"query", "request":{}
                        }),
                    )
                    .await
            }
        });
        data.recv().await.expect("the query entered the writer");
        fail_all(&pending, &streams).await;
        let command_error = command.await.unwrap().unwrap_err();
        assert!(command_error.outcome_uncertain());
        assert!(!command_error.retryable());
        let query_error = query.await.unwrap().unwrap_err();
        assert_eq!(query_error.code(), "host_unavailable");
        assert!(!query_error.outcome_uncertain());
        assert!(query_error.retryable());
    }

    #[tokio::test]
    async fn closed_pending_receiver_after_command_enqueue_is_uncertain() {
        let (writer, _control, mut data) = PriorityWriter::channels();
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let client = CapabilityClient::new(
            Arc::new(writer),
            pending.clone(),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(
                teamarness_package_protocol::ContractState::default(),
            )),
        );
        let command = tokio::spawn(async move {
            client
                .call(
                    "Z3JhbnQx",
                    "invoke",
                    serde_json::json!({
                        "contract":"fixture.messages", "port":"message.post",
                        "kind":"command", "request":{}
                    }),
                )
                .await
        });
        data.recv().await.expect("writer accepted the command");
        pending.lock().await.clear();
        let failure = command.await.unwrap().unwrap_err();
        assert_eq!(failure.code(), "outcome_unknown");
        assert!(failure.outcome_uncertain());
        assert!(!failure.retryable());
    }

    #[tokio::test]
    async fn refused_command_writer_remains_certain() {
        let (writer, _control, data) = PriorityWriter::channels();
        drop(data);
        let client = CapabilityClient::new(
            Arc::new(writer),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(
                teamarness_package_protocol::ContractState::default(),
            )),
        );
        let failure = client
            .call(
                "Z3JhbnQx",
                "invoke",
                serde_json::json!({
                    "contract":"fixture.messages", "port":"message.post",
                    "kind":"command", "request":{}
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code(), "host_unavailable");
        assert!(!failure.outcome_uncertain());
        assert!(failure.retryable());
    }

    // Verifies: TEAM.SYS.PKG-R14
    #[tokio::test]
    async fn inbound_event_lag_retires_only_its_stream_after_accepted_events() {
        // Inject one slot instead of the full published credit window to exercise
        // a locally lagged adapter without violating the wire classifier.
        const LAG_TEST_QUEUE: usize = INITIAL_STREAM_CREDIT / INITIAL_STREAM_CREDIT;
        let (slow, mut slow_rx) = mpsc::channel(LAG_TEST_QUEUE);
        let (peer, mut peer_rx) = mpsc::channel(LAG_TEST_QUEUE);
        let streams = Arc::new(Mutex::new(BTreeMap::from([
            ("p:slow".to_owned(), CapabilityStreamRoute { sender: slow }),
            ("p:peer".to_owned(), CapabilityStreamRoute { sender: peer }),
        ])));
        CapabilityClient::event(&streams, "p:slow", serde_json::json!({"order":1}))
            .await
            .unwrap();
        assert_eq!(
            CapabilityClient::event(&streams, "p:slow", serde_json::json!({"order":2})).await,
            Err("event_lagged")
        );
        CapabilityClient::event(&streams, "p:peer", serde_json::json!({"order":2}))
            .await
            .unwrap();
        assert_eq!(slow_rx.recv().await.unwrap().unwrap()["order"], 1);
        assert_eq!(
            slow_rx.recv().await.unwrap().unwrap_err().code(),
            "event_lagged"
        );
        assert_eq!(peer_rx.recv().await.unwrap().unwrap()["order"], 2);
        assert!(streams.lock().await.contains_key("p:peer"));
    }
}
