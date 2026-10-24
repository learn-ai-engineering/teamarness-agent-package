use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teamarness_package_protocol::{
    Direction, Frame, FrameKind, INITIAL_STREAM_CREDIT, JsonValue, MAX_IN_FLIGHT_EACH_DIRECTION,
    Verdict,
};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::cancellation::CancellationToken;
use crate::capability::{CapabilityClient, CapabilityStreams, PendingCapabilities};
use crate::port_observation::{self, ProviderPortObservation, ProviderPortStage};
use crate::priority_writer::{PriorityWriter, SharedState};
use crate::{Backend, BackendError, BackendRequest, Initialization, SdkError, object};

/// Streams one Host may hold open on this session at once.
///
/// The contract publishes no concurrent-stream limit, so this is the SDK's own
/// bound on the tasks one session holds. ADR 0019 §3 forbids an adapter-chosen
/// bound with a fatal verdict, and this one used to end the generation. It is
/// set at the in-flight request budget the contract does publish — the most
/// concurrent work either peer may ask of the other, and comfortably above the
/// eight UI subscriptions one mount may hold — and a stream past it is
/// answered with `stream_close` carrying `overloaded`, which ends that one
/// stream and is retryable.
const MAX_OPEN_HOST_STREAMS: usize = MAX_IN_FLIGHT_EACH_DIRECTION;

/// One in-flight request: its task, its cancellation signal, and whether it is a lifecycle call.
struct InFlight {
    task: JoinHandle<()>,
    cancellation: CancellationToken,
    lifecycle: bool,
    /// Set by whichever of the handler task and the cancel path gets there
    /// first. Exactly one `response` or `error` is terminal for a request, and
    /// a cancel that races a finishing handler must not emit the second.
    answered: Arc<AtomicBool>,
}

#[allow(clippy::too_many_arguments)] // The optional observer adds no work to the seven transport seams.
pub(crate) async fn run(
    backend: Arc<dyn Backend>,
    writer: Arc<PriorityWriter>,
    mut frames: mpsc::Receiver<Frame>,
    pending_capabilities: PendingCapabilities,
    capability_streams: CapabilityStreams,
    package_runtime: Handle,
    state: SharedState,
    observations: Option<tokio::sync::broadcast::Sender<ProviderPortObservation>>,
) -> Result<(), SdkError> {
    let initialize = frames.recv().await.ok_or(SdkError::ParentClosed)?;
    if initialize.kind() != FrameKind::Initialize {
        return Err(SdkError::Protocol("handshake_order"));
    }
    state
        .lock()
        .await
        .observe(Direction::HostToPackage, &initialize)
        .map_err(SdkError::protocol)?;
    let initialization = Initialization::from_frame(&initialize)?;
    writer.control(initialized(&initialize)?).await?;

    let capabilities = CapabilityClient::new(
        writer.clone(),
        pending_capabilities.clone(),
        capability_streams.clone(),
        state.clone(),
    );
    let lifecycle = Arc::new(Mutex::new(()));
    let (done_tx, mut done_rx) = mpsc::channel::<String>(MAX_IN_FLIGHT_EACH_DIRECTION);
    let mut tasks: BTreeMap<String, InFlight> = BTreeMap::new();
    let mut streams: BTreeMap<String, (JoinHandle<()>, Arc<Semaphore>, CancellationToken)> =
        BTreeMap::new();

    loop {
        tokio::select! {
            completed = done_rx.recv() => {
                if let Some(id) = completed { tasks.remove(&id); }
            }
            frame = frames.recv() => {
                let Some(frame) = frame else {
                    cancel_all(&tasks, &streams);
                    crate::parent_lifeline::abort_all(tasks.into_values().map(|entry| entry.task));
                    crate::parent_lifeline::abort_all(streams.into_values().map(|value| value.0));
                    return Err(SdkError::ParentClosed);
                };
                // One classifier, shared with the Host and with this side's own
                // writer. A frame it ignores names state this side has already
                // retired, and never reaches a handler.
                let verdict = state
                    .lock()
                    .await
                    .observe(Direction::HostToPackage, &frame)
                    .map_err(SdkError::protocol)?;
                if verdict == Verdict::Ignore {
                    continue;
                }
                match frame.kind() {
                    FrameKind::Request => {
                        let id = string_field(&frame, "id")?;
                        let operation = string_field(&frame, "operation")?;
                        let is_lifecycle = crate::LifecycleOperation::parse(&operation).is_some();
                        let cancellation = CancellationToken::new();
                        // PKG-R02: a suspend or dispose releases the work the previous state owns
                        // before the lifecycle handler runs.
                        if matches!(operation.as_str(), "lifecycle.suspend" | "lifecycle.dispose") {
                            for entry in tasks.values() {
                                if !entry.lifecycle {
                                    entry.cancellation.cancel();
                                }
                            }
                            for (_, _, token) in streams.values() {
                                token.cancel();
                            }
                        }
                        let deadline = deadline_of(&frame);
                        if let Some(deadline) = deadline {
                            match deadline.duration_since(SystemTime::now()) {
                                Err(_) => cancellation.cancel(),
                                Ok(remaining) => {
                                    // The timer belongs to the control runtime, so a blocked
                                    // Package worker cannot delay it.
                                    let token = cancellation.clone();
                                    tokio::spawn(async move {
                                        tokio::time::sleep(remaining).await;
                                        token.cancel();
                                    });
                                }
                            }
                        }
                        let observed = port_observation::operation(&operation);
                        emit(&observations, observed, &id, ProviderPortStage::Received, None);
                        let request = BackendRequest {
                            operation,
                            payload: frame.field("payload").cloned().ok_or(SdkError::Protocol("malformed_frame"))?,
                            initialization: initialization.clone(),
                            capabilities: capabilities.clone(),
                            cancellation: cancellation.clone(),
                            deadline,
                        };
                        let backend = backend.clone();
                        let writer = writer.clone();
                        let done = done_tx.clone();
                        let lifecycle = lifecycle.clone();
                        let task_id = id.clone();
                        let answered = Arc::new(AtomicBool::new(false));
                        let task_answered = answered.clone();
                        let observer = observations.clone();
                        let task = package_runtime.spawn(async move {
                            let result = if is_lifecycle {
                                let _barrier = lifecycle.lock().await;
                                backend.handle(request).await
                            } else {
                                backend.handle(request).await
                            };
                            // Claim the one terminal frame before writing it,
                            // so a cancel that reaches the dispatcher while
                            // this task is finishing cannot write a second.
                            // The claim is why the write must then wait for
                            // capacity: nothing else will ever answer this id,
                            // so a refused frame is a request that receives no
                            // terminal frame at all. Only a session that has
                            // already ended can fail this send.
                            if !task_answered.swap(true, Ordering::AcqRel) {
                                let stage = if result.is_ok() { ProviderPortStage::Completed } else { ProviderPortStage::Failed };
                                let code = result.as_ref().err().map(|error| error.code().to_owned());
                                let uncertain = result.as_ref().err().is_some_and(BackendError::outcome_uncertain);
                                emit(&observer, observed, &id, stage, code.map(|code| (code, uncertain)));
                                let _ = writer.control_waiting(terminal(&id, result)).await;
                            }
                            let _ = done.send(id).await;
                        });
                        tasks.insert(task_id, InFlight { task, cancellation, lifecycle: is_lifecycle, answered });
                    }
                    FrameKind::Cancel => {
                        let target = string_field(&frame, "targetId")?;
                        if let Some(entry) = tasks.remove(&target) {
                            // Cancel first, so blocking work sees the signal before the abort.
                            entry.cancellation.cancel();
                            entry.task.abort();
                            if !entry.answered.swap(true, Ordering::AcqRel) {
                                writer.control(error_frame(
                                    &target,
                                    &BackendError::new("cancelled", true, empty_object()),
                                )).await?;
                            }
                        }
                    }
                    FrameKind::Event => {
                        let id = string_field(&frame, "streamId")?;
                        let payload = frame
                            .field("payload")
                            .cloned()
                            .ok_or(SdkError::Protocol("malformed_frame"))?;
                        if crate::capability::CapabilityClient::event(
                            &capability_streams,
                            &id,
                            payload,
                        )
                        .await
                        .is_err()
                        {
                            state.lock().await.retire_stream(&id);
                            writer.control(stream_close(&id, "event_lagged")).await?;
                        }
                    }
                    FrameKind::Response | FrameKind::Error => {
                        let id = string_field(&frame, "id")?;
                        // The classifier has already decided this frame. A call
                        // whose caller stopped waiting has no correlation entry
                        // left, and the answer is simply dropped.
                        let Some(pending) = pending_capabilities.lock().await.remove(&id) else {
                            continue;
                        };
                        let result = if frame.kind() == FrameKind::Response {
                            Ok(frame.field("payload").cloned().unwrap_or_else(empty_object))
                        } else {
                            Err(BackendError::new(
                                frame.field("code").and_then(JsonValue::as_str).unwrap_or("capability_error"),
                                frame.field("retryable").and_then(JsonValue::as_bool).unwrap_or(false),
                                frame.field("details").cloned().unwrap_or_else(empty_object),
                            ).with_outcome_uncertain(frame.field("outcomeUncertain").and_then(JsonValue::as_bool).unwrap_or(false)))
                        };
                        let _ = pending.sender.send(result);
                    }
                    FrameKind::Ping => {
                        writer.control(object([
                            ("kind", JsonValue::String("pong".into())),
                            ("nonce", frame.field("nonce").cloned().ok_or(SdkError::Protocol("malformed_frame"))?),
                        ])).await?;
                    }
                    FrameKind::StreamOpen => {
                        streams.retain(|_, (task, _, _)| !task.is_finished());
                        let id = string_field(&frame, "id")?;
                        if streams.len() >= MAX_OPEN_HOST_STREAMS {
                            // One stream is refused, not the session. The
                            // contract publishes no concurrent-stream limit and
                            // its fatal list names a 33rd in-flight *request*,
                            // so ending the generation over an adapter's own
                            // bound was out of contract. `overloaded` is the
                            // host code for exactly this, and the SDK's own
                            // table calls it retryable.
                            writer.data(stream_close(&id, "overloaded")).await?;
                            continue;
                        }
                        let cancellation = CancellationToken::new();
                        let operation = string_field(&frame, "topic")?;
                        let observed = port_observation::operation(&operation);
                        let request = BackendRequest {
                            operation,
                            payload: frame.field("payload").cloned().ok_or(SdkError::Protocol("malformed_frame"))?,
                            initialization: initialization.clone(),
                            capabilities: capabilities.clone(),
                            cancellation: cancellation.clone(),
                            deadline: None,
                        };
                        let backend = backend.clone();
                        let writer = writer.clone();
                        let stream_id = id.clone();
                        let credit = Arc::new(Semaphore::new(INITIAL_STREAM_CREDIT));
                        let stream_credit = credit.clone();
                        let observer = observations.clone();
                        let task = package_runtime.spawn(async move {
                            // The Package's own code reaches the Host, and through it the UI.
                            let mut subscription = match backend.subscribe(request).await {
                                Ok(subscription) => subscription,
                                Err(error) => {
                                    emit(&observer, observed, &stream_id, ProviderPortStage::Failed,
                                        Some((error.code().to_owned(), error.outcome_uncertain())));
                                    let _ =
                                        writer.data(stream_close(&stream_id, error.code())).await;
                                    return;
                                }
                            };
                            emit(&observer, observed, &stream_id, ProviderPortStage::Opened, None);
                            let mut sequence = 0_u64;
                            while let Some(event) = subscription.receiver.recv().await {
                                let permit = match stream_credit.acquire().await {
                                    Ok(permit) => permit,
                                    Err(_) => return,
                                };
                                permit.forget();
                                match event {
                                    Ok(payload) => {
                                        let payload = if subscription.port_values {
                                            crate::ports::encode_port_value(payload)
                                        } else { payload };
                                        sequence += 1;
                                        if writer.data(object([
                                            ("kind", JsonValue::String("event".into())),
                                            ("streamId", JsonValue::String(stream_id.clone())),
                                            ("sequence", JsonValue::String(sequence.to_string())),
                                            ("payload", payload),
                                        ])).await.is_err() { return; }
                                        emit(&observer, observed, &stream_id, ProviderPortStage::Event, None);
                                    }
                                    Err(error) => {
                                        emit(&observer, observed, &stream_id, ProviderPortStage::Failed,
                                            Some((error.code().to_owned(), error.outcome_uncertain())));
                                        let _ = writer.data(stream_close(&stream_id, error.code())).await;
                                        return;
                                    }
                                }
                            }
                            emit(&observer, observed, &stream_id, ProviderPortStage::Closed, None);
                            let _ = writer.data(stream_close(&stream_id, "completed")).await;
                        });
                        streams.insert(id, (task, credit, cancellation));
                    }
                    FrameKind::Credit => {
                        let id = string_field(&frame, "streamId")?;
                        // The amount and the window are the classifier's; what
                        // is left is handing the permits to the stream's task.
                        let amount = frame.field("amount").and_then(JsonValue::as_u64)
                            .and_then(|value| usize::try_from(value).ok())
                            .ok_or(SdkError::Protocol("credit_violation"))?;
                        if let Some((_, credit, _)) = streams.get(&id) {
                            credit.add_permits(amount);
                        }
                    }
                    FrameKind::StreamClose => {
                        let id = string_field(&frame, "streamId")?;
                        if let Some((task, _, cancellation)) = streams.remove(&id) {
                            cancellation.cancel();
                            task.abort();
                        } else {
                            let code = frame.field("code").and_then(JsonValue::as_str);
                            let error = code
                                .filter(|code| !matches!(*code, "completed" | "closed" | "dropped"))
                                .map(|code| {
                                    BackendError::new(
                                        code,
                                        host_code_is_retryable(code),
                                        empty_object(),
                                    )
                                });
                            crate::capability::CapabilityClient::close_stream(&capability_streams, &id, error).await;
                        }
                    }
                    FrameKind::Shutdown => {
                        cancel_all(&tasks, &streams);
                        crate::parent_lifeline::abort_all(tasks.into_values().map(|entry| entry.task));
                        crate::parent_lifeline::abort_all(streams.into_values().map(|value| value.0));
                        writer.control(object([("kind", JsonValue::String("shutdown_ack".into()))])).await?;
                        return Ok(());
                    }
                    _ => return Err(SdkError::Protocol("protocol_violation")),
                }
            }
        }
    }
}

fn emit(
    sender: &Option<tokio::sync::broadcast::Sender<ProviderPortObservation>>,
    operation: Option<&'static str>,
    provider_id: &str,
    stage: ProviderPortStage,
    error: Option<(String, bool)>,
) {
    if let (Some(sender), Some(operation)) = (sender, operation) {
        let (code, outcome_uncertain) =
            error.map_or((None, false), |(code, uncertain)| (Some(code), uncertain));
        let _ = sender.send(ProviderPortObservation {
            provider_id: provider_id.to_owned(),
            operation,
            stage,
            code,
            outcome_uncertain,
        });
    }
}

/// Whether a host code means "try again", on a stream close as on a call (register C-15).
///
/// A stream close carries no `retryable` field, so the SDK applies the same table the host
/// applies to a capability call: the same code can never be retryable on one path and permanent
/// on the other. `cursor_invalid` is not retryable here; the record log session answers it by
/// code before it consults retryability.
fn host_code_is_retryable(code: &str) -> bool {
    matches!(
        code,
        "overloaded" | "deadline_exceeded" | "host_unavailable"
    )
}

/// Cancels every in-flight request and stream, so blocking work stops before the aborts land.
fn cancel_all(
    tasks: &BTreeMap<String, InFlight>,
    streams: &BTreeMap<String, (JoinHandle<()>, Arc<Semaphore>, CancellationToken)>,
) {
    for entry in tasks.values() {
        entry.cancellation.cancel();
    }
    for (_, _, cancellation) in streams.values() {
        cancellation.cancel();
    }
}

/// Reads `deadlineUnixMs`, a decimal string of milliseconds since the Unix epoch.
fn deadline_of(frame: &Frame) -> Option<SystemTime> {
    let millis = frame
        .field("deadlineUnixMs")
        .and_then(JsonValue::as_str)
        .and_then(|value| value.parse::<u64>().ok())?;
    UNIX_EPOCH.checked_add(Duration::from_millis(millis))
}

fn initialized(frame: &Frame) -> Result<JsonValue, SdkError> {
    Ok(object([
        ("kind", JsonValue::String("initialized".into())),
        ("nonce", required(frame, "nonce")?),
        ("package", required(frame, "package")?),
        ("uiContractDigest", required(frame, "uiContractDigest")?),
        ("processGeneration", required(frame, "processGeneration")?),
        ("protocol", required(frame, "protocol")?),
    ]))
}

fn required(frame: &Frame, name: &str) -> Result<JsonValue, SdkError> {
    frame
        .field(name)
        .cloned()
        .ok_or(SdkError::Protocol("malformed_frame"))
}

fn string_field(frame: &Frame, name: &str) -> Result<String, SdkError> {
    frame
        .field(name)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .ok_or(SdkError::Protocol("malformed_frame"))
}

fn terminal(id: &str, result: Result<JsonValue, BackendError>) -> JsonValue {
    match result {
        Ok(payload) if payload.is_object() => object([
            ("kind", JsonValue::String("response".into())),
            ("id", JsonValue::String(id.into())),
            ("payload", payload),
        ]),
        Ok(_) => error_frame(
            id,
            &BackendError::new("invalid_response", false, empty_object()),
        ),
        Err(error) => error_frame(id, &error),
    }
}

fn error_frame(id: &str, error: &BackendError) -> JsonValue {
    let mut frame = object([
        ("kind", JsonValue::String("error".into())),
        ("id", JsonValue::String(id.into())),
        ("code", JsonValue::String(error.code.clone())),
        ("retryable", JsonValue::Bool(error.retryable)),
        ("details", error.details.clone()),
    ]);
    if error.outcome_uncertain() {
        frame["outcomeUncertain"] = JsonValue::Bool(true);
    }
    frame
}

fn stream_close(stream_id: &str, code: &str) -> JsonValue {
    object([
        ("kind", JsonValue::String("stream_close".into())),
        ("streamId", JsonValue::String(stream_id.into())),
        ("code", JsonValue::String(code.into())),
    ])
}

fn empty_object() -> JsonValue {
    JsonValue::Object(Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::priority_writer;
    use teamarness_package_protocol::{Frame, MAX_WRITER_FRAMES, decode_frame};
    use tokio::io::{AsyncReadExt, DuplexStream, duplex};
    use tokio::sync::oneshot;

    async fn read_frame(stream: &mut DuplexStream) -> Frame {
        let mut prefix = [0; 4];
        stream.read_exact(&mut prefix).await.unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(prefix) as usize + 4];
        bytes[..4].copy_from_slice(&prefix);
        stream.read_exact(&mut bytes[4..]).await.unwrap();
        decode_frame(&bytes).unwrap()
    }

    fn frame(value: JsonValue) -> Frame {
        Frame::try_from(value).unwrap()
    }

    #[test]
    fn uncertain_author_error_is_a_nonretryable_wire_error() {
        let error = BackendError::new("fixture.committed", true, empty_object())
            .with_outcome_uncertain(true);
        let value = terminal("p:command", Err(error));
        assert_eq!(value["outcomeUncertain"], true);
        assert_eq!(value["retryable"], false);
        assert_eq!(value["code"], "fixture.committed");
        Frame::try_from(value).expect("the uncertain error is a valid Process frame");
    }

    // Verifies: TEAM.SYS.PKG-R14
    #[tokio::test]
    async fn inbound_event_lag_closes_one_stream_and_dispatcher_answers_ping() {
        let (writer, control, data) = PriorityWriter::channels();
        let writer = Arc::new(writer);
        let state = Arc::new(Mutex::new(
            teamarness_package_protocol::ContractState::default(),
        ));
        let (sdk_out, mut host_in) = duplex(4096);
        let (stop, stopped) = oneshot::channel();
        let writer_task = tokio::spawn(priority_writer::run(
            sdk_out,
            control,
            data,
            stopped,
            state.clone(),
        ));
        let (host_frames, frames) = mpsc::channel(MAX_WRITER_FRAMES);
        let pending = Arc::new(Mutex::new(BTreeMap::new()));
        let streams: CapabilityStreams = Arc::new(Mutex::new(BTreeMap::new()));
        let task = tokio::spawn(run(
            crate::Router::stateless().into_backend(),
            writer.clone(),
            frames,
            pending,
            streams.clone(),
            Handle::current(),
            state,
            None,
        ));
        host_frames.send(frame(serde_json::json!({
            "kind":"initialize","nonce":"bm9uY2U",
            "package":{"identity":"dev.teamarness.fixture","version":"1.0.0",
                "manifestDigest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "backendDigest":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
            "uiContractDigest":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "processGeneration":"1","workspace":{"id":"workspace1","name":"Workspace"},
            "installation":{"identity":"installation1","displayName":"Installation"},
            "dataDirectory":{"path":"/tmp/package","continuityId":"Y29udGludWl0eQ"},
            "protocol":{"major":1,"minor":0},"capabilities":[],
            "limits":{"maxFrameBytes":1048576,"maxInFlightEachDirection":32,
                "maxWriterFrames":64,"maxWriterBytes":8388608,"initialStreamCredit":32}
        }))).await.unwrap();
        assert_eq!(
            read_frame(&mut host_in).await.kind(),
            FrameKind::Initialized
        );
        for id in ["p:slow", "p:peer"] {
            writer
                .control(serde_json::json!({"kind":"stream_open","id":id,
                "topic":"capability.stream","initialCredit":32,"payload":{}}))
                .await
                .unwrap();
            assert_eq!(read_frame(&mut host_in).await.kind(), FrameKind::StreamOpen);
        }
        // The test injects a smaller adapter queue while retaining the wire's
        // legal 32-event credit. This is the only way a conforming classifier
        // can expose adapter lag without fabricating a fatal credit violation.
        const LAG_TEST_QUEUE: usize = INITIAL_STREAM_CREDIT / INITIAL_STREAM_CREDIT;
        let (slow, mut slow_rx) = mpsc::channel(LAG_TEST_QUEUE);
        let (peer, mut peer_rx) = mpsc::channel(LAG_TEST_QUEUE);
        streams.lock().await.extend([
            (
                "p:slow".into(),
                crate::capability::CapabilityStreamRoute { sender: slow },
            ),
            (
                "p:peer".into(),
                crate::capability::CapabilityStreamRoute { sender: peer },
            ),
        ]);
        for (id, sequence) in [("p:slow", 1), ("p:slow", 2), ("p:peer", 1)] {
            host_frames
                .send(frame(serde_json::json!({"kind":"event","streamId":id,
                "sequence":sequence.to_string(),"payload":{"order":sequence}})))
                .await
                .unwrap();
        }
        let close = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut host_in))
            .await
            .unwrap();
        assert_eq!(close.kind(), FrameKind::StreamClose);
        assert_eq!(close.field("streamId").unwrap(), "p:slow");
        assert_eq!(close.field("code").unwrap(), "event_lagged");
        assert_eq!(slow_rx.recv().await.unwrap().unwrap()["order"], 1);
        assert_eq!(
            slow_rx.recv().await.unwrap().unwrap_err().code(),
            "event_lagged"
        );
        assert_eq!(peer_rx.recv().await.unwrap().unwrap()["order"], 1);
        host_frames
            .send(frame(serde_json::json!({"kind":"ping","nonce":"cGVlcg"})))
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut host_in))
            .await
            .unwrap();
        assert_eq!(pong.kind(), FrameKind::Pong);
        drop(host_frames);
        assert!(matches!(task.await.unwrap(), Err(SdkError::ParentClosed)));
        let _ = stop.send(());
        writer_task.await.unwrap().unwrap();
    }
}
