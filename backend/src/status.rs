//! The Package-owned status the UI shows: one state machine, one watch channel.
//!
//! `unconfigured` and `no_agent` are derived from configuration and binding; `ready`, `replying`
//! and `error` are the harness's activity once both are present. A change to configuration or
//! binding resets a stale `error`, so the Steward can recover by fixing what was wrong.

use std::sync::Mutex;

use serde::Serialize;
use teamarness_package_sdk::capabilities::agent_binding::BoundAgent;
use tokio::sync::watch;

/// The closed vocabulary in the UI contract's `status.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Unconfigured,
    NoAgent,
    Ready,
    Replying,
    Error,
}

/// The bound Agent as the UI sees it. Its signing key is kept beside, not shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSummary {
    pub principal_id: String,
    pub name: Option<String>,
}

impl From<&BoundAgent> for AgentSummary {
    fn from(agent: &BoundAgent) -> Self {
        Self {
            principal_id: agent.principal_id.clone(),
            name: agent.name.clone(),
        }
    }
}

/// The last Package-owned failure the harness reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusError {
    pub code: String,
    pub retryable: bool,
}

/// The exact UI-contract `status` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: StatusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<StatusError>,
}

/// What the harness is doing while the Package is otherwise ready.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Activity {
    Idle,
    Replying,
    Error(StatusError),
}

#[derive(Debug)]
struct Inner {
    configured: bool,
    agent: Option<BoundAgent>,
    activity: Activity,
}

impl Inner {
    fn status(&self) -> Status {
        let state = if !self.configured {
            StatusState::Unconfigured
        } else if self.agent.is_none() {
            StatusState::NoAgent
        } else {
            match &self.activity {
                Activity::Idle => StatusState::Ready,
                Activity::Replying => StatusState::Replying,
                Activity::Error(_) => StatusState::Error,
            }
        };
        let last_error = match (&state, &self.activity) {
            (StatusState::Error, Activity::Error(error)) => Some(error.clone()),
            _ => None,
        };
        Status {
            state,
            agent: self.agent.as_ref().map(AgentSummary::from),
            last_error,
        }
    }
}

/// The status of one activation, observable through a watch channel.
#[derive(Debug)]
pub struct StatusCell {
    inner: Mutex<Inner>,
    sender: watch::Sender<Status>,
}

impl StatusCell {
    #[must_use]
    pub fn new(configured: bool, agent: Option<BoundAgent>) -> Self {
        let inner = Inner {
            configured,
            agent,
            activity: Activity::Idle,
        };
        let (sender, _) = watch::channel(inner.status());
        Self {
            inner: Mutex::new(inner),
            sender,
        }
    }

    /// The status right now.
    #[must_use]
    pub fn current(&self) -> Status {
        self.lock().status()
    }

    /// The bound Agent, including its signing key, for the harness.
    #[must_use]
    pub fn agent(&self) -> Option<BoundAgent> {
        self.lock().agent.clone()
    }

    /// Whether configuration and binding are both present, whatever the harness is doing.
    #[must_use]
    pub fn is_ready_to_run(&self) -> bool {
        let inner = self.lock();
        inner.configured && inner.agent.is_some()
    }

    /// A receiver that already holds the current status and wakes on every change.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.sender.subscribe()
    }

    /// Records whether endpoint, model and key are all present.
    pub fn set_configured(&self, configured: bool) {
        self.update(|inner| {
            if inner.configured != configured {
                inner.configured = configured;
                inner.activity = Activity::Idle;
            }
        });
    }

    /// Records what `teamarness.agent-binding` currently answers.
    pub fn set_agent(&self, agent: Option<BoundAgent>) {
        self.update(|inner| {
            if inner.agent != agent {
                inner.agent = agent;
                inner.activity = Activity::Idle;
            }
        });
    }

    /// The harness has a reply in flight.
    pub fn set_replying(&self) {
        self.update(|inner| inner.activity = Activity::Replying);
    }

    /// The harness finished without a failure.
    pub fn set_idle(&self) {
        self.update(|inner| inner.activity = Activity::Idle);
    }

    /// A reply was cancelled: back to idle, but a recorded error is kept.
    pub fn set_idle_if_replying(&self) {
        self.update(|inner| {
            if inner.activity == Activity::Replying {
                inner.activity = Activity::Idle;
            }
        });
    }

    /// The harness failed with a Package-owned code. The code is never a secret.
    pub fn set_error(&self, code: &str, retryable: bool) {
        let error = StatusError {
            code: code.to_owned(),
            retryable,
        };
        self.update(|inner| inner.activity = Activity::Error(error));
    }

    fn update(&self, change: impl FnOnce(&mut Inner)) {
        let status = {
            let mut inner = self.lock();
            change(&mut inner);
            inner.status()
        };
        self.sender.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status;
                true
            }
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> BoundAgent {
        BoundAgent {
            principal_id: "a".repeat(64),
            identity: "b".repeat(64),
            name: Some(name.into()),
        }
    }

    #[test]
    fn derived_states_take_precedence_over_activity() {
        let cell = StatusCell::new(false, None);
        assert_eq!(cell.current().state, StatusState::Unconfigured);
        cell.set_agent(Some(agent("Scout")));
        assert_eq!(cell.current().state, StatusState::Unconfigured);
        assert_eq!(cell.current().agent.unwrap().name.as_deref(), Some("Scout"));
        cell.set_configured(true);
        assert_eq!(cell.current().state, StatusState::Ready);
        cell.set_replying();
        assert_eq!(cell.current().state, StatusState::Replying);
        cell.set_error("agent.endpoint_rejected", false);
        let status = cell.current();
        assert_eq!(status.state, StatusState::Error);
        assert_eq!(
            status.last_error,
            Some(StatusError {
                code: "agent.endpoint_rejected".into(),
                retryable: false
            })
        );
        // The same binding read again does not clear the error; a change does.
        cell.set_agent(Some(agent("Scout")));
        assert_eq!(cell.current().state, StatusState::Error);
        cell.set_agent(Some(agent("Pathfinder")));
        assert_eq!(cell.current().state, StatusState::Ready);
        cell.set_error("agent.model_error", false);
        cell.set_idle_if_replying();
        assert_eq!(cell.current().state, StatusState::Error);
        cell.set_replying();
        cell.set_idle_if_replying();
        assert_eq!(cell.current().state, StatusState::Ready);
        cell.set_error("agent.model_error", false);
        cell.set_agent(None);
        let status = cell.current();
        assert_eq!(status.state, StatusState::NoAgent);
        assert_eq!(status.last_error, None);
        cell.set_configured(false);
        assert_eq!(cell.current().state, StatusState::Unconfigured);
    }

    #[test]
    fn watchers_only_wake_on_a_change() {
        let cell = StatusCell::new(true, Some(agent("Scout")));
        let mut receiver = cell.subscribe();
        assert!(!receiver.has_changed().unwrap());
        cell.set_configured(true);
        assert!(!receiver.has_changed().unwrap());
        cell.set_idle();
        assert!(!receiver.has_changed().unwrap());
        cell.set_replying();
        assert!(receiver.has_changed().unwrap());
        assert_eq!(receiver.borrow_and_update().state, StatusState::Replying);
        let json = serde_json::to_value(cell.current()).unwrap();
        assert_eq!(json["state"], "replying");
        assert!(json.get("lastError").is_none());
        assert!(json["agent"].get("identity").is_none());
    }
}
