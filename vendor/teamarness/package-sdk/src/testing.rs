//! A protocol-free test harness for Package Backends.
//!
//! A Package test drives its own [`Backend`] — usually a [`Router`](crate::Router) turned into one
//! — the way the Host does, without writing a single frame. The harness owns the handshake, the
//! request and stream correlation, the credit window, and the scripted answers a Semantic
//! Capability would give.
//!
//! It is behind the `testing` Cargo feature, so a shipped Package Backend never carries it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use teamarness_package_protocol::{
    Frame, FrameKind, INITIAL_STREAM_CREDIT, JsonValue, MAX_FRAME_BYTES,
    MAX_IN_FLIGHT_EACH_DIRECTION, MAX_WRITER_BYTES, MAX_WRITER_FRAMES, decode_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::router::RouterOperations;
use crate::{Backend, BackendError, Package, SdkError, object};

/// How long a harness request waits before the Host would give up, unless a test names its own.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The credit window the Host opens on a Package subscription.
const INITIAL_CREDIT: u64 = INITIAL_STREAM_CREDIT as u64;

/// Builds a [`Harness`] around a Package's own [`Backend`].
pub struct HarnessBuilder {
    backend: Arc<dyn Backend>,
    package_identity: String,
    package_version: String,
    workspace: (String, String),
    installation: (String, String),
    data_directory: Option<PathBuf>,
    capabilities: BTreeMap<String, ScriptedCapability>,
    minors: BTreeMap<String, u16>,
}

impl HarnessBuilder {
    /// Starts a harness for one Backend. `Router::into_backend` produces the argument.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            package_identity: "dev.teamarness.harness".to_owned(),
            package_version: "1.0.0".to_owned(),
            workspace: ("workspace1".to_owned(), "Harness Workspace".to_owned()),
            installation: ("installation1".to_owned(), "Harness".to_owned()),
            data_directory: None,
            capabilities: BTreeMap::new(),
            minors: BTreeMap::new(),
        }
    }

    /// The Package identity the Host would report.
    #[must_use]
    pub fn package_identity(mut self, identity: &str) -> Self {
        self.package_identity = identity.to_owned();
        self
    }

    /// The Package version the Host would report.
    #[must_use]
    pub fn package_version(mut self, version: &str) -> Self {
        self.package_version = version.to_owned();
        self
    }

    /// The Workspace this Instance is bound to.
    #[must_use]
    pub fn workspace(mut self, id: &str, name: &str) -> Self {
        self.workspace = (id.to_owned(), name.to_owned());
        self
    }

    /// The Installation running the Package.
    #[must_use]
    pub fn installation(mut self, identity: &str, display_name: &str) -> Self {
        self.installation = (identity.to_owned(), display_name.to_owned());
        self
    }

    /// The Package's own private directory. A Package test passes a temporary directory.
    #[must_use]
    pub fn data_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.data_directory = Some(path.into());
        self
    }

    /// Grants one Semantic Capability at minor 0, answered by a scripted double.
    #[must_use]
    pub fn capability(self, identity: &str, scripted: ScriptedCapability) -> Self {
        self.capability_minor(identity, scripted, 0)
    }

    /// Grants one Semantic Capability at the negotiated `minor` the Host would report.
    #[must_use]
    pub fn capability_minor(
        mut self,
        identity: &str,
        scripted: ScriptedCapability,
        minor: u16,
    ) -> Self {
        self.capabilities.insert(identity.to_owned(), scripted);
        self.minors.insert(identity.to_owned(), minor);
        self
    }

    /// Serves the Backend and completes the handshake.
    ///
    /// # Panics
    ///
    /// Panics when no data directory was named, or when the Backend refuses the handshake.
    pub async fn start(self) -> Harness {
        let data_directory = self
            .data_directory
            .expect("HarnessBuilder::data_directory is required; pass a temporary directory");
        let (mut host_read, sdk_write) = tokio::io::duplex(1 << 20);
        let (sdk_read, host_write) = tokio::io::duplex(1 << 20);
        let service =
            tokio::spawn(Package::from_backend(self.backend).serve_io(sdk_read, sdk_write));
        let writer = Arc::new(Writer {
            stream: tokio::sync::Mutex::new(host_write),
        });

        let grants: BTreeMap<String, ScriptedCapability> = self
            .capabilities
            .iter()
            .map(|(identity, scripted)| (grant_for(identity), scripted.clone()))
            .collect();
        for scripted in self.capabilities.values() {
            let _ = scripted.shared.stream.writer.set(writer.clone());
        }
        let capabilities = self
            .capabilities
            .keys()
            .map(|identity| {
                object([
                    ("identity", JsonValue::String(identity.clone())),
                    ("grant", JsonValue::String(grant_for(identity))),
                    ("major", JsonValue::from(1_u64)),
                    (
                        "minor",
                        JsonValue::from(u64::from(self.minors.get(identity).copied().unwrap_or(0))),
                    ),
                ])
            })
            .collect::<Vec<_>>();

        let initialize = object([
            ("kind", JsonValue::String("initialize".into())),
            ("nonce", JsonValue::String(base64url("harness-nonce"))),
            (
                "package",
                object([
                    ("identity", JsonValue::String(self.package_identity.clone())),
                    ("version", JsonValue::String(self.package_version.clone())),
                    ("manifestDigest", JsonValue::String("a".repeat(64))),
                    ("backendDigest", JsonValue::String("b".repeat(64))),
                ]),
            ),
            ("uiContractDigest", JsonValue::String("c".repeat(64))),
            ("processGeneration", JsonValue::String("1".into())),
            (
                "workspace",
                object([
                    ("id", JsonValue::String(self.workspace.0)),
                    ("name", JsonValue::String(self.workspace.1)),
                ]),
            ),
            (
                "installation",
                object([
                    ("identity", JsonValue::String(self.installation.0)),
                    ("displayName", JsonValue::String(self.installation.1)),
                ]),
            ),
            (
                "dataDirectory",
                object([
                    (
                        "path",
                        JsonValue::String(data_directory.to_string_lossy().into_owned()),
                    ),
                    (
                        "continuityId",
                        JsonValue::String(base64url("harness-continuity")),
                    ),
                ]),
            ),
            (
                "protocol",
                object([
                    ("major", JsonValue::from(1_u64)),
                    ("minor", JsonValue::from(0_u64)),
                ]),
            ),
            ("capabilities", JsonValue::Array(capabilities)),
            (
                "limits",
                object([
                    ("maxFrameBytes", JsonValue::from(MAX_FRAME_BYTES as u64)),
                    (
                        "maxInFlightEachDirection",
                        JsonValue::from(MAX_IN_FLIGHT_EACH_DIRECTION as u64),
                    ),
                    ("maxWriterFrames", JsonValue::from(MAX_WRITER_FRAMES as u64)),
                    ("maxWriterBytes", JsonValue::from(MAX_WRITER_BYTES as u64)),
                    ("initialStreamCredit", JsonValue::from(INITIAL_CREDIT)),
                ]),
            ),
        ]);
        writer.send(&initialize).await.expect("harness initialize");

        let acknowledged = read_frame(&mut host_read).await.expect("initialized frame");
        assert_eq!(
            acknowledged.kind(),
            FrameKind::Initialized,
            "the Backend answered the handshake with {:?}",
            acknowledged.kind()
        );

        let routes = Arc::new(Mutex::new(Routes::default()));
        let pump = tokio::spawn(pump(
            host_read,
            writer.clone(),
            routes.clone(),
            Arc::new(grants),
        ));
        Harness {
            writer,
            routes,
            next_id: AtomicU64::new(1),
            pump,
            service: Some(service),
        }
    }
}

/// A Backend served the way the Host serves it, driven by ordinary method calls.
pub struct Harness {
    writer: Arc<Writer>,
    routes: Arc<Mutex<Routes>>,
    next_id: AtomicU64,
    pump: JoinHandle<()>,
    service: Option<JoinHandle<Result<(), SdkError>>>,
}

impl Harness {
    /// Activates the Package.
    ///
    /// # Errors
    ///
    /// Returns the Backend's own activation failure.
    pub async fn activate(&mut self) -> Result<JsonValue, BackendError> {
        self.request("lifecycle.activate", empty()).await
    }

    /// Recovers the Package, as a replacement generation or after a suspend.
    ///
    /// # Errors
    ///
    /// Returns the Backend's own failure.
    pub async fn recover(&mut self) -> Result<JsonValue, BackendError> {
        self.request("lifecycle.recover", empty()).await
    }

    /// Suspends the Package, keeping its state.
    ///
    /// # Errors
    ///
    /// Returns the Backend's own failure.
    pub async fn suspend(&mut self) -> Result<JsonValue, BackendError> {
        self.request("lifecycle.suspend", empty()).await
    }

    /// Disposes the Package's state.
    ///
    /// # Errors
    ///
    /// Returns the Backend's own failure.
    pub async fn dispose(&mut self) -> Result<JsonValue, BackendError> {
        self.request("lifecycle.dispose", empty()).await
    }

    /// Sends one request and waits for its terminal frame.
    ///
    /// # Errors
    ///
    /// Returns the Package-owned error the Backend answered with.
    pub async fn request(
        &mut self,
        operation: &str,
        payload: JsonValue,
    ) -> Result<JsonValue, BackendError> {
        self.start_request(operation, payload).await.result().await
    }

    /// Sends one request without waiting, so a test can cancel it or race it.
    pub async fn start_request(&mut self, operation: &str, payload: JsonValue) -> PendingRequest {
        let deadline = SystemTime::now() + DEFAULT_REQUEST_TIMEOUT;
        self.start_request_with_deadline(operation, payload, deadline)
            .await
    }

    /// Sends one request carrying the deadline the Host would have named.
    pub async fn start_request_with_deadline(
        &mut self,
        operation: &str,
        payload: JsonValue,
        deadline: SystemTime,
    ) -> PendingRequest {
        let id = self.next("request");
        let (sender, receiver) = oneshot::channel();
        self.lock().requests.insert(id.clone(), sender);
        let frame = object([
            ("kind", JsonValue::String("request".into())),
            ("id", JsonValue::String(id.clone())),
            ("operation", JsonValue::String(operation.to_owned())),
            (
                "deadlineUnixMs",
                JsonValue::String(unix_millis(deadline).to_string()),
            ),
            ("payload", payload),
        ]);
        self.writer.send(&frame).await.expect("harness request");
        PendingRequest {
            id,
            cancel_id: self.next("cancel"),
            writer: self.writer.clone(),
            receiver,
        }
    }

    /// Opens one Package subscription with the Host's credit window.
    ///
    /// A Backend that refuses the subscription closes the stream, which the returned subscription
    /// reports as its terminal error.
    ///
    /// # Errors
    ///
    /// Returns an error only when the session has already ended.
    pub async fn subscribe(
        &mut self,
        operation: &str,
        payload: JsonValue,
    ) -> Result<HarnessSubscription, BackendError> {
        let id = self.next("stream");
        let (sender, events) = mpsc::unbounded_channel();
        self.lock().subscriptions.insert(id.clone(), sender);
        let frame = object([
            ("kind", JsonValue::String("stream_open".into())),
            ("id", JsonValue::String(id.clone())),
            ("topic", JsonValue::String(operation.to_owned())),
            ("initialCredit", JsonValue::from(INITIAL_CREDIT)),
            ("payload", payload),
        ]);
        if self.writer.send(&frame).await.is_err() {
            self.lock().subscriptions.remove(&id);
            return Err(BackendError::new("host_unavailable", true, empty()));
        }
        Ok(HarnessSubscription {
            id,
            writer: self.writer.clone(),
            routes: self.routes.clone(),
            events,
            closed: false,
        })
    }

    /// Sends a control `ping` and returns how long the answer took.
    ///
    /// # Panics
    ///
    /// Panics when the session ends before answering.
    pub async fn ping(&mut self) -> Duration {
        let nonce = base64url(&format!(
            "harness-ping-{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        ));
        let (sender, receiver) = oneshot::channel();
        self.lock().pings.insert(nonce.clone(), sender);
        let frame = object([
            ("kind", JsonValue::String("ping".into())),
            ("nonce", JsonValue::String(nonce)),
        ]);
        let started = Instant::now();
        self.writer.send(&frame).await.expect("harness ping");
        receiver.await.expect("the session answered the ping");
        started.elapsed()
    }

    /// Asks the Backend to shut down and waits for the session to end.
    ///
    /// # Errors
    ///
    /// Returns the transport failure that ended the session, when it was not a clean shutdown.
    pub async fn shutdown(mut self) -> Result<(), SdkError> {
        let (sender, receiver) = oneshot::channel();
        self.lock().shutdown = Some(sender);
        let frame = object([
            ("kind", JsonValue::String("shutdown".into())),
            ("reason", JsonValue::String("application_shutdown".into())),
        ]);
        let _ = self.writer.send(&frame).await;
        let _ = receiver.await;
        let service = self.service.take().expect("service task");
        let outcome = service.await.expect("the service task did not panic");
        self.pump.abort();
        outcome
    }

    fn next(&self, kind: &str) -> String {
        format!(
            "h:harness-{kind}-{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.pump.abort();
        if let Some(service) = self.service.take() {
            service.abort();
        }
    }
}

/// A request the harness has sent but not yet waited for.
pub struct PendingRequest {
    id: String,
    cancel_id: String,
    writer: Arc<Writer>,
    receiver: oneshot::Receiver<Result<JsonValue, BackendError>>,
}

impl PendingRequest {
    /// The correlation identity the Host gave this request.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Cancels the request, as the Host does when the UI goes away.
    pub async fn cancel(&self) {
        let frame = object([
            ("kind", JsonValue::String("cancel".into())),
            ("id", JsonValue::String(self.cancel_id.clone())),
            ("targetId", JsonValue::String(self.id.clone())),
        ]);
        let _ = self.writer.send(&frame).await;
    }

    /// Waits for the request's terminal frame.
    ///
    /// # Errors
    ///
    /// Returns the Package-owned error the Backend answered with.
    pub async fn result(self) -> Result<JsonValue, BackendError> {
        self.receiver
            .await
            .unwrap_or_else(|_| Err(BackendError::new("host_unavailable", true, empty())))
    }
}

/// One open Package subscription.
pub struct HarnessSubscription {
    id: String,
    writer: Arc<Writer>,
    routes: Arc<Mutex<Routes>>,
    events: mpsc::UnboundedReceiver<Option<Result<JsonValue, BackendError>>>,
    closed: bool,
}

impl HarnessSubscription {
    /// The next event, `None` once the stream ends gracefully.
    ///
    /// A stream the Package closed with its own code yields `Some(Err(..))` once, then `None`.
    pub async fn next_event(&mut self) -> Option<Result<JsonValue, BackendError>> {
        self.events.recv().await.unwrap_or_default()
    }

    /// Closes the stream from the Host's side.
    pub async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscriptions
            .remove(&self.id);
        let frame = object([
            ("kind", JsonValue::String("stream_close".into())),
            ("streamId", JsonValue::String(self.id.clone())),
            ("code", JsonValue::String("closed".into())),
        ]);
        let _ = self.writer.send(&frame).await;
    }
}

/// One call a Package made through a scripted capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub method: String,
    pub payload: JsonValue,
}

type CallHandler = Arc<dyn Fn(&str, JsonValue) -> Result<JsonValue, BackendError> + Send + Sync>;

/// A stand-in for one Semantic Capability.
///
/// It is cheap to clone, so a test keeps a handle while the builder takes one.
#[derive(Clone, Default)]
pub struct ScriptedCapability {
    shared: Arc<CapabilityShared>,
}

#[derive(Default)]
struct CapabilityShared {
    handler: Mutex<Option<CallHandler>>,
    calls: Mutex<Vec<RecordedCall>>,
    opens: Mutex<Vec<JsonValue>>,
    stream: Arc<StreamState>,
}

impl ScriptedCapability {
    /// A capability that refuses every call until [`ScriptedCapability::calls`] scripts one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers `capabilities.call` for this capability.
    #[must_use]
    pub fn calls<F>(self, handler: F) -> Self
    where
        F: Fn(&str, JsonValue) -> Result<JsonValue, BackendError> + Send + Sync + 'static,
    {
        *self
            .shared
            .handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(handler));
        self
    }

    /// The sender for whichever stream the Package has open on this capability.
    #[must_use]
    pub fn stream(&self) -> ScriptedStreamSender {
        ScriptedStreamSender {
            state: self.shared.stream.clone(),
        }
    }

    /// Every call the Package made, in order.
    #[must_use]
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.shared
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The payload of every stream the Package opened, in order.
    #[must_use]
    pub fn recorded_opens(&self) -> Vec<JsonValue> {
        self.shared
            .opens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn answer(&self, method: &str, payload: JsonValue) -> Result<JsonValue, BackendError> {
        self.shared
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedCall {
                method: method.to_owned(),
                payload: payload.clone(),
            });
        let handler = self
            .shared
            .handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match handler {
            Some(handler) => handler(method, payload),
            None => Err(BackendError::new("capability_unavailable", false, empty())),
        }
    }
}

#[derive(Default)]
struct StreamState {
    inner: Mutex<StreamInner>,
    notify: Notify,
    writer: OnceLock<Arc<Writer>>,
}

#[derive(Default)]
struct StreamInner {
    id: Option<String>,
    credit: u64,
    sequence: u64,
    closed: bool,
}

/// Pushes events into whichever stream a Package has open on a scripted capability.
#[derive(Clone)]
pub struct ScriptedStreamSender {
    state: Arc<StreamState>,
}

impl ScriptedStreamSender {
    /// Whether the Package currently has a stream open on this capability.
    #[must_use]
    pub fn is_open(&self) -> bool {
        let inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.id.is_some() && !inner.closed
    }

    /// Waits until the Package has a stream open on this capability.
    pub async fn opened(&self) {
        loop {
            let notified = self.state.notify.notified();
            if self.is_open() {
                return;
            }
            notified.await;
        }
    }

    /// Sends one event, waiting for the Package's credit first.
    ///
    /// Returns `false` when the stream closed before the event could be sent.
    pub async fn send(&self, payload: JsonValue) -> bool {
        loop {
            let notified = self.state.notify.notified();
            let frame = {
                let mut inner = self
                    .state
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if inner.closed {
                    return false;
                }
                match inner.id.clone() {
                    Some(id) if inner.credit > 0 => {
                        inner.credit -= 1;
                        inner.sequence += 1;
                        let sequence = inner.sequence;
                        Some(object([
                            ("kind", JsonValue::String("event".into())),
                            ("streamId", JsonValue::String(id)),
                            ("sequence", JsonValue::String(sequence.to_string())),
                            ("payload", payload.clone()),
                        ]))
                    }
                    _ => None,
                }
            };
            match frame {
                Some(frame) => {
                    let writer = self.state.writer.get().expect("a started harness");
                    return writer.send(&frame).await.is_ok();
                }
                None => notified.await,
            }
        }
    }

    /// Closes the open stream with a Host code, such as `completed` or `cursor_invalid`.
    pub async fn close(&self, code: &str) {
        let id = {
            let mut inner = self
                .state
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.closed {
                return;
            }
            inner.closed = true;
            inner.id.clone()
        };
        self.state.notify.notify_waiters();
        let Some(id) = id else { return };
        let frame = object([
            ("kind", JsonValue::String("stream_close".into())),
            ("streamId", JsonValue::String(id)),
            ("code", JsonValue::String(code.to_owned())),
        ]);
        if let Some(writer) = self.state.writer.get() {
            let _ = writer.send(&frame).await;
        }
    }
}

/// Fails the test when a router's handlers and a UI contract's operations differ.
///
/// # Panics
///
/// Panics naming both differences: operations the contract declares without a handler, and
/// handlers the contract does not declare.
pub fn assert_router_matches_contract(operations: &RouterOperations, contract_json: &str) {
    let contract: JsonValue =
        serde_json::from_str(contract_json).expect("the UI contract is valid JSON");
    let declared = |section: &str| -> Vec<String> {
        contract
            .get(section)
            .and_then(JsonValue::as_object)
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default()
    };
    let mut declared_without_handler = Vec::new();
    let mut handler_without_declaration = Vec::new();
    for (section, handlers) in [
        ("requests", &operations.requests),
        ("subscriptions", &operations.subscriptions),
    ] {
        let names = declared(section);
        for name in &names {
            if !handlers.contains(name) {
                declared_without_handler.push(name.clone());
            }
        }
        for name in handlers {
            if !names.contains(name) {
                handler_without_declaration.push(name.clone());
            }
        }
    }
    declared_without_handler.sort();
    handler_without_declaration.sort();
    assert!(
        declared_without_handler.is_empty() && handler_without_declaration.is_empty(),
        "contract/router mismatch: declared without handler: {declared_without_handler:?}; handler without declaration: {handler_without_declaration:?}"
    );
}

#[derive(Default)]
struct Routes {
    requests: BTreeMap<String, oneshot::Sender<Result<JsonValue, BackendError>>>,
    subscriptions: BTreeMap<String, mpsc::UnboundedSender<Option<Result<JsonValue, BackendError>>>>,
    pings: BTreeMap<String, oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
}

struct Writer {
    stream: tokio::sync::Mutex<DuplexStream>,
}

impl Writer {
    async fn send(&self, value: &JsonValue) -> Result<(), SdkError> {
        let payload =
            serde_json::to_vec(value).map_err(|_| SdkError::Protocol("malformed_frame"))?;
        let length =
            u32::try_from(payload.len()).map_err(|_| SdkError::Protocol("frame_too_large"))?;
        let mut stream = self.stream.lock().await;
        stream
            .write_all(&length.to_be_bytes())
            .await
            .map_err(|_| SdkError::ParentClosed)?;
        stream
            .write_all(&payload)
            .await
            .map_err(|_| SdkError::ParentClosed)
    }
}

/// Reads and routes every frame the Backend writes.
async fn pump(
    mut reader: DuplexStream,
    writer: Arc<Writer>,
    routes: Arc<Mutex<Routes>>,
    grants: Arc<BTreeMap<String, ScriptedCapability>>,
) {
    // A Package stream id is mapped to the capability it belongs to when the stream opens.
    let mut package_streams: BTreeMap<String, ScriptedCapability> = BTreeMap::new();
    while let Some(frame) = read_frame(&mut reader).await {
        match frame.kind() {
            FrameKind::Response | FrameKind::Error => {
                let Some(id) = string(&frame, "id") else {
                    continue;
                };
                let waiting = lock(&routes).requests.remove(&id);
                if let Some(sender) = waiting {
                    let _ = sender.send(terminal_of(&frame));
                }
            }
            FrameKind::Pong => {
                let waiting =
                    string(&frame, "nonce").and_then(|nonce| lock(&routes).pings.remove(&nonce));
                if let Some(sender) = waiting {
                    let _ = sender.send(());
                }
            }
            FrameKind::ShutdownAck => {
                let waiting = lock(&routes).shutdown.take();
                if let Some(sender) = waiting {
                    let _ = sender.send(());
                }
            }
            FrameKind::Event => {
                let Some(id) = string(&frame, "streamId") else {
                    continue;
                };
                let payload = frame.field("payload").cloned().unwrap_or_else(empty);
                let delivered = lock(&routes)
                    .subscriptions
                    .get(&id)
                    .is_some_and(|sender| sender.send(Some(Ok(payload))).is_ok());
                if delivered {
                    let credit = object([
                        ("kind", JsonValue::String("credit".into())),
                        ("streamId", JsonValue::String(id)),
                        ("amount", JsonValue::from(1_u64)),
                    ]);
                    let _ = writer.send(&credit).await;
                }
            }
            FrameKind::StreamClose => {
                let Some(id) = string(&frame, "streamId") else {
                    continue;
                };
                if let Some(scripted) = package_streams.remove(&id) {
                    let mut inner = scripted
                        .shared
                        .stream
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if inner.id.as_deref() == Some(id.as_str()) {
                        inner.closed = true;
                    }
                    drop(inner);
                    scripted.shared.stream.notify.notify_waiters();
                    continue;
                }
                let waiting = lock(&routes).subscriptions.remove(&id);
                if let Some(sender) = waiting {
                    // The Host's own table: a graceful close carries no error.
                    let code = frame.field("code").and_then(JsonValue::as_str);
                    match code {
                        None | Some("completed" | "closed" | "dropped") => {
                            let _ = sender.send(None);
                        }
                        Some(code) => {
                            let _ = sender.send(Some(Err(BackendError::new(code, false, empty()))));
                        }
                    }
                }
            }
            FrameKind::Request => {
                let (Some(id), Some(grant)) = (string(&frame, "id"), string(&frame, "grant"))
                else {
                    continue;
                };
                let mut payload = frame
                    .field("payload")
                    .and_then(JsonValue::as_object)
                    .cloned()
                    .unwrap_or_default();
                let method = payload
                    .remove("method")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let answer = match grants.get(&grant) {
                    Some(scripted) => scripted.answer(&method, JsonValue::Object(payload)),
                    None => Err(BackendError::new("capability_unavailable", false, empty())),
                };
                let reply = match answer {
                    Ok(payload) => object([
                        ("kind", JsonValue::String("response".into())),
                        ("id", JsonValue::String(id)),
                        ("payload", payload),
                    ]),
                    Err(error) => object([
                        ("kind", JsonValue::String("error".into())),
                        ("id", JsonValue::String(id)),
                        ("code", JsonValue::String(error.code().to_owned())),
                        ("retryable", JsonValue::Bool(error.retryable())),
                        ("details", error.details().clone()),
                    ]),
                };
                let _ = writer.send(&reply).await;
            }
            FrameKind::StreamOpen => {
                let Some(id) = string(&frame, "id") else {
                    continue;
                };
                let payload = frame.field("payload").cloned().unwrap_or_else(empty);
                let grant = payload
                    .get("grant")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let credit = frame
                    .field("initialCredit")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(INITIAL_CREDIT);
                let Some(scripted) = grants.get(&grant) else {
                    let close = object([
                        ("kind", JsonValue::String("stream_close".into())),
                        ("streamId", JsonValue::String(id)),
                        ("code", JsonValue::String("capability_unavailable".into())),
                    ]);
                    let _ = writer.send(&close).await;
                    continue;
                };
                scripted
                    .shared
                    .opens
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(payload);
                *scripted
                    .shared
                    .stream
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = StreamInner {
                    id: Some(id.clone()),
                    credit,
                    sequence: 0,
                    closed: false,
                };
                scripted.shared.stream.notify.notify_waiters();
                package_streams.insert(id, scripted.clone());
            }
            FrameKind::Credit => {
                let Some(id) = string(&frame, "streamId") else {
                    continue;
                };
                let amount = frame
                    .field("amount")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(0);
                if let Some(scripted) = package_streams.get(&id) {
                    scripted
                        .shared
                        .stream
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .credit += amount;
                    scripted.shared.stream.notify.notify_waiters();
                }
            }
            _ => {}
        }
    }
    // The session ended: nothing else will answer what is still waiting.
    let mut waiting = lock(&routes);
    for (_, sender) in std::mem::take(&mut waiting.requests) {
        let _ = sender.send(Err(BackendError::new("host_unavailable", true, empty())));
    }
    for (_, sender) in std::mem::take(&mut waiting.subscriptions) {
        let _ = sender.send(None);
    }
}

fn terminal_of(frame: &Frame) -> Result<JsonValue, BackendError> {
    if frame.kind() == FrameKind::Response {
        Ok(frame.field("payload").cloned().unwrap_or_else(empty))
    } else {
        Err(BackendError::new(
            frame
                .field("code")
                .and_then(JsonValue::as_str)
                .unwrap_or("backend_error"),
            frame
                .field("retryable")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
            frame.field("details").cloned().unwrap_or_else(empty),
        )
        .with_outcome_uncertain(
            frame
                .field("outcomeUncertain")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        ))
    }
}

async fn read_frame(reader: &mut DuplexStream) -> Option<Frame> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix).await.ok()?;
    let length = u32::from_be_bytes(prefix) as usize;
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await.ok()?;
    let mut bytes = prefix.to_vec();
    bytes.extend(payload);
    decode_frame(&bytes).ok()
}

fn string(frame: &Frame, name: &str) -> Option<String> {
    frame
        .field(name)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
}

fn grant_for(identity: &str) -> String {
    base64url(&format!("grant-{identity}"))
}

fn base64url(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

fn unix_millis(instant: SystemTime) -> u64 {
    instant
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn empty() -> JsonValue {
    JsonValue::Object(serde_json::Map::new())
}

fn lock(routes: &Arc<Mutex<Routes>>) -> std::sync::MutexGuard<'_, Routes> {
    routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
