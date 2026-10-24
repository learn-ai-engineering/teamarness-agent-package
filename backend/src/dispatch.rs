//! Process-owned post ownership, independent of cancellable harness/request tasks.
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use teamarness_package_sdk::{BackendError, CancellationToken};
use tokio::task::JoinHandle;

#[derive(Default)]
struct State {
    open: bool,
    pending: usize,
    uncertain: bool,
    binding_uncertain: bool,
}

#[derive(Default)]
pub(crate) struct DispatchFence {
    state: Mutex<State>,
}

impl DispatchFence {
    pub(crate) fn close(&self) {
        self.state.lock().expect("dispatch fence poisoned").open = false;
    }

    pub(crate) fn open(&self) -> Result<(), BackendError> {
        let mut state = self.state.lock().expect("dispatch fence poisoned");
        if state.uncertain || state.binding_uncertain {
            return Err(crate::error(crate::harness::POST_FAILED, false));
        }
        state.open = true;
        Ok(())
    }

    fn binding_error(state: &State) -> Result<(), BackendError> {
        if state.pending != 0 || state.uncertain || state.binding_uncertain {
            return Err(crate::error(
                crate::harness::POST_FAILED,
                !state.uncertain && !state.binding_uncertain,
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn before_binding(&self) -> Result<(), BackendError> {
        Self::binding_error(&self.state.lock().expect("dispatch fence poisoned"))
    }

    /// An ambiguous binding result cannot be reconciled by a later read: the old write may
    /// still run after that read. Only a replacement Host generation can fence that work.
    pub(crate) fn begin_binding(&self) -> Result<(), BackendError> {
        let mut state = self.state.lock().expect("dispatch fence poisoned");
        Self::binding_error(&state)?;
        state.binding_uncertain = true;
        state.open = false;
        Ok(())
    }

    pub(crate) fn binding_resolved(&self) {
        self.state
            .lock()
            .expect("dispatch fence poisoned")
            .binding_uncertain = false;
    }

    pub(crate) fn start<F>(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
        post: F,
    ) -> Option<JoinHandle<Option<Result<Value, BackendError>>>>
    where
        F: Future<Output = Result<Value, BackendError>> + Send + 'static,
    {
        let mut state = self.state.lock().expect("dispatch fence poisoned");
        if !state.open || cancellation.is_cancelled() {
            return None;
        }
        state.pending += 1;
        let lease = PostLease {
            fence: self.clone(),
            settled: false,
        };
        let cancellation = cancellation.clone();
        // No await between registration and transferring ownership. Dropping the reply's
        // JoinHandle detaches this owner; it does not release an enqueued command's lease.
        Some(tokio::spawn(async move {
            let mut lease = lease;
            let open = lease
                .fence
                .state
                .lock()
                .expect("dispatch fence poisoned")
                .open;
            if !open || cancellation.is_cancelled() {
                lease.settle(true); // The Port future has never been polled; nothing can be enqueued.
                return None;
            }
            // From the first poll onward the SDK may enqueue; cancellation cannot prove otherwise.
            let result = post.await;
            lease.settle(result.is_ok());
            Some(result)
        }))
    }
}

struct PostLease {
    fence: Arc<DispatchFence>,
    settled: bool,
}

impl PostLease {
    fn settle(&mut self, safe: bool) {
        let mut state = self.fence.state.lock().expect("dispatch fence poisoned");
        state.pending -= 1;
        // No arbitrary error code proves remote quiescence. Even SDK/Host timeouts can
        // leave detached work alive; fail closed on every non-success terminal or abort.
        state.uncertain |= !safe;
        if !safe {
            state.open = false;
        }
        self.settled = true;
    }
}

impl Drop for PostLease {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn reply_abort_does_not_release_enqueued_post() {
        let fence = Arc::new(DispatchFence::default());
        fence.open().unwrap();
        let release = Arc::new(Notify::new());
        let post_release = release.clone();
        let entered = Arc::new(Notify::new());
        let post_entered = entered.clone();
        let post = fence
            .start(&CancellationToken::new(), async move {
                post_entered.notify_one();
                post_release.notified().await;
                Ok(serde_json::json!({}))
            })
            .unwrap();
        entered.notified().await;
        let waiter = tokio::spawn(post);
        waiter.abort();
        let _ = waiter.await;
        fence.close();
        assert!(
            fence.before_binding().is_err(),
            "local abort is not remote quiescence"
        );
        release.notify_one();
        for _ in 0..100 {
            if fence.before_binding().is_ok() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("terminal success must release ownership");
    }

    #[tokio::test]
    async fn unknown_error_and_owner_abort_latch_across_reopen() {
        for abort in [false, true] {
            let fence = Arc::new(DispatchFence::default());
            fence.open().unwrap();
            let task = fence
                .start(&CancellationToken::new(), async move {
                    if abort {
                        std::future::pending::<()>().await;
                    }
                    Err(
                        BackendError::new("outcome_unknown", false, serde_json::json!({}))
                            .with_outcome_uncertain(true),
                    )
                })
                .unwrap();
            if abort {
                task.abort();
            }
            let _ = task.await;
            fence.close();
            assert!(fence.open().is_err());
            assert!(fence.before_binding().is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_before_owner_runs_never_polls_the_port_future() {
        let fence = Arc::new(DispatchFence::default());
        fence.open().unwrap();
        let cancellation = CancellationToken::new();
        let task = fence
            .start(&cancellation, async {
                panic!("cancelled waiting post dispatched")
            })
            .unwrap();
        cancellation.cancel();
        assert!(task.await.unwrap().is_none());
        assert!(fence.before_binding().is_ok());
        fence.open().unwrap();
    }

    #[tokio::test]
    async fn closed_or_cancelled_work_never_dispatches() {
        let fence = Arc::new(DispatchFence::default());
        assert!(
            fence
                .start(&CancellationToken::new(), async {
                    panic!("closed dispatch")
                })
                .is_none()
        );
        fence.open().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            fence
                .start(&cancel, async { panic!("cancelled dispatch") })
                .is_none()
        );
        assert!(fence.before_binding().is_ok());
    }

    #[test]
    fn ambiguous_binding_never_reopens_on_lifecycle_reset() {
        let fence = DispatchFence::default();
        fence.begin_binding().unwrap();
        for _ in 0..3 {
            fence.close();
            assert!(fence.open().is_err());
            assert!(fence.before_binding().is_err());
        }
    }
}
