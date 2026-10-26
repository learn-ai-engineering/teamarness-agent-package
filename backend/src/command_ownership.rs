//! Owns actual commands, not their cancellable observers. Binding admission and command
//! admission share one lock; an intent cannot cross a later identity revision.
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use teamarness_package_sdk::BackendError;
use tokio::sync::oneshot;

use crate::tools::PortCaller;

/// Across all turns, including abandoned turns. Overflow refuses before Host enqueue.
pub const MAX_PENDING_COMMANDS: usize = 16;
pub const BINDING_BUSY: &str = "agent.binding_busy";
pub const BINDING_UNCERTAIN: &str = "agent.binding_uncertain";
pub const STALE_INTENT: &str = "agent.stale_intent";
pub const COMMAND_BUSY: &str = "agent.command_busy";

fn refusal(code: &str) -> BackendError {
    BackendError::new(
        code,
        code == BINDING_BUSY || code == COMMAND_BUSY,
        json!({}),
    )
}

/// The SDK marks invoke-command timeout/disconnect uncertain, but binding calls are not
/// invoke commands. Opaque transport/decoding failures cannot establish quiescence even
/// when their outcomeUncertain flag is false. Unknown non-domain errors fail closed.
fn certain(error: &BackendError) -> bool {
    if error.outcome_uncertain() {
        return false;
    }
    let code = error.code();
    (code.contains('.') && !code.starts_with("package.") && !code.starts_with("agent."))
        || matches!(
            code,
            "agent_not_bound"
                | "agent_not_stewarded"
                | "agent_not_found"
                | "package.invalid_capability_request"
                | "capability_request_invalid"
                | "ungranted_port"
                | "port_not_enabled"
                | "port_unavailable"
                | "port_catalog_changed"
                | "invalid_request"
                | "agent_binding_changed"
                | "port_version_mismatch"
                | "port_request_invalid"
                | "capability_version_unsupported"
                | "overloaded"
        )
}

#[derive(Default)]
struct State {
    revision: u64,
    pending: usize,
    changing: bool,
    command_uncertain: bool,
    binding_uncertain: bool,
}

#[derive(Clone, Default)]
pub struct IdentityWork(Arc<Mutex<State>>);

impl IdentityWork {
    /// Captured before a turn starts, never at its final command dispatch.
    pub fn intent<P>(&self, ports: P) -> IntentPorts<P> {
        let revision = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision;
        IntentPorts {
            identity: self.clone(),
            revision,
            ports,
        }
    }

    pub fn identity_ready(&self) -> bool {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.changing && !state.binding_uncertain
    }

    /// Invalidates old turns on an independently observed roster/binding change. This is
    /// never reconciliation of an ambiguous mutation and never clears uncertainty.
    pub fn invalidate(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.revision = state
            .revision
            .checked_add(1)
            .expect("identity revision exhausted");
    }

    /// Admission happens before spawning and before waiting for unrelated UI transitions.
    /// At most one actual mutation owner exists, even if every UI waiter disappears.
    pub async fn change_binding<T, F, Fut>(&self, work: F) -> Result<T, BackendError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, BackendError>> + Send + 'static,
    {
        let owner = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.binding_uncertain || state.command_uncertain {
                return Err(refusal(BINDING_UNCERTAIN));
            }
            if state.changing || state.pending != 0 {
                return Err(refusal(BINDING_BUSY));
            }
            state.changing = true;
            state.revision = state
                .revision
                .checked_add(1)
                .expect("identity revision exhausted");
            Owner {
                identity: self.clone(),
                binding: true,
                settled: false,
            }
        };
        observe(owner, work).await
    }
}

/// The external PortCaller adapter used by both model tools and the final reply post.
#[derive(Clone)]
pub struct IntentPorts<P> {
    identity: IdentityWork,
    revision: u64,
    ports: P,
}

impl<P> IntentPorts<P> {
    /// Background participation may wait, but must not read/subscribe from stale identity state.
    pub fn is_current(&self) -> bool {
        let state = self
            .identity
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check(&state).is_ok()
    }

    fn check(&self, state: &State) -> Result<(), BackendError> {
        if state.binding_uncertain {
            return Err(refusal(BINDING_UNCERTAIN));
        }
        if state.changing {
            return Err(refusal(BINDING_BUSY));
        }
        if self.revision != state.revision {
            return Err(refusal(STALE_INTENT));
        }
        Ok(())
    }
}

impl<P: PortCaller + Clone + 'static> PortCaller for IntentPorts<P> {
    async fn query(
        &self,
        contract: &str,
        major: u16,
        port: &str,
        request: Value,
    ) -> Result<Value, BackendError> {
        {
            let state = self
                .identity
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.check(&state)?;
        }
        self.ports.query(contract, major, port, request).await
    }

    async fn command(
        &self,
        contract: &str,
        major: u16,
        port: &str,
        request: Value,
    ) -> Result<Value, BackendError> {
        let owner = {
            let mut state = self
                .identity
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.check(&state)?;
            if state.pending >= MAX_PENDING_COMMANDS {
                return Err(refusal(COMMAND_BUSY));
            }
            state.pending += 1;
            Owner {
                identity: self.identity.clone(),
                binding: false,
                settled: false,
            }
        };
        let ports = self.ports.clone();
        let contract = contract.to_owned();
        let port = port.to_owned();
        observe(owner, move || async move {
            ports.command(&contract, major, &port, request).await
        })
        .await
    }
}

struct Owner {
    identity: IdentityWork,
    binding: bool,
    settled: bool,
}

impl Owner {
    fn settle<T>(&mut self, result: &Result<T, BackendError>) {
        self.settled = result.as_ref().err().is_none_or(certain);
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let mut state = self
            .identity
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.binding {
            state.changing = false;
            state.binding_uncertain |= !self.settled;
        } else {
            state.pending -= 1;
            state.command_uncertain |= !self.settled;
        }
    }
}

async fn observe<T, F, Fut>(mut owner: Owner, work: F) -> Result<T, BackendError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, BackendError>> + Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let result = work().await.map_err(|error| {
            if certain(&error) {
                error
            } else {
                error.with_outcome_uncertain(true)
            }
        });
        owner.settle(&result);
        drop(owner); // Publish cleanup before the observer receives its terminal.
        let _ = sender.send(result);
    });
    receiver
        .await
        .unwrap_or_else(|_| Err(refusal(BINDING_UNCERTAIN).with_outcome_uncertain(true)))
}
