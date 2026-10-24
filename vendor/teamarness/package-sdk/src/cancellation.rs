//! Cooperative cancellation for Package work.
//!
//! The SDK ships its own token rather than depending on `tokio-util`, so the dependency set stays
//! small and exactly pinned, and so a token can also expose a plain `Arc<AtomicBool>` for
//! libraries such as `gix` that poll an interrupt flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

struct Shared {
    /// The flag is an `Arc` so `interrupt_flag` can hand the same cell to a polling library.
    cancelled: Arc<AtomicBool>,
    notify: Notify,
    children: Mutex<Vec<Weak<Shared>>>,
}

/// A cancellation signal shared by a request, its children, and any blocking work it starts.
#[derive(Clone)]
pub struct CancellationToken {
    shared: Arc<Shared>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancellationToken {
    /// Creates an uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                cancelled: Arc::new(AtomicBool::new(false)),
                notify: Notify::new(),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Cancels this token and every child token derived from it.
    pub fn cancel(&self) {
        cancel_shared(&self.shared);
    }

    /// Reports whether this token has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::SeqCst)
    }

    /// Completes as soon as the token is cancelled, whether before or after the call.
    pub async fn cancelled(&self) {
        loop {
            // Register interest before re-checking, so a cancel between the two is not missed.
            let notified = self.shared.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
            if self.is_cancelled() {
                return;
            }
        }
    }

    /// A flag that follows this token, for libraries that poll rather than await.
    ///
    /// It is the token's own storage, so a poller and an awaiter always agree.
    #[must_use]
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        self.shared.cancelled.clone()
    }

    /// A token that is cancelled when this one is, and can also be cancelled on its own.
    #[must_use]
    pub fn child(&self) -> Self {
        let child = Self::new();
        if self.is_cancelled() {
            child.cancel();
            return child;
        }
        let mut children = self
            .shared
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        children.retain(|entry| entry.strong_count() > 0);
        children.push(Arc::downgrade(&child.shared));
        drop(children);
        // A cancel racing the registration above still reaches the child.
        if self.is_cancelled() {
            child.cancel();
        }
        child
    }
}

fn cancel_shared(shared: &Arc<Shared>) {
    if shared.cancelled.swap(true, Ordering::SeqCst) {
        return;
    }
    shared.notify.notify_waiters();
    let children = {
        let mut guard = shared
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    };
    for child in children {
        if let Some(child) = child.upgrade() {
            cancel_shared(&child);
        }
    }
}
