//! Reply to addressed Messages using durable projection revisions, not display positions.
//!
//! `messages.changed` only wakes catch-up. Startup, reconnection and locally pending work use
//! `message.changes` after the acknowledged revision. Each Channel has one reply in flight;
//! its trigger is persisted before the model/post attempt and is never retried afterward.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use teamarness_package_sdk::CancellationToken;
use teamarness_package_sdk::capabilities::agent_binding::BoundAgent;
use teamarness_package_sdk::capabilities::principals::{Principal, PrincipalKind, Principals};
use teamarness_package_sdk::ports::PortClient;
use tokio::task::{JoinHandle, JoinSet};

use crate::mention::mentions;
use crate::model::{ChatMessage, ModelClient, ModelFailure};
use crate::progress::{Cursor, Progress};
use crate::settings::SettingsStore;
use crate::status::StatusCell;

pub const CONVERSATIONS_CONTRACT: &str = "dev.teamarness.conversations";
pub const POST_FAILED: &str = "agent.post_failed";
pub const STORAGE_FAILED: &str = "agent.storage_failed";
/// Saved progress cannot be continued safely; never silently replay or rebaseline it.
pub const PROGRESS_INVALID: &str = "agent.progress_invalid";

/// Published `message.changes` event bound and finite work per Channel per pass.
const CHANGE_LIMIT: usize = 32;
const MAX_BATCHES_PER_CHANNEL: usize = 4;
const MAX_REPLY_CHARS: usize = 32_768;
const RETRY: Duration = Duration::from_secs(1);
const DRAIN: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct HarnessInputs {
    pub ports: PortClient,
    pub principals: Principals,
    pub settings: Arc<Mutex<SettingsStore>>,
    pub status: Arc<StatusCell>,
    /// Shared across suspend/resume so blocking writes have one serialized owner.
    pub progress: Arc<Mutex<Progress>>,
}

pub struct TaskHandle {
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl TaskHandle {
    pub fn spawn<F, Fut>(parent: &CancellationToken, work: F) -> Self
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let cancellation = parent.child();
        let task = tokio::spawn(work(cancellation.clone()));
        Self { cancellation, task }
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn stop(self) {
        self.cancellation.cancel();
        let abort = self.task.abort_handle();
        if tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .is_err()
        {
            abort.abort();
        }
    }
}

pub fn start(parent: &CancellationToken, inputs: HarnessInputs) -> TaskHandle {
    TaskHandle::spawn(parent, move |cancellation| run(inputs, cancellation))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Message {
    id: String,
    position: String,
    author: String,
    markdown: String,
    #[serde(rename = "isLocal")]
    _is_local: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Change {
    revision: String,
    id: String,
    position: String,
    kind: String,
    status: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Batch {
    events: Vec<Change>,
    continuation: String,
    high_water: String,
    #[serde(default)]
    resync_required: bool,
}

/// Validate continuity before allowing any response to acknowledge unseen work.
impl Batch {
    fn parse(value: Value, after: u64) -> Option<Self> {
        let batch: Self = serde_json::from_value(value).ok()?;
        let continuation = revision(&batch.continuation)?;
        let high_water = revision(&batch.high_water)?;
        if batch.resync_required
            || batch.events.len() > CHANGE_LIMIT
            || continuation < after
            || continuation > high_water
            || (high_water > after && continuation == after)
        {
            return None;
        }
        let mut previous = after;
        for event in &batch.events {
            let next = revision(&event.revision)?;
            if next <= previous
                || next > continuation
                || event.id.is_empty()
                || event.position.is_empty()
            {
                return None;
            }
            match (event.kind.as_str(), event.status.as_deref(), &event.message) {
                ("remove", None, None) | ("insert", Some("pending" | "unavailable"), None) => {}
                ("insert", Some("ready"), Some(message))
                    if message.id == event.id && message.position == event.position => {}
                _ => return None,
            }
            previous = next;
        }
        Some(batch)
    }
}

fn revision(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value <= i64::MAX as u64)
}

#[derive(Default)]
struct Slot {
    in_flight: bool,
    pending: bool,
}

enum ReplyOutcome {
    Posted,
    Cancelled,
    Failed(&'static str, bool),
}

struct Loop {
    inputs: HarnessInputs,
    client: ModelClient,
    slots: HashMap<String, Slot>,
    replies: JoinSet<(String, ReplyOutcome)>,
    work: CancellationToken,
    retry_at: Option<tokio::time::Instant>,
    fatal: bool,
}

async fn run(inputs: HarnessInputs, cancellation: CancellationToken) {
    let client = match ModelClient::new() {
        Ok(client) => client,
        Err(failure) => {
            inputs.status.set_error(failure.code, failure.retryable);
            return;
        }
    };
    let mut status = inputs.status.subscribe();
    let mut state = Loop {
        inputs,
        client,
        slots: HashMap::new(),
        replies: JoinSet::new(),
        work: cancellation.child(),
        retry_at: None,
        fatal: false,
    };
    'subscription: loop {
        if cancellation.is_cancelled() || !state.inputs.status.is_ready_to_run() {
            break;
        }
        let subscription = tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            events = state.inputs.ports.subscribe_event(CONVERSATIONS_CONTRACT, "messages.changed") => events,
        };
        let mut events = match subscription {
            Ok(events) => events,
            Err(_) => {
                if wait_or_cancel(&cancellation, RETRY).await {
                    break;
                }
                continue;
            }
        };
        state.pass().await;
        loop {
            if state.fatal || cancellation.is_cancelled() {
                events.close().await;
                break 'subscription;
            }
            let retry_at = state.retry_at;
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    events.close().await;
                    break 'subscription;
                }
                changed = status.changed() => {
                    if changed.is_err() || !state.inputs.status.is_ready_to_run() {
                        events.close().await;
                        break 'subscription;
                    }
                }
                joined = state.replies.join_next(), if !state.replies.is_empty() => {
                    if let Some(joined) = joined { state.finished(joined).await; }
                }
                next = events.recv() => match next {
                    Some(Ok(_)) => state.pass().await,
                    Some(Err(_)) | None => { events.close().await; break; }
                },
                () = tokio::time::sleep_until(retry_at.unwrap_or_else(tokio::time::Instant::now)), if retry_at.is_some() => {
                    state.pass().await;
                }
            }
        }
        if wait_or_cancel(&cancellation, RETRY).await {
            break;
        }
    }
    state.shutdown().await;
}

async fn wait_or_cancel(cancellation: &CancellationToken, duration: Duration) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => true,
        () = tokio::time::sleep(duration) => false,
    }
}

impl Loop {
    fn schedule(&mut self, delay: Duration) {
        let deadline = tokio::time::Instant::now() + delay;
        self.retry_at = Some(
            self.retry_at
                .map_or(deadline, |previous| previous.min(deadline)),
        );
    }

    fn invalid_progress(&mut self) {
        self.fatal = true;
        self.work.cancel();
        self.inputs.status.set_error(PROGRESS_INVALID, false);
    }

    async fn query(&self, port: &str, request: Value) -> Result<Value, ()> {
        tokio::select! {
            biased;
            () = self.work.cancelled() => Err(()),
            response = self.inputs.ports.query(CONVERSATIONS_CONTRACT, port, request) => response.map_err(|_| ()),
        }
    }

    async fn cursor(&self, channel: &str) -> Result<Option<Cursor>, ()> {
        let progress = self.inputs.progress.clone();
        let channel = channel.to_owned();
        teamarness_package_sdk::blocking(&self.work, move |_| {
            progress
                .lock()
                .map_err(|_| crate::storage_failed())
                .map(|progress| progress.cursor(&channel).cloned())
        })
        .await
        .map_err(|_| ())
    }

    async fn persist(&mut self, channel: &str, cursor: &Cursor) -> bool {
        let progress = self.inputs.progress.clone();
        let channel = channel.to_owned();
        let cursor = cursor.clone();
        let saved = teamarness_package_sdk::blocking(&self.work, move |token| {
            if token.is_cancelled() {
                return Err(crate::storage_failed());
            }
            progress
                .lock()
                .map_err(|_| crate::storage_failed())?
                .advance(&channel, cursor)
                .map_err(|_| crate::storage_failed())
        })
        .await;
        if saved.is_err() {
            if !self.work.is_cancelled() {
                self.inputs.status.set_error(STORAGE_FAILED, true);
                self.schedule(RETRY);
            }
            return false;
        }
        true
    }

    async fn pass(&mut self) {
        self.retry_at = None;
        let Some(agent) = self.inputs.status.agent() else {
            return;
        };
        let Ok(list) = self.query("channel.list", json!({})).await else {
            self.schedule(RETRY);
            return;
        };
        let Some(channels) = list["channels"].as_array() else {
            self.invalid_progress();
            return;
        };
        let channels = channels
            .iter()
            .filter_map(|row| row["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let (context_messages, system_prompt) = {
            let store = self
                .inputs
                .settings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                store.settings().context_messages as usize,
                store.settings().system_prompt.clone(),
            )
        };
        let mut lookups = HashMap::new();
        for channel in channels {
            if self.work.is_cancelled() || self.fatal {
                break;
            }
            if self.slots.get(&channel).is_some_and(|slot| slot.in_flight) {
                self.slots.entry(channel).or_default().pending = true;
                continue;
            }
            self.scan(
                &agent,
                &channel,
                context_messages,
                &system_prompt,
                &mut lookups,
            )
            .await;
        }
    }

    async fn scan(
        &mut self,
        agent: &BoundAgent,
        channel: &str,
        context_messages: usize,
        system_prompt: &str,
        lookups: &mut HashMap<String, Option<Principal>>,
    ) {
        let mut cursor = match self.cursor(channel).await {
            Ok(Some(cursor)) => cursor,
            Ok(None) => {
                let Ok(page) = self
                    .query("message.page", json!({"channelId":channel,"limit":1}))
                    .await
                else {
                    self.schedule(RETRY);
                    return;
                };
                let cursor = page["epoch"]
                    .as_str()
                    .zip(page["revision"].as_str().and_then(revision))
                    .and_then(|(epoch, after)| Cursor::new(epoch.to_owned(), after));
                let Some(cursor) = cursor else {
                    self.invalid_progress();
                    return;
                };
                self.persist(channel, &cursor).await;
                return;
            }
            Err(()) => {
                self.inputs.status.set_error(STORAGE_FAILED, true);
                self.schedule(RETRY);
                return;
            }
        };
        for _ in 0..MAX_BATCHES_PER_CHANNEL {
            let Ok(value) = self.query("message.changes", json!({
                "channelId":channel,"epoch":cursor.epoch,"after":cursor.after.to_string(),"limit":CHANGE_LIMIT,
            })).await else { self.schedule(RETRY); return; };
            let Some(batch) = Batch::parse(value, cursor.after) else {
                self.invalid_progress();
                return;
            };
            for event in batch.events {
                if event.status.as_deref() == Some("pending") {
                    if self.persist(channel, &cursor).await {
                        self.schedule(RETRY);
                    }
                    return;
                }
                if let Some(message) = event.message
                    && message.author != agent.identity
                    && agent
                        .name
                        .as_deref()
                        .is_some_and(|name| mentions(&message.markdown, name))
                {
                    let principal = match self.lookup(&message.author, lookups).await {
                        Ok(principal) => principal,
                        Err(()) => {
                            if self.persist(channel, &cursor).await {
                                self.schedule(RETRY);
                            }
                            return;
                        }
                    };
                    if principal.is_some_and(|principal| principal.kind == PrincipalKind::Person) {
                        let messages = match self
                            .context(
                                agent,
                                channel,
                                &message,
                                context_messages,
                                system_prompt,
                                lookups,
                            )
                            .await
                        {
                            Ok(messages) => messages,
                            Err(()) => {
                                if self.persist(channel, &cursor).await {
                                    self.schedule(RETRY);
                                }
                                return;
                            }
                        };
                        cursor.after = revision(&event.revision).expect("validated revision");
                        if self.persist(channel, &cursor).await && !self.work.is_cancelled() {
                            self.spawn_reply(channel.to_owned(), messages);
                        }
                        return;
                    }
                }
                cursor.after = revision(&event.revision).expect("validated revision");
            }
            cursor.after = revision(&batch.continuation).expect("validated continuation");
            if !self.persist(channel, &cursor).await {
                return;
            }
            if cursor.after == revision(&batch.high_water).expect("validated high water") {
                return;
            }
        }
        // The work bound limits this pass, not the history that can ever be processed.
        self.schedule(Duration::ZERO);
    }

    async fn context(
        &self,
        agent: &BoundAgent,
        channel: &str,
        trigger: &Message,
        limit: usize,
        system_prompt: &str,
        lookups: &mut HashMap<String, Option<Principal>>,
    ) -> Result<Vec<ChatMessage>, ()> {
        let mut prior = if limit == 0 {
            Vec::new()
        } else {
            let page = self
                .query(
                    "message.page",
                    json!({"channelId": channel, "before":trigger.position,"limit":limit}),
                )
                .await?;
            let rows = page["messages"].as_array().ok_or(())?;
            rows.iter()
                .map(|row| serde_json::from_value::<Message>(row.clone()).map_err(|_| ()))
                .collect::<Result<Vec<_>, _>>()?
        };
        prior.reverse();
        let mut messages = vec![ChatMessage::system(system_message(agent, system_prompt))];
        for row in prior.iter().chain(std::iter::once(trigger)) {
            if row.author == agent.identity {
                messages.push(ChatMessage::assistant(row.markdown.clone()));
            } else {
                let principal = self.lookup(&row.author, lookups).await?;
                messages.push(ChatMessage::user(format!(
                    "{}: {}",
                    label(principal.as_ref(), &row.author),
                    row.markdown
                )));
            }
        }
        Ok(messages)
    }

    async fn lookup(
        &self,
        author: &str,
        lookups: &mut HashMap<String, Option<Principal>>,
    ) -> Result<Option<Principal>, ()> {
        if let Some(found) = lookups.get(author) {
            return Ok(found.clone());
        }
        let found = tokio::select! {
            biased;
            () = self.work.cancelled() => return Err(()),
            found = self.inputs.principals.lookup(author) => found.map_err(|_| ())?,
        };
        lookups.insert(author.to_owned(), found.clone());
        Ok(found)
    }

    fn spawn_reply(&mut self, channel: String, messages: Vec<ChatMessage>) {
        let slot = self.slots.entry(channel.clone()).or_default();
        slot.in_flight = true;
        slot.pending = true;
        if self.replies.is_empty() {
            self.inputs.status.set_replying();
        }
        let client = self.client.clone();
        let ports = self.inputs.ports.clone();
        let settings = self.inputs.settings.clone();
        let cancellation = self.work.clone();
        self.replies.spawn(async move {
            let outcome = reply(&client, &ports, settings, &channel, messages, &cancellation).await;
            (channel, outcome)
        });
    }

    async fn finished(&mut self, joined: Result<(String, ReplyOutcome), tokio::task::JoinError>) {
        let Ok((channel, outcome)) = joined else {
            return;
        };
        let slot = self.slots.entry(channel).or_default();
        slot.in_flight = false;
        let pending = std::mem::take(&mut slot.pending);
        match outcome {
            ReplyOutcome::Posted if self.replies.is_empty() => self.inputs.status.set_idle(),
            ReplyOutcome::Cancelled if self.replies.is_empty() => {
                self.inputs.status.set_idle_if_replying()
            }
            ReplyOutcome::Failed(code, retryable) => self.inputs.status.set_error(code, retryable),
            _ => {}
        }
        if pending && !self.work.is_cancelled() {
            self.pass().await;
        }
    }

    async fn shutdown(&mut self) {
        self.work.cancel();
        let drain = async {
            while let Some(joined) = self.replies.join_next().await {
                self.finished(joined).await;
            }
        };
        if tokio::time::timeout(DRAIN, drain).await.is_err() {
            self.replies.shutdown().await;
        }
        self.inputs.status.set_idle_if_replying();
    }
}

fn system_message(agent: &BoundAgent, system_prompt: &str) -> String {
    let name = agent.name.as_deref().unwrap_or("the Agent");
    let mut text = format!(
        "You are {name}, an Agent in a team Workspace. A Person addressed you by name in a \
         Channel; reply to them. Earlier messages are shown oldest first, each prefixed with \
         its author's name and kind in the form `Name (person): text`. Answer with the reply \
         text only, in Markdown."
    );
    let prompt = system_prompt.trim();
    if !prompt.is_empty() {
        text.push_str("\n\n");
        text.push_str(prompt);
    }
    text
}

fn label(principal: Option<&Principal>, author: &str) -> String {
    match principal {
        Some(principal) => {
            let kind = match principal.kind {
                PrincipalKind::Person => "person",
                PrincipalKind::Agent => "agent",
            };
            let name = principal
                .name
                .as_deref()
                .unwrap_or(principal.fingerprint.as_str());
            format!("{name} ({kind})")
        }
        None => format!("{} (unknown)", &author[..author.len().min(8)]),
    }
}

async fn reply(
    client: &ModelClient,
    ports: &PortClient,
    settings: Arc<Mutex<SettingsStore>>,
    channel: &str,
    messages: Vec<ChatMessage>,
    cancellation: &CancellationToken,
) -> ReplyOutcome {
    let configured = teamarness_package_sdk::blocking(cancellation, move |_| {
        let store = settings.lock().map_err(|_| crate::storage_failed())?;
        Ok((
            store.settings().endpoint_base_url.clone(),
            store.settings().model.clone(),
            store.api_key().map_err(|_| crate::storage_failed())?,
        ))
    })
    .await;
    let (endpoint, model, key) = match configured {
        Ok((endpoint, model, Some(key))) => (endpoint, model, key),
        Err(_) if cancellation.is_cancelled() => return ReplyOutcome::Cancelled,
        _ => return ReplyOutcome::Failed(crate::model::ENDPOINT_REJECTED, false),
    };
    let completion = tokio::select! {
        biased;
        () = cancellation.cancelled() => return ReplyOutcome::Cancelled,
        completion = client.complete(&endpoint, &model, &messages, &key) => completion,
    };
    drop(key);
    let markdown = match completion {
        Ok(text) => truncate_chars(text, MAX_REPLY_CHARS),
        Err(ModelFailure { code, retryable }) => return ReplyOutcome::Failed(code, retryable),
    };
    if markdown.trim().is_empty() {
        return ReplyOutcome::Failed(crate::model::MODEL_ERROR, false);
    }
    let post = tokio::select! {
        biased;
        () = cancellation.cancelled() => return ReplyOutcome::Cancelled,
        post = ports.command(CONVERSATIONS_CONTRACT, "message.post", json!({"channelId":channel,"markdown":markdown})) => post,
    };
    match post {
        Ok(_) => ReplyOutcome::Posted,
        Err(_) => ReplyOutcome::Failed(POST_FAILED, false),
    }
}

fn truncate_chars(text: String, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((index, _)) => text[..index].to_owned(),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_system_message() {
        let principal = Principal {
            principal_id: "a".repeat(64),
            kind: PrincipalKind::Person,
            steward_principal_id: None,
            keys: vec!["b".repeat(64)],
            name: Some("Alice".into()),
            fingerprint: "bbbbbbbb".into(),
        };
        assert_eq!(label(Some(&principal), "x"), "Alice (person)");
        assert_eq!(label(None, &"c".repeat(64)), "cccccccc (unknown)");
        let agent = BoundAgent {
            principal_id: "a".repeat(64),
            identity: "b".repeat(64),
            name: Some("Scout".into()),
        };
        let text = system_message(&agent, "  Be brief.  ");
        assert!(text.starts_with("You are Scout, an Agent"));
        assert!(text.ends_with("Be brief."));
        assert_eq!(truncate_chars("héllo".into(), 2), "hé");
        assert_eq!(truncate_chars("hi".into(), 5), "hi");
    }

    #[test]
    fn malformed_changes_never_acknowledge_unknown_progress() {
        for value in [
            json!({"events":[],"continuation":"0","highWater":"1"}),
            json!({"events":[],"continuation":"2","highWater":"1"}),
            json!({"events":[],"continuation":"01","highWater":"1"}),
            json!({"events":[],"continuation":"0","highWater":"0","resyncRequired":true}),
        ] {
            assert!(Batch::parse(value, 0).is_none());
        }
    }
}
