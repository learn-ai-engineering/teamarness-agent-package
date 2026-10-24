#![forbid(unsafe_code)]

//! The Agent Package Backend: Package-owned settings and key, the Agent Binding held by Core and
//! read through `teamarness.agent-binding`, the status its UI shows, and the reply harness
//! ([`harness`]) with its model client ([`model`]) that runs while the status is in the ready
//! family.

pub mod harness;
pub mod mention;
pub mod model;
pub mod progress;
pub mod settings;
pub mod status;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;
use teamarness_package_sdk::capabilities::agent_binding::{AgentBinding, AgentCandidate};
use teamarness_package_sdk::capabilities::principals::Principals;
use teamarness_package_sdk::ports::PortClient;
use teamarness_package_sdk::{
    BackendError, CancellationToken, Context, Empty, Router, event_channel,
};

use harness::{HarnessInputs, TaskHandle};
use progress::Progress;
use settings::{KeyError, Settings, SettingsStore};
use status::{AgentSummary, Status, StatusCell};

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
    /// The reply harness; runs while configuration and binding are both present.
    harness: Option<TaskHandle>,
}

/// Everything one activation of the Agent Package owns.
pub struct AgentActive {
    settings: Arc<Mutex<SettingsStore>>,
    status: Arc<StatusCell>,
    binding: AgentBinding,
    principals: Principals,
    ports: Option<PortClient>,
    progress: Arc<Mutex<Progress>>,
    cancellation: CancellationToken,
    tasks: tokio::sync::Mutex<Tasks>,
}

impl AgentActive {
    /// Re-reads whether endpoint, model and key are present.
    fn refresh_configured(&self) {
        let configured = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_configured();
        self.status.set_configured(configured);
    }

    /// Starts the roster watcher for this activation, replacing any earlier one.
    async fn start_roster_watch(&self) {
        let handle = spawn_roster_watch(
            &self.cancellation,
            self.principals.clone(),
            self.binding.clone(),
            self.status.clone(),
        );
        let mut tasks = self.tasks.lock().await;
        if let Some(previous) = tasks.roster.replace(handle) {
            previous.stop().await;
        }
    }

    /// The inputs a harness task needs, or `None` when the Package has no Port grant.
    fn harness_inputs(&self) -> Option<HarnessInputs> {
        Some(HarnessInputs {
            ports: self.ports.clone()?,
            principals: self.principals.clone(),
            settings: self.settings.clone(),
            status: self.status.clone(),
            progress: self.progress.clone(),
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
        if self.status.is_ready_to_run() {
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
        let (roster, harness) = {
            let mut tasks = self.tasks.lock().await;
            (tasks.roster.take(), tasks.harness.take())
        };
        if let Some(task) = harness {
            task.stop().await;
        }
        if let Some(task) = roster {
            task.stop().await;
        }
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

/// Spawns the roster watcher under `parent`.
fn spawn_roster_watch(
    parent: &CancellationToken,
    principals: Principals,
    binding: AgentBinding,
    status: Arc<StatusCell>,
) -> TaskHandle {
    TaskHandle::spawn(parent, move |cancellation| async move {
        watch_roster(&principals, &binding, &status, &cancellation).await;
    })
}

/// Re-reads the binding from the capability and publishes it. Nothing about the Agent is stored
/// locally; the Host's answer is the truth.
async fn refresh_binding(binding: &AgentBinding, status: &StatusCell) -> Result<(), BackendError> {
    let agent = binding
        .current()
        .await
        .map_err(|cause| capability_error(&cause))?;
    status.set_agent(agent);
    Ok(())
}

/// Follows the Principal roster. Each snapshot is a trigger to re-read the binding, whose answer
/// is the truth: the Host reports no Agent once it left the registry, and the new name after a
/// rename.
async fn watch_roster(
    principals: &Principals,
    binding: &AgentBinding,
    status: &StatusCell,
    cancellation: &CancellationToken,
) {
    const RETRY: std::time::Duration = std::time::Duration::from_secs(1);
    loop {
        let cursor = match principals.current().await {
            Ok(snapshot) => snapshot.cursor,
            Err(_) => {
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    () = tokio::time::sleep(RETRY) => continue,
                }
            }
        };
        let Ok(mut stream) = principals.subscribe(cursor).await else {
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = tokio::time::sleep(RETRY) => continue,
            }
        };
        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                () = cancellation.cancelled() => {
                    stream.close().await;
                    return;
                }
            };
            match next {
                Some(Ok(_)) => {
                    let _ = refresh_binding(binding, status).await;
                }
                Some(Err(_)) | None => break,
            }
        }
        stream.close().await;
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
struct BindingSetRequest {
    principal_id: String,
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

/// The Agent Backend.
#[must_use]
pub fn router() -> Router<AgentActive> {
    Router::new(|activation| {
        let initialization = activation.initialization().clone();
        let capabilities = activation.capabilities().clone();
        let cancellation = activation.cancellation().clone();
        async move {
            let directory = initialization.data_directory().path().to_owned();
            let (store, progress) = teamarness_package_sdk::blocking(&cancellation, move |_| {
                Ok((
                    SettingsStore::open(&directory).map_err(|_| storage_failed())?,
                    Progress::open(&directory).map_err(|_| storage_failed())?,
                ))
            })
            .await?;
            let binding = AgentBinding::from_initialization(&initialization, capabilities.clone())
                .map_err(|cause| capability_error(&cause))?;
            let principals = Principals::from_initialization(&initialization, capabilities.clone())
                .map_err(|cause| capability_error(&cause))?;
            let ports = initialization
                .capability("teamarness.package-ports")
                .map(|grant| PortClient::new(capabilities.clone(), grant.grant.clone()));
            let configured = store.is_configured();
            let agent = binding
                .current()
                .await
                .map_err(|cause| capability_error(&cause))?;
            let status = Arc::new(StatusCell::new(configured, agent));
            // Background tasks hang off a token the activation's own request cannot cancel
            // once it has answered; suspend and dispose stop them explicitly.
            let background = CancellationToken::new();
            let roster = spawn_roster_watch(
                &background,
                principals.clone(),
                binding.clone(),
                status.clone(),
            );
            let state = AgentActive {
                settings: Arc::new(Mutex::new(store)),
                status,
                binding,
                principals,
                ports,
                progress: Arc::new(Mutex::new(progress)),
                cancellation: background,
                tasks: tokio::sync::Mutex::new(Tasks {
                    roster: Some(roster),
                    harness: None,
                }),
            };
            // A configured and bound Package resumes replying as soon as it activates.
            state.sync_harness().await;
            Ok(state)
        }
    })
    .on_resume(|state: Arc<AgentActive>| async move {
        state.refresh_configured();
        let _ = refresh_binding(&state.binding, &state.status).await;
        state.start_roster_watch().await;
        state.sync_harness().await;
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
            let view = {
                let state = state.clone();
                context
                    .blocking(move |_| {
                        state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .replace(settings)
                            .map_err(|_| storage_failed())?;
                        state.refresh_configured();
                        Ok(state.settings_view())
                    })
                    .await?
            };
            state.sync_harness().await;
            Ok(view)
        },
    )
    .request(
        "agent.key.set",
        |context: Context<AgentActive>, request: KeySetRequest| async move {
            let state = context.state().clone();
            let view = {
                let state = state.clone();
                context
                    .blocking(move |_| {
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
                        state.refresh_configured();
                        Ok(KeyView {
                            has_key: state.settings_view().has_key,
                        })
                    })
                    .await?
            };
            state.sync_harness().await;
            Ok(view)
        },
    )
    .request(
        "agent.key.clear",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            let view = {
                let state = state.clone();
                context
                    .blocking(move |_| {
                        state
                            .settings
                            .lock()
                            .map_err(|_| storage_failed())?
                            .clear_key()
                            .map_err(|_| storage_failed())?;
                        state.refresh_configured();
                        Ok(KeyView {
                            has_key: state.settings_view().has_key,
                        })
                    })
                    .await?
            };
            state.sync_harness().await;
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
            let agent = state
                .binding
                .bind(&request.principal_id)
                .await
                .map_err(|cause| capability_error(&cause))?;
            let summary = AgentSummary::from(&agent);
            state.status.set_agent(Some(agent));
            state.sync_harness().await;
            Ok(BindingView {
                agent: Some(summary),
            })
        },
    )
    .request(
        "agent.binding.clear",
        |context: Context<AgentActive>, _: Empty| async move {
            let state = context.state().clone();
            state
                .binding
                .unbind()
                .await
                .map_err(|cause| capability_error(&cause))?;
            state.status.set_agent(None);
            state.sync_harness().await;
            Ok(BindingView { agent: None })
        },
    )
    .request(
        "agent.status.get",
        |context: Context<AgentActive>, _: Empty| async move {
            Ok::<Status, BackendError>(context.state().status.current())
        },
    )
    .subscription(
        "agent.status.changes",
        |context: Context<AgentActive>, _: Empty| async move {
            let (sender, subscription) = event_channel(4);
            let mut receiver = context.state().status.subscribe();
            tokio::spawn(async move {
                loop {
                    let status = receiver.borrow_and_update().clone();
                    let Ok(payload) = serde_json::to_value(status) else {
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
            Ok(subscription)
        },
    )
}
