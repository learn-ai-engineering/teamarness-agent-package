//! Running blocking Package work without holding an async worker.

use teamarness_package_protocol::JsonValue;

use crate::cancellation::CancellationToken;
use crate::{BackendError, object};

/// Runs `work` on the blocking pool, racing it against cancellation.
///
/// On cancellation the call returns immediately with `cancelled`; the closure keeps its own token
/// and is expected to observe it, so long-running work stops of its own accord. A panic inside the
/// closure becomes `package.blocking_task_failed`.
///
/// # Errors
///
/// Returns the closure's error, `cancelled` when the token fires first, or
/// `package.blocking_task_failed` when the closure panics.
pub async fn blocking<T, F>(cancellation: &CancellationToken, work: F) -> Result<T, BackendError>
where
    F: FnOnce(&CancellationToken) -> Result<T, BackendError> + Send + 'static,
    T: Send + 'static,
{
    let token = cancellation.clone();
    let handle = tokio::task::spawn_blocking(move || work(&token));
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(cancelled()),
        joined = handle => match joined {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(cancelled()),
            Err(_) => Err(BackendError::new(
                "package.blocking_task_failed",
                false,
                object([]),
            )),
        },
    }
}

fn cancelled() -> BackendError {
    BackendError::new("cancelled", true, JsonValue::Object(Default::default()))
}
