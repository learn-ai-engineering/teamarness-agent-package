#![forbid(unsafe_code)]

//! The Agent Package Backend: Package-owned settings and key, the Agent Binding held by Core and
//! read through `teamarness.agent-binding`, the status its UI shows, and the reply harness
//! ([`harness`]) with its model client ([`model`]) that runs while the status is in the ready
//! family, the Steward's direct [`chat`] with the same model, and the Steward's Package
//! enablement ([`packages`]), which [`access`] combines with the [`catalog`] and Host availability.
//! A turn's [`tools`] are the usable contracts' ports plus the Agent's compiled-in [`skills`].

pub mod access;
pub mod catalog;
pub mod chat;
pub mod harness;
pub mod mention;
pub mod model;
pub mod packages;
pub mod progress;
pub mod settings;
pub mod skills;
pub mod status;
pub mod tools;
pub mod turn;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;
use teamarness_package_sdk::capabilities::agent_binding::{
    AgentBinding, AgentCandidate, BoundAgent,
};
use teamarness_package_sdk::capabilities::principals::Principals;
use teamarness_package_sdk::ports::PortClient;
use teamarness_package_sdk::{
    BackendError, CancellationToken, Context, Empty, EventSubscription, Router, event_channel,
};
use tokio::sync::watch;

use access::{Availability, PackageView};
use catalog::{BundledCatalog, Catalog};
use chat::{ChatCell, ChatView};
use harness::{HarnessInputs, TaskHandle};
use model::{ConfiguredModel, ModelClient};
use packages::PackagesStore;
use progress::Progress;
use settings::{KeyError, Settings, SettingsStore};
use status::{AgentSummary, Status, StatusCell};
use turn::ToolExecutor;

fn error(code: &str, retryable: bool) -> BackendError {
    BackendError::new(code, retryable, json!({}))
}

fn storage_failed() -> BackendError {
    error("agent.storage_failed", true)
}

fn invalid_request() -> BackendError {
    error("agent.invalid_request", false)
}

/// Maps a capability failure to a Package-owned code. Never carries payload or secrets.
fn capability_error(cause: &BackendError) -> BackendError {
    match cause.code() {
        "agent_not_stewarded" => error("agent.not_stewarded", false),
        "agent_not_found" => error("agent.agent_not_found", false),
        "capability_request_invalid" | "package.invalid_capability_request" => invalid_request(),
        code => BackendError::new(
            "agent.capability_unavailable",
            cause.retryable(),
            json!({ "causeCode": code }),
        ),
    }
}

/// The background tasks one activation owns.
#[derive(Default)]
struct Tasks {
    /// Watches the Principal roster so a removed or renamed Agent reaches the status.
    roster: Option<TaskHandle>,
    enabled: bool,
    /// No reply work starts until startup/resume binding reconciliation succeeds.
    reconciled: bool,
    /// The reply harness; runs while configuration and binding are both present.
    harness: Option<TaskHandle>,
    /// The chat turn in flight, if any.
    chat: Option<TaskHandle>,
}

/// Everything one activation of the Agent Package owns.
#[derive(Clone)]
pub struct AgentActive {
    settings: Arc<Mutex<SettingsStore>>,
    status: Arc<StatusCell>,
    binding: AgentBinding,
    principals: Principals,
    ports: Option<PortClient>,
    progress: Arc<Mutex<Progress>>,
    cancellation: CancellationToken,
    tasks: Arc<tokio::sync::Mutex<Tasks>>,
    /// Serializes mutations, reconciliation and readiness against harness replacement.
    transition: Arc<tokio::sync::Mutex<()>>,
    chat: Arc<ChatCell>,
    /// The Steward's Package enablement, `packages.json`.
    packages: Arc<Mutex<PackagesStore>>,
    /// The Provider contracts the Agent knows; [`access`] decides which are usable.
    catalog: &'static (dyn Catalog + Send + Sync),
}

/// What one chat turn needs from the stored configuration. Holds the key; never logged.
struct ChatConfig {
    endpoint_base_url: String,
    model: String,
    system_prompt: String,
    context_messages: usize,
    api_key: String,
    /// The Steward's enabled Packages when the turn starts.
    enabled: Vec<String>,
}

impl AgentActive {
    /// Called under `transition`; drain old replies before publishing a different identity/name.
    async fn apply_agent(&self, agent: Option<BoundAgent>) {
        if self.status.agent() != agent {
            self.stop_harness().await;
            self.status.set_agent(agent);
        }
        self.sync_harness().await;
    }

    async fn stop_harness(&self) {
        if let Some(previous) = self.tasks.lock().await.harness.take() {
            previous.stop().await;
        }
    }

    /// Read metadata on the blocking pool, not the async lifecycle worker.
    async fn configured(&self, cancellation: &CancellationToken) -> Result<bool, BackendError> {
        let settings = self.settings.clone();
        teamarness_package_sdk::blocking(cancellation, move |cancel| {
            if cancel.is_cancelled() {
                return Err(storage_failed());
            }
            Ok(settings
                .lock()
                .map_err(|_| storage_failed())?
                .is_configured())
        })
        .await
    }

    /// Successful settings/key changes clear stale errors, but complete edits keep active work.
    async fn apply_configured(&self, configured: bool) {
        if !configured {
            self.stop_harness().await;
        }
        self.status.set_configured(configured);
        self.sync_harness().await;
    }

    /// Starts the watcher while the transition gate is held.
    async fn start_roster_watch(&self) {
        let state = self.clone();
        let handle = TaskHandle::spawn(&self.cancellation, move |cancellation| async move {
            watch_roster(&state, &cancellation).await;
        });
        let mut tasks = self.tasks.lock().await;
        debug_assert!(tasks.roster.is_none());
        tasks.roster = Some(handle);
    }

    /// The inputs a harness task needs, or `None` when the Package has no Port grant.
    fn harness_inputs(&self) -> Option<HarnessInputs> {
        Some(HarnessInputs {
            ports: self.ports.clone()?,
            principals: self.principals.clone(),
            settings: self.settings.clone(),
            status: self.status.clone(),
            progress: self.progress.clone(),
            packages: self.packages.clone(),
            catalog: self.catalog,
        })
    }

    /// Brings the harness in line with the status: running while configuration and binding are
    /// both present, stopped otherwise. A harness that ended on its own is replaced.
    ///
    /// Called after every settings, key and binding change and on resume. The harness also
    /// watches the status itself and returns when it leaves the ready family, so this is what
    /// starts it again.
    async fn sync_harness(&self) {
        let mut tasks = self.tasks.lock().await;
        if tasks.enabled && tasks.reconciled && self.status.is_ready_to_run() {
            if tasks
                .harness
                .as_ref()
                .is_some_and(|task| !task.is_finished())
            {
                return;
            }
            if let Some(previous) = tasks.harness.take() {
                previous.stop().await;
            }
            if let Some(inputs) = self.harness_inputs() {
                tasks.harness = Some(harness::start(&self.cancellation, inputs));
            }
        } else if let Some(previous) = tasks.harness.take() {
            previous.stop().await;
        }
    }

    /// Stops every background task; the state itself is kept.
    async fn stop_tasks(&self) {
        // Cancel roster reads/waits first, but never abort a transition halfway through draining.
        let roster = self.tasks.lock().await.roster.take();
        if let Some(task) = &roster {
            task.cancel();
        }
        let _transition = self.transition.lock().await;
        if let Some(task) = roster {
            task.stop().await;
        }
        {
            let mut tasks = self.tasks.lock().await;
            tasks.enabled = false;
            tasks.reconciled = false;
        }
        self.stop_harness().await;
        self.stop_chat().await;
        self.chat.update(chat::ChatSession::interrupt);
    }

    async fn stop_chat(&self) {
        let turn = self.tasks.lock().await.chat.take();
        if let Some(turn) = turn {
            turn.stop().await;
        }
    }

    /// Reads endpoint, model, prompt and key on the blocking pool; `None` when any is missing.
    async fn chat_config(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Option<ChatConfig>, BackendError> {
        let settings = self.settings.clone();
        let packages = self.packages.clone();
        teamarness_package_sdk::blocking(cancellation, move |cancel| {
            if cancel.is_cancelled() {
                return Err(storage_failed());
            }
            let store = settings.lock().map_err(|_| storage_failed())?;
            let current = store.settings();
            if !current.is_complete() {
                return Ok(None);
            }
            let Some(api_key) = store.api_key().map_err(|_| storage_failed())? else {
                return Ok(None);
            };
            let enabled = packages
                .lock()
                .map_err(|_| storage_failed())?
                .enabled()
                .to_vec();
            Ok(Some(ChatConfig {
                endpoint_base_url: current.endpoint_base_url.clone(),
                model: current.model.clone(),
                system_prompt: current.system_prompt.clone(),
                context_messages: current.context_messages as usize,
                api_key,
                enabled,
            }))
        })
        .await
    }

    /// Records the Steward's message and starts its turn in the background, so the request
    /// answers before the Host's UI deadline whatever the model takes.
    ///
    /// The turn offers the tools of the Packages usable now. Availability is read before
    /// anything is recorded, and only when a Package is enabled; the Host's deadline bounds it,
    /// as it does for `agent.packages.get`.
    async fn start_chat(
        &self,
        content: &str,
        config: ChatConfig,
    ) -> Result<ChatView, BackendError> {
        let usable = access::usable_now(self.catalog, self.ports.as_ref(), &config.enabled).await;
        let name = self.status.agent().and_then(|agent| agent.name);
        let system = chat::system_message(
            name.as_deref(),
            &config.system_prompt,
            &tools::ToolContext::for_turn(&usable, skills::bundled()),
        );
        let (turn, view) = self
            .chat
            .update(|session| session.begin(content, system, config.context_messages));
        let turn = turn.map_err(|code| error(code, code == chat::CHAT_BUSY))?;
        let cell = self.chat.clone();
        let ports = self.ports.clone();
        let task = TaskHandle::spawn(&self.cancellation, move |cancellation| async move {
            let generation = turn.generation;
            let (system, history) = turn.into_parts();
            let outcome = match (
                ModelClient::new(),
                tools::Toolset::for_turn(&usable, ports, skills::bundled()),
            ) {
                (Ok(client), Ok(toolset)) => {
                    let ChatConfig {
                        endpoint_base_url,
                        model,
                        api_key,
                        ..
                    } = config;
                    let model = ConfiguredModel::new(client, endpoint_base_url, model, api_key);
                    let turn = turn::Turn::new(
                        system,
                        history,
                        toolset.definitions().to_vec(),
                        turn::TurnBounds::PRODUCTION,
                    );
                    turn::drive(turn, &model, &toolset, &cancellation).await
                }
                (Err(failure), _) => turn::TurnOutcome::Failed(failure.code, failure.retryable),
                // The bundled catalog always loads; a tool set that cannot be built is reported,
                // never a panic.
                (_, Err(_)) => turn::TurnOutcome::Failed(turn::TOOL_UNAVAILABLE, false),
            };
            if outcome == turn::TurnOutcome::Cancelled {
                return;
            }
            cell.update(|session| session.finish(generation, outcome));
        });
        // Any earlier task has settled or belongs to a cleared generation.
        let previous = self.tasks.lock().await.chat.replace(task);
        if let Some(previous) = previous {
            previous.stop().await;
        }
        Ok(view)
    }

    async fn change_binding(&self, principal: Option<String>) -> Result<BindingView, BackendError> {
        let _transition = self.transition.lock().await;
        self.stop_harness().await;
        let result = match principal {
            Some(principal) => self.binding.bind(&principal).await.map(Some),
            None => self.binding.unbind().await.map(|()| None),
        };
        let agent = match result {
            Ok(agent) => agent,
            Err(cause) => {
                self.sync_harness().await;
                return Err(capability_error(&cause));
            }
        };
        let summary = agent.as_ref().map(AgentSummary::from);
        self.tasks.lock().await.reconciled = true;
        self.apply_agent(agent).await;
        Ok(BindingView { agent: summary })
    }

    /// What the Host reports available now; no Port grant or a Host error means nothing is.
    async fn availability(&self) -> Availability {
        match &self.ports {
            Some(ports) => Availability::from_result(&ports.available().await),
            None => Availability::none(),
        }
    }

    /// The Steward's view of every agent-facing contract. Reads only the in-memory store.
    fn packages_view(&self, availability: &Availability) -> Result<PackagesView, BackendError> {
        let store = self.packages.lock().map_err(|_| storage_failed())?;
        Ok(PackagesView {
            packages: access::listed(self.catalog, availability, store.enabled()),
        })
    }

    fn settings_view(&self) -> SettingsView {
        let store = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SettingsView {
            settings: store.settings().clone(),
            has_key: store.has_key(),
        }
    }
}

/// Read the binding under the same gate as explicit bind/unbind and lifecycle changes.
async fn refresh_binding(state: &AgentActive, cancellation: &CancellationToken) -> Result<(), ()> {
    let _transition = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(()),
        transition = state.transition.lock() => transition,
    };
    let agent = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(()),
        agent = state.binding.current() => agent.map_err(|_| ())?,
    };
    state.tasks.lock().await.reconciled = true;
    // Once replacement starts, finish draining even if the watcher is cancelled meanwhile.
    state.apply_agent(agent).await;
    Ok(())
}

/// Capture the roster cursor before reconciliation, then subscribe after that cursor. Changes
/// during reconciliation remain replayable; initial/reconnect snapshots are never just discarded.
/// Reconciliation failures restart this sequence without needing any future invalidation.
async fn watch_roster(state: &AgentActive, cancellation: &CancellationToken) {
    const RETRY: std::time::Duration = std::time::Duration::from_secs(1);
    loop {
        let attempt = async {
            let cursor = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok::<(), ()>(()),
                snapshot = state.principals.current() => snapshot.map_err(|_| ())?.cursor,
            };
            refresh_binding(state, cancellation).await?;
            let mut stream = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Ok(()),
                stream = state.principals.subscribe(cursor) => stream.map_err(|_| ())?,
            };
            loop {
                let next = tokio::select! {
                    next = stream.next() => next,
                    () = cancellation.cancelled() => { stream.close().await; return Ok(()); }
                };
                match next {
                    Some(Ok(_)) => {
                        if let Err(error) = refresh_binding(state, cancellation).await {
                            stream.close().await;
                            return Err(error);
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            stream.close().await;
            Ok(())
        };
        let _ = attempt.await;
        tokio::select! {
            () = cancellation.cancelled() => return,
            () = tokio::time::sleep(RETRY) => {}
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    settings: Settings,
    has_key: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyView {
    has_key: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsSetRequest {
    settings: Settings,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KeySetRequest {
    api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChatSendRequest {
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingSetRequest {
    principal_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PackagesSetRequest {
    contract: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PackagesView {
    packages: Vec<PackageView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BindingView {
    agent: Option<AgentSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateView {
    principal_id: String,
    name: Option<String>,
    bound: bool,
}

impl From<AgentCandidate> for CandidateView {
    fn from(candidate: AgentCandidate) -> Self {
        Self {
            principal_id: candidate.principal_id,
            name: candidate.name,
            bound: candidate.bound,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidatesView {
    agents: Vec<CandidateView>,
}

/// Forwards every value a watch channel holds, the current one first, until either side closes.
fn watch_subscription<T>(mut receiver: watch::Receiver<T>) -> EventSubscription
where
    T: Serialize + Clone + Send + Sync + 'static,
{
    let (sender, subscription) = event_channel(4);
    tokio::spawn(async move {
        loop {
            let value = receiver.borrow_and_update().clone();
            let Ok(payload) = serde_json::to_value(value) else {
                return;
            };
            if sender.send(payload).await.is_err() {
                return;
            }
            tokio::select! {
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = sender.closed() => return,
            }
        }
    });
    subscription
}

/// The Agent Backend.
#[must_use]
pub fn router() -> Router<AgentActive> {
    Router::new(move |activation| {
        let initialization = activation.initialization().clone();
        let capabilities = activation.capabilities().clone();
        let cancellation = activation.cancellation().clone();
        async move {
            let directory = initialization.data_directory().path().to_owned();
            let (store, packages, progress, configured) =
                teamarness_package_sdk::blocking(&cancellation, move |cancel| {
                    if cancel.is_cancelled() {
                        return Err(storage_failed());
                    }
                    let store = SettingsStore::open(&directory).map_err(|_| storage_failed())?;
                    let packages = PackagesStore::open(&directory).map_err(|_| storage_failed())?;
                    let progress = Progress::open(&directory).map_err(|_| storage_failed())?;
                    let configured = store.is_configured();
                    Ok((store, packages, progress, configured))
                })
                .await?;
            let binding = AgentBinding::from_initialization(&initialization, capabilities.clone())
                .map_err(|cause| capability_error(&cause))?;
            let principals = Principals::from_initialization(&initialization, capabilities.clone())
                .map_err(|cause| capability_error(&cause))?;
            let ports = initialization
                .capability("teamarness.package-ports")
                .map(|grant| PortClient::new(capabilities.clone(), grant.grant.clone()));
            let agent = binding
                .current()
                .await
                .map_err(|cause| capability_error(&cause))?;
            let status = Arc::new(StatusCell::new(configured, agent));
            // Background tasks hang off a token the activation's own request cannot cancel
            // once it has answered; suspend and dispose stop them explicitly.
            let background = CancellationToken::new();
            let state = AgentActive {
                settings: Arc::new(Mutex::new(store)),
                status,
                binding,
                principals,
                ports,
                progress: Arc::new(Mutex::new(progress)),
                cancellation: background,
                tasks: Arc::new(tokio::sync::Mutex::new(Tasks {
                    roster: None,
                    harness: None,
                    chat: None,
                    enabled: true,
                    reconciled: false,
                })),
                transition: Arc::new(tokio::sync::Mutex::new(())),
                chat: Arc::new(ChatCell::default()),
                packages: Arc::new(Mutex::new(packages)),
                catalog: BundledCatalog::get(),
            };
            // Reconcile the startup roster before starting any reply work.
            {
                let _transition = state.transition.lock().await;
                state.start_roster_watch().await;
            }
            Ok(state)
        }
    })
    .on_resume(|state: Arc<AgentActive>| async move {
        let _transition = state.transition.lock().await;
        state.tasks.lock().await.enabled = true;
        let configured = state.configured(&state.cancellation).await?;
        state.status.set_configured(configured);
        // The watcher reconciles binding before subscribing and retries any read failure.
        state.start_roster_watch().await;
        Ok(())
    })
    .on_suspend(|state: Arc<AgentActive>| async move {
        state.stop_tasks().await;
        Ok(())
    })
    .on_dispose(|state: Arc<AgentActive>| async move {
        state.stop_tasks().await;
        state.cancellation.cancel();
        Ok(())
    })
    .request(
        "agent.settings.get",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            context.blocking(move |_| Ok(state.settings_view())).await
        },
    )
    .request(
        "agent.settings.set",
        |context: Context<AgentActive>, request: SettingsSetRequest| async move {
            let settings = request.settings.normalized();
            if let Err(field) = settings.validate() {
                return Err(BackendError::new(
                    "agent.invalid_settings",
                    false,
                    json!({ "field": field }),
                ));
            }
            let state = context.state().clone();
            let _transition = state.transition.lock().await;
            let (view, configured) = {
                let state = state.clone();
                context
                    .blocking(move |cancel| {
                        if cancel.is_cancelled() {
                            return Err(storage_failed());
                        }
                        state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .replace(settings)
                            .map_err(|_| storage_failed())?;
                        let configured = state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .is_configured();
                        Ok((state.settings_view(), configured))
                    })
                    .await?
            };
            state.apply_configured(configured).await;
            Ok(view)
        },
    )
    .request(
        "agent.key.set",
        |context: Context<AgentActive>, request: KeySetRequest| async move {
            let state = context.state().clone();
            let _transition = state.transition.lock().await;
            let (view, configured) = {
                let state = state.clone();
                context
                    .blocking(move |cancel| {
                        if cancel.is_cancelled() {
                            return Err(storage_failed());
                        }
                        let outcome = {
                            let store = state.settings.lock().map_err(|_| storage_failed())?;
                            store.set_key(&request.api_key)
                        };
                        match outcome {
                            Ok(()) => {}
                            Err(KeyError::Invalid) => {
                                return Err(error("agent.invalid_key", false));
                            }
                            Err(KeyError::Io(_)) => return Err(storage_failed()),
                        }
                        let configured = state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .is_configured();
                        Ok((
                            KeyView {
                                has_key: state.settings_view().has_key,
                            },
                            configured,
                        ))
                    })
                    .await?
            };
            state.apply_configured(configured).await;
            Ok(view)
        },
    )
    .request(
        "agent.key.clear",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            let _transition = state.transition.lock().await;
            let (view, configured) = {
                let state = state.clone();
                context
                    .blocking(move |cancel| {
                        if cancel.is_cancelled() {
                            return Err(storage_failed());
                        }
                        state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .clear_key()
                            .map_err(|_| storage_failed())?;
                        let configured = state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .is_configured();
                        Ok((
                            KeyView {
                                has_key: state.settings_view().has_key,
                            },
                            configured,
                        ))
                    })
                    .await?
            };
            state.apply_configured(configured).await;
            Ok(view)
        },
    )
    .request(
        "agent.binding.candidates",
        |context: Context<AgentActive>, _: Empty| async move {
            let agents = context
                .state()
                .binding
                .candidates()
                .await
                .map_err(|cause| capability_error(&cause))?;
            Ok(CandidatesView {
                agents: agents.into_iter().map(CandidateView::from).collect(),
            })
        },
    )
    .request(
        "agent.binding.set",
        |context: Context<AgentActive>, request: BindingSetRequest| async move {
            let state = context.state().clone();
            // Dropping/aborting the UI waiter detaches this owner, never its transition.
            tokio::spawn(async move { state.change_binding(Some(request.principal_id)).await })
                .await
                .map_err(|_| error("agent.capability_unavailable", false))?
        },
    )
    .request(
        "agent.binding.clear",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            tokio::spawn(async move { state.change_binding(None).await })
                .await
                .map_err(|_| error("agent.capability_unavailable", false))?
        },
    )
    .request(
        "agent.status.get",
        |context: Context<AgentActive>, _: Empty| async move {
            Ok::<Status, BackendError>(context.state().status.current())
        },
    )
    .request(
        "agent.chat.get",
        |context: Context<AgentActive>, _: Empty| async move {
            Ok::<ChatView, BackendError>(context.state().chat.current())
        },
    )
    .request(
        "agent.chat.send",
        |context: Context<AgentActive>, request: ChatSendRequest| async move {
            let state = context.state().clone();
            let Some(config) = state.chat_config(context.cancellation()).await? else {
                return Err(error(chat::NOT_CONFIGURED, false));
            };
            state.start_chat(&request.content, config).await
        },
    )
    .request(
        "agent.chat.clear",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            let ((), view) = state.chat.update(chat::ChatSession::clear);
            state.stop_chat().await;
            Ok::<ChatView, BackendError>(view)
        },
    )
    .request(
        "agent.packages.get",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            let availability = state.availability().await;
            context
                .blocking(move |_| state.packages_view(&availability))
                .await
        },
    )
    .request(
        "agent.packages.set",
        |context: Context<AgentActive>, request: PackagesSetRequest| async move {
            let state = context.state().clone();
            if !access::is_agent_facing(state.catalog, &request.contract) {
                return Err(error(access::UNKNOWN_PACKAGE, false));
            }
            {
                let state = state.clone();
                context
                    .blocking(move |cancel| {
                        if cancel.is_cancelled() {
                            return Err(storage_failed());
                        }
                        state
                            .packages
                            .lock()
                            .map_err(|_| storage_failed())?
                            .set(&request.contract, request.enabled)
                            .map_err(|_| storage_failed())
                    })
                    .await?;
            }
            let availability = state.availability().await;
            context
                .blocking(move |_| state.packages_view(&availability))
                .await
        },
    )
    .subscription(
        "agent.status.changes",
        |context: Context<AgentActive>, _: Empty| async move {
            Ok::<EventSubscription, BackendError>(watch_subscription(
                context.state().status.subscribe(),
            ))
        },
    )
    .subscription(
        "agent.chat.changes",
        |context: Context<AgentActive>, _: Empty| async move {
            Ok::<EventSubscription, BackendError>(watch_subscription(
                context.state().chat.subscribe(),
            ))
        },
    )
}
