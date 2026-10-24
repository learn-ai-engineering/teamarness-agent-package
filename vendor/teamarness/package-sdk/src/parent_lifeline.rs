use tokio::task::JoinHandle;

/// Aborts child work when the host closes stdin or the dispatcher stops.
pub(crate) fn abort_all(tasks: impl IntoIterator<Item = JoinHandle<()>>) {
    for task in tasks {
        task.abort();
    }
}
