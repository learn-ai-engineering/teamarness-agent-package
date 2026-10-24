//! Rust SDK for Package Backend executables.
//!
//! Package code implements [`Backend`]. The SDK owns framed standard I/O,
//! concurrent request dispatch, lifecycle barriers, capability correlation,
//! control-frame priority, cancellation, and the parent-stdin lifeline.

#![forbid(unsafe_code)]

mod blocking;
mod cancellation;
pub mod capabilities;
mod capability;
mod dispatcher;
mod initialization;
mod lifecycle;
mod parent_lifeline;
pub mod port_observation;
pub mod ports;
mod priority_writer;
mod reader;
mod router;
pub mod storage;
#[cfg(feature = "testing")]
pub mod testing;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

pub use blocking::blocking;
pub use cancellation::CancellationToken;
pub use capability::{CapabilityClient, CapabilitySubscription, StreamCredit};
pub use initialization::{
    CapabilityGrant, DataDirectory, Initialization, InstallationInfo, PackageInfo, WorkspaceInfo,
};
pub use lifecycle::LifecycleOperation;
pub use router::{Activation, ActivationKind, Context, Empty, Router, RouterOperations};
use teamarness_package_protocol::JsonValue;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

/// One host request delivered without framing details.
#[derive(Debug, Clone)]
pub struct BackendRequest {
    pub operation: String,
    pub payload: JsonValue,
    pub initialization: Initialization,
    pub capabilities: CapabilityClient,
    pub(crate) cancellation: CancellationToken,
    pub(crate) deadline: Option<SystemTime>,
}

impl BackendRequest {
    /// The signal that fires when the host cancels this request or its deadline passes.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// The instant the host stops waiting for this request, when it named one.
    #[must_use]
    pub const fn deadline(&self) -> Option<SystemTime> {
        self.deadline
    }
}

/// A bounded Package-owned terminal error.
#[derive(Debug, Clone)]
pub struct BackendError {
    code: String,
    retryable: bool,
    outcome_uncertain: bool,
    details: JsonValue,
}

impl BackendError {
    /// Creates an error. Invalid codes are replaced with `backend_error`.
    #[must_use]
    pub fn new(code: impl Into<String>, retryable: bool, details: JsonValue) -> Self {
        let code = code.into();
        let code = if !code.is_empty()
            && code.len() <= 128
            && code.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
            }) {
            code
        } else {
            "backend_error".into()
        };
        Self {
            code,
            retryable,
            outcome_uncertain: false,
            details: if details.is_object() {
                details
            } else {
                JsonValue::Object(Default::default())
            },
        }
    }

    /// Returns the Package-owned stable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Reports whether the caller may retry the operation.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// Whether a dispatched command may already have taken effect. Never replay it automatically.
    #[must_use]
    pub const fn outcome_uncertain(&self) -> bool {
        self.outcome_uncertain
    }

    /// Marks a dispatched command's error as possibly committed. This also forbids retry.
    #[must_use]
    pub fn with_outcome_uncertain(mut self, uncertain: bool) -> Self {
        self.outcome_uncertain = uncertain;
        if uncertain {
            self.retryable = false;
        }
        self
    }

    /// The bounded object the Package attached to this error.
    #[must_use]
    pub const fn details(&self) -> &JsonValue {
        &self.details
    }
}

/// Sending half of a bounded Package-owned event adapter.
#[derive(Clone)]
pub struct EventSender {
    sender: mpsc::Sender<Result<JsonValue, BackendError>>,
}

impl EventSender {
    /// Resolves when the Package event receiver has closed (for detached publishers).
    pub async fn closed(&self) {
        self.sender.closed().await;
    }

    /// Sends one object event, waiting only on the SDK's bounded Package queue.
    pub async fn send(&self, payload: JsonValue) -> Result<(), SdkError> {
        if !payload.is_object() {
            return Err(SdkError::Protocol("malformed_frame"));
        }
        self.sender
            .send(Ok(payload))
            .await
            .map_err(|_| SdkError::ParentClosed)
    }

    /// Sends any JSON Port value. The Port router wraps it for the object-only frame.
    /// Ordinary UI events still require objects through [`Self::send`].
    pub async fn send_port_value(&self, payload: JsonValue) -> Result<(), SdkError> {
        self.sender
            .send(Ok(payload))
            .await
            .map_err(|_| SdkError::ParentClosed)
    }

    /// Closes the event stream with a Package-owned error.
    pub async fn fail(&self, error: BackendError) -> Result<(), SdkError> {
        self.sender
            .send(Err(error))
            .await
            .map_err(|_| SdkError::ParentClosed)
    }
}

/// Receiving half consumed by the SDK dispatcher.
pub struct EventSubscription {
    pub(crate) receiver: mpsc::Receiver<Result<JsonValue, BackendError>>,
    pub(crate) port_values: bool,
}

/// Creates a bounded event adapter. Capacity is clamped to 1 through 32.
#[must_use]
pub fn event_channel(capacity: usize) -> (EventSender, EventSubscription) {
    let (sender, receiver) = mpsc::channel(capacity.clamp(1, 32));
    (
        EventSender { sender },
        EventSubscription {
            receiver,
            port_values: false,
        },
    )
}

/// Package-owned lifecycle, request, and event implementation.
pub trait Backend: Send + Sync + 'static {
    fn handle(
        &self,
        request: BackendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<JsonValue, BackendError>> + Send + '_>>;

    fn subscribe(
        &self,
        _request: BackendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<EventSubscription, BackendError>> + Send + '_>> {
        Box::pin(async {
            Err(BackendError::new(
                "unsupported_stream",
                false,
                JsonValue::Object(Default::default()),
            ))
        })
    }
}

/// Builder for a Package Backend process service.
pub struct Package {
    backend: Arc<dyn Backend>,
    port_observations:
        Option<tokio::sync::broadcast::Sender<port_observation::ProviderPortObservation>>,
}

impl Package {
    /// Creates an SDK service around Package-owned behavior.
    #[must_use]
    pub fn builder<B>(backend: Arc<B>) -> Self
    where
        B: Backend,
    {
        Self {
            backend,
            port_observations: None,
        }
    }

    /// Creates an SDK service around an already-erased Backend, as [`Router`] does.
    ///
    /// A Package that wants `serve_io` for its own tests can use this with
    /// [`Router::into_backend`].
    #[must_use]
    pub fn from_backend(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            port_observations: None,
        }
    }

    /// Opt in to a bounded, read-only diagnostic receiver. Lag reports a dropped observation;
    /// no Provider request or event ever waits for this receiver.
    #[must_use]
    pub fn observe_ports(mut self) -> (Self, port_observation::ProviderPortObservations) {
        let (sender, receiver) = tokio::sync::broadcast::channel(128);
        self.port_observations = Some(sender);
        (self, receiver)
    }

    /// Serves the protocol over process stdin and stdout.
    pub async fn serve(self) -> Result<(), SdkError> {
        self.serve_source(InputSource::ProcessStdio).await
    }

    /// Serves the protocol over supplied I/O. This is useful for conformance tests.
    pub async fn serve_io<R, W>(self, input: R, output: W) -> Result<(), SdkError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_source(InputSource::Supplied(Box::new(input), Box::new(output)))
            .await
    }

    /// Runs the control plane on its own thread and runtime.
    ///
    /// Framing, dispatch and `ping` belong to a dedicated `current_thread` runtime on a named OS
    /// thread, so a Package handler that blocks every worker of the Package's own runtime cannot
    /// stop the host from getting a `pong`. Handler and subscription tasks are spawned back onto
    /// the Package runtime this call was made from.
    async fn serve_source(self, source: InputSource) -> Result<(), SdkError> {
        let package_runtime = tokio::runtime::Handle::current();
        let (result_tx, result_rx) = oneshot::channel();
        let backend = self.backend;
        let observations = self.port_observations;
        let thread = std::thread::Builder::new()
            .name("teamarness-package-control".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = result_tx.send(Err(SdkError::ParentClosed));
                        return;
                    }
                };
                let outcome =
                    runtime.block_on(control_loop(backend, source, package_runtime, observations));
                let _ = result_tx.send(outcome);
            })
            .map_err(|_| SdkError::ParentClosed)?;
        let result = result_rx.await.unwrap_or(Err(SdkError::ParentClosed));
        // The thread is finished once it has sent its result; joining it off the async worker
        // keeps the runtime free while the control runtime drops.
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        result
    }
}

/// Where the control loop reads and writes frames.
enum InputSource {
    ProcessStdio,
    Supplied(
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ),
}

async fn control_loop(
    backend: Arc<dyn Backend>,
    source: InputSource,
    package_runtime: tokio::runtime::Handle,
    observations: Option<tokio::sync::broadcast::Sender<port_observation::ProviderPortObservation>>,
) -> Result<(), SdkError> {
    // Opening the process files belongs to the control runtime, so their blocking I/O never lands
    // on a Package worker.
    let (input, output): (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) = match source {
        InputSource::Supplied(input, output) => (input, output),
        InputSource::ProcessStdio => (
            Box::new(
                tokio::fs::File::open("/dev/stdin")
                    .await
                    .map_err(|_| SdkError::ParentClosed)?,
            ),
            Box::new(
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/stdout")
                    .await
                    .map_err(|_| SdkError::ParentClosed)?,
            ),
        ),
    };
    let (writer, control, data) = priority_writer::PriorityWriter::channels();
    let writer = Arc::new(writer);
    // One classifier for this Package's whole session: the reader, the writer
    // and every capability client share it, so neither direction can hold its
    // own idea of which lifecycle races are tolerable.
    let state: priority_writer::SharedState = Arc::new(tokio::sync::Mutex::new(
        teamarness_package_protocol::ContractState::default(),
    ));
    let (stop_writer, writer_stopped) = oneshot::channel();
    let writer_task = tokio::spawn(priority_writer::run(
        output,
        control,
        data,
        writer_stopped,
        state.clone(),
    ));
    let (frames_tx, frames_rx) = mpsc::channel(teamarness_package_protocol::MAX_WRITER_FRAMES);
    let reader_task = tokio::spawn(reader::run(input, frames_tx));
    let pending: capability::PendingCapabilities =
        Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new()));
    let streams: capability::CapabilityStreams =
        Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new()));
    let result = dispatcher::run(
        backend,
        writer.clone(),
        frames_rx,
        pending.clone(),
        streams.clone(),
        package_runtime,
        state,
        observations,
    )
    .await;
    reader_task.abort();
    // Nothing will answer a pending capability call once the dispatcher has
    // returned, so every caller is told so before the writer stops.
    capability::fail_all(&pending, &streams).await;
    drop(writer);
    // Package-owned tasks may still hold `CapabilityClient` clones, which keep
    // writer senders alive. Stop the writer explicitly so the session always
    // ends once the dispatcher has returned (shutdown frame or parent closure).
    let _ = stop_writer.send(());
    match writer_task.await {
        Ok(Ok(())) | Err(_) => result,
        Ok(Err(error)) => Err(error),
    }
}

/// Stable SDK transport failures. Payloads and operating-system text are excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdkError {
    ParentClosed,
    Protocol(&'static str),
}

impl SdkError {
    fn protocol(error: teamarness_package_protocol::ProtocolError) -> Self {
        Self::Protocol(error.code())
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentClosed => formatter.write_str("parent_closed"),
            Self::Protocol(code) => formatter.write_str(code),
        }
    }
}

impl Error for SdkError {}

fn object<const N: usize>(entries: [(&str, JsonValue); N]) -> JsonValue {
    let mut value = JsonValue::Object(Default::default());
    let fields = value.as_object_mut().expect("new object");
    for (name, entry) in entries {
        fields.insert(name.to_owned(), entry);
    }
    value
}
