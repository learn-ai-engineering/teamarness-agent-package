//! A typed request router with per-activation state.
//!
//! The [`Backend`] trait stays the escape hatch for protocol-level control. The router is what a
//! Package writes instead: typed request and response values, one place that owns the activation's
//! state, and the lifecycle conventions applied the same way in every Package.
//!
//! SDK-owned error codes use the reserved `package.` prefix. A Package's own codes use its request
//! namespace, so the two can never be confused.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;
use serde::de::DeserializeOwned;
use teamarness_package_protocol::JsonValue;
use tokio::sync::Mutex;

use crate::cancellation::CancellationToken;
use crate::ports::{PortActor, ProviderEnvelope};
use crate::{
    Backend, BackendError, BackendRequest, CapabilityClient, EventSubscription, Initialization,
    Package, SdkError,
};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type ActivateHook<S> =
    Arc<dyn Fn(Activation) -> BoxFuture<'static, Result<S, BackendError>> + Send + Sync>;
type StateHook<S> =
    Arc<dyn Fn(Arc<S>) -> BoxFuture<'static, Result<(), BackendError>> + Send + Sync>;
type RequestHandler<S> = Arc<
    dyn Fn(Context<S>, JsonValue) -> BoxFuture<'static, Result<JsonValue, BackendError>>
        + Send
        + Sync,
>;
type SubscriptionHandler<S> = Arc<
    dyn Fn(Context<S>, JsonValue) -> BoxFuture<'static, Result<EventSubscription, BackendError>>
        + Send
        + Sync,
>;

/// A request or event payload with no fields.
#[derive(Debug, Clone, Copy, Default, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// Why the host is activating the Package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationKind {
    /// A fresh activation.
    Activate,
    /// A replacement generation recovering an Instance that was already active.
    Recover,
}

/// What an activation hook is given.
pub struct Activation {
    initialization: Initialization,
    capabilities: CapabilityClient,
    cancellation: CancellationToken,
    kind: ActivationKind,
}

impl Activation {
    /// The values the host bound to this process generation.
    #[must_use]
    pub const fn initialization(&self) -> &Initialization {
        &self.initialization
    }

    /// The Package's own private directory.
    #[must_use]
    pub fn data_directory(&self) -> &Path {
        self.initialization.data_directory().path()
    }

    /// The Semantic Capability client for this generation.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilityClient {
        &self.capabilities
    }

    /// The signal that fires when the host cancels this activation.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Whether this is a fresh activation or a recovery.
    #[must_use]
    pub const fn kind(&self) -> ActivationKind {
        self.kind
    }
}

/// What a request or subscription handler is given, beside its typed payload.
pub struct Context<S> {
    state: Arc<S>,
    initialization: Initialization,
    capabilities: CapabilityClient,
    cancellation: CancellationToken,
    deadline: Option<SystemTime>,
}

impl<S> Clone for Context<S> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            initialization: self.initialization.clone(),
            capabilities: self.capabilities.clone(),
            cancellation: self.cancellation.clone(),
            deadline: self.deadline,
        }
    }
}

impl<S> Context<S> {
    /// The state this activation built.
    #[must_use]
    pub const fn state(&self) -> &Arc<S> {
        &self.state
    }

    /// The values the host bound to this process generation.
    #[must_use]
    pub const fn initialization(&self) -> &Initialization {
        &self.initialization
    }

    /// The Semantic Capability client for this generation.
    #[must_use]
    pub const fn capabilities(&self) -> &CapabilityClient {
        &self.capabilities
    }

    /// The signal that fires when the host cancels this request or its deadline passes.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// The instant the host stops waiting, when it named one.
    #[must_use]
    pub const fn deadline(&self) -> Option<SystemTime> {
        self.deadline
    }

    /// Runs blocking work off the async runtime, racing it against this request's cancellation.
    ///
    /// # Errors
    ///
    /// Returns the closure's error, `cancelled`, or `package.blocking_task_failed`.
    pub async fn blocking<T, F>(&self, work: F) -> Result<T, BackendError>
    where
        F: FnOnce(&CancellationToken) -> Result<T, BackendError> + Send + 'static,
        T: Send + 'static,
    {
        crate::blocking(&self.cancellation, work).await
    }
}

/// The operation names a router has handlers for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterOperations {
    pub requests: Vec<String>,
    pub subscriptions: Vec<String>,
}

enum RouterState<S> {
    Inactive,
    Active(Arc<S>),
    Suspended(Arc<S>),
}

/// A typed request router with per-activation state.
pub struct Router<S: Send + Sync + 'static> {
    activate: ActivateHook<S>,
    on_suspend: Option<StateHook<S>>,
    on_resume: Option<StateHook<S>>,
    on_dispose: Option<StateHook<S>>,
    requests: BTreeMap<&'static str, RequestHandler<S>>,
    subscriptions: BTreeMap<&'static str, SubscriptionHandler<S>>,
    port_requests: BTreeMap<(&'static str, &'static str, &'static str), RequestHandler<S>>,
    port_queries: BTreeMap<(&'static str, &'static str), SubscriptionHandler<S>>,
    port_events: BTreeMap<(&'static str, &'static str), SubscriptionHandler<S>>,
}

impl Router<()> {
    /// A router with no per-activation state.
    #[must_use]
    pub fn stateless() -> Self {
        Self::new(|_| async { Ok(()) })
    }
}

impl<S: Send + Sync + 'static> Router<S> {
    /// Creates a router whose activation hook builds the state every handler shares.
    #[must_use]
    pub fn new<F, Fut>(activate: F) -> Self
    where
        F: Fn(Activation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, BackendError>> + Send + 'static,
    {
        Self {
            activate: Arc::new(move |activation| Box::pin(activate(activation))),
            on_suspend: None,
            on_resume: None,
            on_dispose: None,
            requests: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            port_requests: BTreeMap::new(),
            port_queries: BTreeMap::new(),
            port_events: BTreeMap::new(),
        }
    }

    /// Runs before the Package is suspended. The state is kept.
    #[must_use]
    pub fn on_suspend<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(Arc<S>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BackendError>> + Send + 'static,
    {
        self.on_suspend = Some(Arc::new(move |state| Box::pin(hook(state))));
        self
    }

    /// Runs when a suspended Package is recovered in the same process.
    #[must_use]
    pub fn on_resume<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(Arc<S>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BackendError>> + Send + 'static,
    {
        self.on_resume = Some(Arc::new(move |state| Box::pin(hook(state))));
        self
    }

    /// Runs when the state is discarded, by dispose or by a replacing activation.
    #[must_use]
    pub fn on_dispose<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(Arc<S>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BackendError>> + Send + 'static,
    {
        self.on_dispose = Some(Arc::new(move |state| Box::pin(hook(state))));
        self
    }

    /// Registers a typed request handler.
    ///
    /// # Panics
    ///
    /// Panics when `operation` is already registered, is a `lifecycle.*` name, or breaks the UI
    /// contract operation grammar. These are programmer errors the first test finds.
    #[must_use]
    pub fn request<Req, Resp, F, Fut>(mut self, operation: &'static str, handler: F) -> Self
    where
        Req: DeserializeOwned + Send + 'static,
        Resp: Serialize + Send + 'static,
        F: Fn(Context<S>, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Resp, BackendError>> + Send + 'static,
    {
        check_operation(operation, self.requests.contains_key(operation));
        let handler = Arc::new(handler);
        self.requests.insert(
            operation,
            Arc::new(move |context, payload| {
                let handler = handler.clone();
                Box::pin(async move {
                    let request: Req =
                        serde_json::from_value(payload).map_err(|_| invalid_payload())?;
                    let response = handler(context, request).await?;
                    let value = serde_json::to_value(response).map_err(|_| invalid_response())?;
                    if value.is_object() {
                        Ok(value)
                    } else {
                        Err(invalid_response())
                    }
                })
            }),
        );
        self
    }

    /// Registers a typed subscription handler.
    ///
    /// # Panics
    ///
    /// Panics on the same programmer errors as [`Router::request`].
    #[must_use]
    pub fn subscription<Req, F, Fut>(mut self, operation: &'static str, handler: F) -> Self
    where
        Req: DeserializeOwned + Send + 'static,
        F: Fn(Context<S>, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<EventSubscription, BackendError>> + Send + 'static,
    {
        check_operation(operation, self.subscriptions.contains_key(operation));
        let handler = Arc::new(handler);
        self.subscriptions.insert(
            operation,
            Arc::new(move |context, payload| {
                let handler = handler.clone();
                Box::pin(async move {
                    let request: Req =
                        serde_json::from_value(payload).map_err(|_| invalid_payload())?;
                    handler(context, request).await
                })
            }),
        );
        self
    }

    /// Registers a generic command or ordinary query without a Package-specific wire operation.
    #[must_use]
    pub fn port_request<Req, Resp, F, Fut>(
        mut self,
        contract: &'static str,
        port: &'static str,
        kind: &'static str,
        handler: F,
    ) -> Self
    where
        Req: DeserializeOwned + Send + 'static,
        Resp: Serialize + Send + 'static,
        F: Fn(Context<S>, PortActor, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Resp, BackendError>> + Send + 'static,
    {
        assert!(matches!(kind, "command" | "query") && valid_operation_name(port));
        let key = (contract, port, kind);
        assert!(
            !self.port_requests.contains_key(&key),
            "port registered twice"
        );
        let handler = Arc::new(handler);
        self.port_requests.insert(
            key,
            Arc::new(move |context, payload| {
                let handler = handler.clone();
                Box::pin(async move {
                    let envelope: ProviderEnvelope =
                        serde_json::from_value(payload).map_err(|_| invalid_payload())?;
                    if envelope.major == 0 {
                        return Err(invalid_payload());
                    }
                    let request: Req =
                        serde_json::from_value(envelope.request).map_err(|_| invalid_payload())?;
                    let answer = handler(context, envelope.actor, request).await?;
                    let answer = serde_json::to_value(answer).map_err(|_| invalid_response())?;
                    Ok(crate::ports::encode_port_value(answer))
                })
            }),
        );
        self
    }

    /// Registers a live event source. The Host opens it once for the first subscriber.
    #[must_use]
    pub fn port_event<F, Fut>(
        mut self,
        contract: &'static str,
        port: &'static str,
        handler: F,
    ) -> Self
    where
        F: Fn(Context<S>, PortActor) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<EventSubscription, BackendError>> + Send + 'static,
    {
        assert!(valid_operation_name(port));
        let key = (contract, port);
        assert!(
            !self.port_events.contains_key(&key),
            "port registered twice"
        );
        let handler = Arc::new(handler);
        self.port_events.insert(
            key,
            Arc::new(move |context, payload| {
                let handler = handler.clone();
                Box::pin(async move {
                    let envelope: ProviderEnvelope =
                        serde_json::from_value(payload).map_err(|_| invalid_payload())?;
                    if envelope.major == 0 || envelope.request != serde_json::json!({}) {
                        return Err(invalid_payload());
                    }
                    handler(context, envelope.actor).await
                })
            }),
        );
        self
    }

    /// Registers a consumer-initiated streaming query; the SDK controls its stream credit.
    #[must_use]
    pub fn port_live_query<Req, F, Fut>(
        mut self,
        contract: &'static str,
        port: &'static str,
        handler: F,
    ) -> Self
    where
        Req: DeserializeOwned + Send + 'static,
        F: Fn(Context<S>, PortActor, Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<EventSubscription, BackendError>> + Send + 'static,
    {
        assert!(valid_operation_name(port));
        let key = (contract, port);
        assert!(
            !self.port_queries.contains_key(&key),
            "port registered twice"
        );
        let handler = Arc::new(handler);
        self.port_queries.insert(
            key,
            Arc::new(move |context, payload| {
                let handler = handler.clone();
                Box::pin(async move {
                    let envelope: ProviderEnvelope =
                        serde_json::from_value(payload).map_err(|_| invalid_payload())?;
                    if envelope.major == 0 {
                        return Err(invalid_payload());
                    }
                    let request: Req =
                        serde_json::from_value(envelope.request).map_err(|_| invalid_payload())?;
                    handler(context, envelope.actor, request).await
                })
            }),
        );
        self
    }

    /// The registered operation names, sorted, for comparison against the UI contract.
    #[must_use]
    pub fn operations(&self) -> RouterOperations {
        RouterOperations {
            requests: self
                .requests
                .keys()
                .map(|name| (*name).to_owned())
                .collect(),
            subscriptions: self
                .subscriptions
                .keys()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }

    /// The router as a [`Backend`], for a Package that drives the SDK itself.
    #[must_use]
    pub fn into_backend(self) -> Arc<dyn Backend> {
        Arc::new(RouterBackend {
            state: Mutex::new(RouterState::Inactive),
            activate: self.activate,
            on_suspend: self.on_suspend,
            on_resume: self.on_resume,
            on_dispose: self.on_dispose,
            requests: self.requests,
            subscriptions: self.subscriptions,
            port_requests: self.port_requests,
            port_queries: self.port_queries,
            port_events: self.port_events,
        })
    }

    /// Serves the Package Process Protocol over process stdin and stdout.
    ///
    /// # Errors
    ///
    /// Returns the transport failure that ended the session.
    pub async fn serve(self) -> Result<(), SdkError> {
        Package::from_backend(self.into_backend()).serve().await
    }

    /// Serves, and turns a transport failure into a process exit code.
    pub async fn run(self) -> ExitCode {
        match self.serve().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Package Backend stopped: {error}");
                ExitCode::FAILURE
            }
        }
    }
}

/// The router's own [`Backend`] implementation, including the lifecycle state machine.
struct RouterBackend<S: Send + Sync + 'static> {
    state: Mutex<RouterState<S>>,
    activate: ActivateHook<S>,
    on_suspend: Option<StateHook<S>>,
    on_resume: Option<StateHook<S>>,
    on_dispose: Option<StateHook<S>>,
    requests: BTreeMap<&'static str, RequestHandler<S>>,
    subscriptions: BTreeMap<&'static str, SubscriptionHandler<S>>,
    port_requests: BTreeMap<(&'static str, &'static str, &'static str), RequestHandler<S>>,
    port_queries: BTreeMap<(&'static str, &'static str), SubscriptionHandler<S>>,
    port_events: BTreeMap<(&'static str, &'static str), SubscriptionHandler<S>>,
}

impl<S: Send + Sync + 'static> RouterBackend<S> {
    /// The current state, when the Package is active.
    ///
    /// The lock is held only while reading, never across a Package handler's await.
    async fn active_state(&self) -> Result<Arc<S>, BackendError> {
        match &*self.state.lock().await {
            RouterState::Active(state) => Ok(state.clone()),
            RouterState::Inactive | RouterState::Suspended(_) => Err(not_active()),
        }
    }

    /// Takes the current state out, whatever it is.
    async fn take_state(&self) -> Option<Arc<S>> {
        let mut guard = self.state.lock().await;
        match std::mem::replace(&mut *guard, RouterState::Inactive) {
            RouterState::Inactive => None,
            RouterState::Active(state) | RouterState::Suspended(state) => Some(state),
        }
    }

    async fn dispose_state(&self, state: Option<Arc<S>>) -> Result<(), BackendError> {
        match (state, &self.on_dispose) {
            (Some(state), Some(hook)) => hook(state).await,
            _ => Ok(()),
        }
    }

    async fn activate(
        &self,
        request: &BackendRequest,
        kind: ActivationKind,
    ) -> Result<JsonValue, BackendError> {
        // A replacing activation disposes the state it replaces first.
        let previous = self.take_state().await;
        self.dispose_state(previous).await?;
        let activation = Activation {
            initialization: request.initialization.clone(),
            capabilities: request.capabilities.clone(),
            cancellation: request.cancellation().clone(),
            kind,
        };
        let state = Arc::new((self.activate)(activation).await?);
        *self.state.lock().await = RouterState::Active(state);
        Ok(empty_object())
    }

    async fn recover(&self, request: &BackendRequest) -> Result<JsonValue, BackendError> {
        let suspended = {
            let mut guard = self.state.lock().await;
            match std::mem::replace(&mut *guard, RouterState::Inactive) {
                RouterState::Suspended(state) | RouterState::Active(state) => Some(state),
                RouterState::Inactive => None,
            }
        };
        let Some(state) = suspended else {
            // A replacement generation has no state to resume, so it activates instead.
            return self.activate(request, ActivationKind::Recover).await;
        };
        if let Some(hook) = &self.on_resume {
            hook(state.clone()).await?;
        }
        *self.state.lock().await = RouterState::Active(state);
        Ok(empty_object())
    }

    async fn suspend(&self) -> Result<JsonValue, BackendError> {
        let Some(state) = self.take_state().await else {
            return Ok(empty_object());
        };
        if let Some(hook) = &self.on_suspend {
            hook(state.clone()).await?;
        }
        *self.state.lock().await = RouterState::Suspended(state);
        Ok(empty_object())
    }

    async fn dispose(&self) -> Result<JsonValue, BackendError> {
        let state = self.take_state().await;
        self.dispose_state(state).await?;
        Ok(empty_object())
    }

    async fn context(&self, request: &BackendRequest) -> Result<Context<S>, BackendError> {
        Ok(Context {
            state: self.active_state().await?,
            initialization: request.initialization.clone(),
            capabilities: request.capabilities.clone(),
            cancellation: request.cancellation().clone(),
            deadline: request.deadline(),
        })
    }
}

impl<S: Send + Sync + 'static> Backend for RouterBackend<S> {
    fn handle(
        &self,
        request: BackendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<JsonValue, BackendError>> + Send + '_>> {
        Box::pin(async move {
            match request.operation.as_str() {
                "lifecycle.activate" => self.activate(&request, ActivationKind::Activate).await,
                "lifecycle.recover" => self.recover(&request).await,
                "lifecycle.suspend" => self.suspend().await,
                "lifecycle.dispose" => self.dispose().await,
                operation => {
                    if matches!(
                        operation,
                        "teamarness.port.command.v1" | "teamarness.port.query.v1"
                    ) {
                        let envelope: ProviderEnvelope =
                            serde_json::from_value(request.payload.clone())
                                .map_err(|_| invalid_payload())?;
                        let kind = if operation == "teamarness.port.command.v1" {
                            "command"
                        } else {
                            "query"
                        };
                        let handler = self
                            .port_requests
                            .get(&(envelope.contract.as_str(), envelope.port.as_str(), kind))
                            .ok_or_else(|| unknown("package.unknown_port"))?
                            .clone();
                        return handler(self.context(&request).await?, request.payload).await;
                    }
                    let Some(handler) = self.requests.get(operation).cloned() else {
                        return Err(unknown("package.unknown_operation"));
                    };
                    let context = self.context(&request).await?;
                    handler(context, request.payload).await
                }
            }
        })
    }

    fn subscribe(
        &self,
        request: BackendRequest,
    ) -> Pin<Box<dyn Future<Output = Result<EventSubscription, BackendError>> + Send + '_>> {
        Box::pin(async move {
            if matches!(
                request.operation.as_str(),
                "teamarness.port.query.v1" | "teamarness.port.event.v1"
            ) {
                let envelope: ProviderEnvelope = serde_json::from_value(request.payload.clone())
                    .map_err(|_| invalid_payload())?;
                let ports = if request.operation == "teamarness.port.event.v1" {
                    &self.port_events
                } else {
                    &self.port_queries
                };
                let handler = ports
                    .get(&(envelope.contract.as_str(), envelope.port.as_str()))
                    .ok_or_else(|| unknown("package.unknown_port"))?
                    .clone();
                let mut subscription =
                    handler(self.context(&request).await?, request.payload).await?;
                subscription.port_values = true;
                return Ok(subscription);
            }
            let Some(handler) = self.subscriptions.get(request.operation.as_str()).cloned() else {
                return Err(unknown("package.unknown_subscription"));
            };
            let context = self.context(&request).await?;
            handler(context, request.payload).await
        })
    }
}

/// The contract's operation-name grammar.
///
/// Copied from `teamarness-package-contract` on purpose: the SDK is vendored into generated
/// projects and must not pull the contract crate in with it.
fn valid_operation_name(name: &str) -> bool {
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

fn check_operation(operation: &'static str, duplicate: bool) {
    assert!(
        !duplicate,
        "operation {operation} is registered twice on this router"
    );
    assert!(
        !operation.starts_with("lifecycle."),
        "operation {operation} is a lifecycle name the router owns"
    );
    assert!(
        valid_operation_name(operation),
        "operation {operation} breaks the contract operation grammar"
    );
}

fn empty_object() -> JsonValue {
    JsonValue::Object(serde_json::Map::new())
}

fn not_active() -> BackendError {
    BackendError::new("package.not_active", true, empty_object())
}

fn invalid_payload() -> BackendError {
    BackendError::new("package.invalid_payload", false, empty_object())
}

fn invalid_response() -> BackendError {
    BackendError::new("invalid_response", false, empty_object())
}

fn unknown(code: &'static str) -> BackendError {
    BackendError::new(code, false, empty_object())
}
