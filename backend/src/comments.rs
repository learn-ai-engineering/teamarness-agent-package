//! Act on Documents Comments that mention the Agent, and reply under them (Documents 1.3).
//!
//! This is the Conversations reply harness's pattern applied to one Workspace-wide Comment feed.
//! `comments.changed` only wakes catch-up; `comment.changes` is read after the acknowledged
//! revision, saved under [`PROGRESS_KEY`]. The first start baselines at the head without acting,
//! a pending Comment is retried from the same revision, and `resyncRequired` adopts the
//! Provider's new baseline without replaying anything before it.
//!
//! A Person's new Comment that mentions the Agent by name is a trigger. Each Document has one
//! reply in flight; the trigger is persisted before the model call and never retried. The model
//! gets the thread and the request, not the Document: it reads and revises the Document with
//! its own tools. It never gets `comment.add`; the harness adds the turn's final text once as a
//! reply under the trigger, and a failed turn adds nothing.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use teamarness_package_sdk::CancellationToken;
use teamarness_package_sdk::capabilities::agent_binding::BoundAgent;
use teamarness_package_sdk::capabilities::principals::{Principal, PrincipalKind};
use teamarness_package_sdk::ports::{PortAccess, PortClient};
use tokio::task::JoinSet;

use crate::harness::{
    HarnessInputs, POST_FAILED, PROGRESS_INVALID, ReplyOutcome, STORAGE_FAILED, Slot, final_text,
    label, revision, wait_or_cancel,
};
use crate::mention::mentions;
use crate::model::{ChatMessage, ModelClient};
use crate::progress::Cursor;
use crate::tools::{PortCaller, ToolContext};

pub const DOCUMENTS_CONTRACT: &str = "dev.teamarness.documents";
/// The progress entry of the Workspace-wide Comment feed. Channel IDs never contain `#`.
pub const PROGRESS_KEY: &str = "dev.teamarness.documents#comments";

/// The published `comment.changes` bound, and finite work per pass.
const CHANGE_LIMIT: usize = 32;
const MAX_BATCHES: usize = 4;
/// Documents bounds a Comment's Markdown at 32 KiB of UTF-8.
const MAX_COMMENT_BYTES: usize = 32 * 1024;
const RETRY: Duration = Duration::from_secs(1);
/// Documents is enabled but older than 1.3: look again occasionally, not every second.
const UPGRADE_RETRY: Duration = Duration::from_secs(10);
const DRAIN: Duration = Duration::from_secs(1);
/// The Ports this source needs besides `comment.add`, which the model never sees.
const NEEDED: [&str; 3] = ["comments.changed", "comment.changes", "comment.thread"];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comment {
    id: String,
    document_id: String,
    #[serde(default)]
    parent_id: Option<String>,
    author: String,
    markdown: String,
    #[serde(default)]
    quote: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Change {
    revision: String,
    id: String,
    kind: String,
    status: String,
    #[serde(default)]
    comment: Option<Comment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Batch {
    epoch: String,
    events: Vec<Change>,
    continuation: String,
    high_water: String,
    #[serde(default)]
    resync_required: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    document_id: String,
    title: String,
    comments: Vec<Comment>,
    truncated: bool,
}

enum Read {
    Batch(Batch),
    /// The Provider's new baseline; nothing before it is acted on.
    Resync(Cursor),
}

/// Validate continuity before allowing any response to acknowledge unseen work.
fn parse(value: Value, cursor: &Cursor) -> Option<Read> {
    let batch: Batch = serde_json::from_value(value).ok()?;
    let continuation = revision(&batch.continuation)?;
    let high_water = revision(&batch.high_water)?;
    if continuation > high_water {
        return None;
    }
    if batch.resync_required {
        return batch
            .events
            .is_empty()
            .then(|| Cursor::new(batch.epoch, continuation))
            .flatten()
            .map(Read::Resync);
    }
    let after = cursor.after;
    if batch.epoch != cursor.epoch
        || batch.events.len() > CHANGE_LIMIT
        || continuation < after
        || (high_water > after && continuation == after)
    {
        return None;
    }
    let mut previous = after;
    for event in &batch.events {
        let next = revision(&event.revision)?;
        if next <= previous || next > continuation || event.id.is_empty() {
            return None;
        }
        if !matches!(event.kind.as_str(), "add" | "resolve" | "reopen") {
            return None;
        }
        match (event.status.as_str(), &event.comment) {
            ("pending", None) => {}
            ("ready", Some(comment)) if comment.id == event.id => {}
            _ => return None,
        }
        previous = next;
    }
    Some(Read::Batch(batch))
}

/// Whether Documents is usable for the Agent now, by the same check as the model's tools.
enum Gate {
    Usable,
    Unusable,
    /// Enabled, but without the 1.3 Comment Ports.
    Older,
}

async fn gate(ports: &PortClient) -> Gate {
    // The cheap Host summary first: describing every enabled contract each second is not.
    let enabled = crate::catalog::summaries(ports).await.is_ok_and(|rows| {
        rows.iter().any(|row| {
            row.contract == DOCUMENTS_CONTRACT
                && row.major == 1
                && row.available
                && row.enabled
                && row.enabled_major == Some(1)
        })
    });
    if !enabled {
        return Gate::Unusable;
    }
    match crate::access::usable_now(Some(ports)).await {
        Ok(usable) => {
            let documents = usable
                .iter()
                .find(|contract| contract.identity == DOCUMENTS_CONTRACT && contract.major == 1);
            match documents {
                Some(contract)
                    if NEEDED
                        .iter()
                        .all(|needed| contract.ports.iter().any(|port| port.name == *needed)) =>
                {
                    Gate::Usable
                }
                Some(_) => Gate::Older,
                None => Gate::Unusable,
            }
        }
        Err(_) => Gate::Unusable,
    }
}

struct Loop {
    inputs: HarnessInputs,
    client: ModelClient,
    /// One reply in flight per Document.
    slots: HashMap<String, Slot>,
    replies: JoinSet<(String, ReplyOutcome)>,
    work: CancellationToken,
    retry_at: Option<tokio::time::Instant>,
    fatal: bool,
}

pub async fn run(inputs: HarnessInputs, cancellation: CancellationToken) {
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
        // Do not discover, subscribe or read until the binding intent is current and certain.
        if !state.inputs.commands.is_current() {
            if wait_or_cancel(&cancellation, RETRY).await {
                break;
            }
            continue;
        }
        let gate = tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            gate = gate(&state.inputs.ports) => gate,
        };
        let wait = match gate {
            Gate::Usable => None,
            Gate::Unusable => Some(RETRY),
            Gate::Older => Some(UPGRADE_RETRY),
        };
        if let Some(wait) = wait {
            if wait_or_cancel(&cancellation, wait).await {
                break;
            }
            continue;
        }
        let subscription = tokio::select! {
            biased;
            () = cancellation.cancelled() => break,
            events = state.inputs.ports.subscribe_event(PortAccess::Agent, DOCUMENTS_CONTRACT, 1, "comments.changed") => events,
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
                    Some(Err(_)) | None => {
                        events.close().await;
                        // End affected turns before an explicit reconnect, even for an idle stream.
                        state.shutdown().await;
                        state.work = cancellation.child();
                        state.slots.clear();
                        break;
                    }
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
            response = self.inputs.commands.query(DOCUMENTS_CONTRACT, 1, port, request) => response.map_err(|_| ()),
        }
    }

    async fn cursor(&self) -> Result<Option<Cursor>, ()> {
        let progress = self.inputs.progress.clone();
        teamarness_package_sdk::blocking(&self.work, move |_| {
            progress
                .lock()
                .map_err(|_| crate::storage_failed())
                .map(|progress| progress.cursor(PROGRESS_KEY).cloned())
        })
        .await
        .map_err(|_| ())
    }

    /// Saves `cursor`: a monotonic acknowledgment, or with `rebaseline` the Provider's new one.
    async fn save(&mut self, cursor: &Cursor, rebaseline: bool) -> bool {
        let progress = self.inputs.progress.clone();
        let cursor = cursor.clone();
        let saved = teamarness_package_sdk::blocking(&self.work, move |token| {
            if token.is_cancelled() {
                return Err(crate::storage_failed());
            }
            let mut progress = progress.lock().map_err(|_| crate::storage_failed())?;
            if rebaseline {
                progress.rebaseline(PROGRESS_KEY, cursor)
            } else {
                progress.advance(PROGRESS_KEY, cursor)
            }
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

    async fn persist(&mut self, cursor: &Cursor) -> bool {
        self.save(cursor, false).await
    }

    /// Saves the cursor before an unhandled change and retries from it later.
    async fn retry_from(&mut self, cursor: &Cursor) {
        if self.persist(cursor).await {
            self.schedule(RETRY);
        }
    }

    async fn pass(&mut self) {
        self.retry_at = None;
        let Some(agent) = self.inputs.status.agent() else {
            return;
        };
        // The same mutex protects blocking settings writes; never wait for it on this worker.
        let settings = self.inputs.settings.clone();
        let configured = teamarness_package_sdk::blocking(&self.work, move |cancel| {
            let store = settings.lock().map_err(|_| crate::storage_failed())?;
            if cancel.is_cancelled() {
                return Err(crate::storage_failed());
            }
            Ok(store.settings().system_prompt.clone())
        })
        .await;
        let system_prompt = match configured {
            Ok(prompt) => prompt,
            Err(_) => {
                if !self.work.is_cancelled() {
                    self.inputs.status.set_error(STORAGE_FAILED, true);
                    self.schedule(RETRY);
                }
                return;
            }
        };
        let mut cursor = match self.cursor().await {
            Ok(Some(cursor)) => cursor,
            Ok(None) => {
                // First start: the head is the baseline; earlier Comments are never answered.
                let Ok(head) = self.query("comment.changes", json!({})).await else {
                    self.schedule(RETRY);
                    return;
                };
                let baseline = head["epoch"]
                    .as_str()
                    .zip(head["continuation"].as_str().and_then(revision))
                    .and_then(|(epoch, after)| Cursor::new(epoch.to_owned(), after));
                match baseline {
                    Some(baseline) => {
                        self.save(&baseline, true).await;
                    }
                    None => self.invalid_progress(),
                }
                return;
            }
            Err(()) => {
                self.inputs.status.set_error(STORAGE_FAILED, true);
                self.schedule(RETRY);
                return;
            }
        };
        let mut lookups = HashMap::new();
        for _ in 0..MAX_BATCHES {
            let Ok(value) = self
                .query(
                    "comment.changes",
                    json!({"epoch":cursor.epoch,"after":cursor.after.to_string(),"limit":CHANGE_LIMIT}),
                )
                .await
            else {
                self.schedule(RETRY);
                return;
            };
            let batch = match parse(value, &cursor) {
                Some(Read::Batch(batch)) => batch,
                Some(Read::Resync(baseline)) => {
                    // A new epoch or a head behind the cursor: adopt it without acting.
                    self.save(&baseline, true).await;
                    return;
                }
                None => {
                    self.invalid_progress();
                    return;
                }
            };
            for event in batch.events {
                if event.status == "pending" {
                    self.retry_from(&cursor).await;
                    return;
                }
                let next = revision(&event.revision).expect("validated revision");
                if event.kind == "add"
                    && let Some(comment) = event.comment
                    && comment.author != agent.identity
                    && agent
                        .name
                        .as_deref()
                        .is_some_and(|name| mentions(&comment.markdown, name))
                {
                    let principal = match self.lookup(&comment.author, &mut lookups).await {
                        Ok(principal) => principal,
                        Err(()) => {
                            self.retry_from(&cursor).await;
                            return;
                        }
                    };
                    if principal.is_some_and(|principal| principal.kind == PrincipalKind::Person) {
                        if self
                            .slots
                            .get(&comment.document_id)
                            .is_some_and(|slot| slot.in_flight)
                        {
                            // Wait for this Document's reply; its end runs another pass.
                            self.slots
                                .entry(comment.document_id.clone())
                                .or_default()
                                .pending = true;
                            self.persist(&cursor).await;
                            return;
                        }
                        let request = match self.request(&comment, &mut lookups).await {
                            Ok(request) => request,
                            Err(()) => {
                                self.retry_from(&cursor).await;
                                return;
                            }
                        };
                        cursor.after = next;
                        if !self.persist(&cursor).await || self.work.is_cancelled() {
                            return;
                        }
                        let prompt = Prompt {
                            agent: agent.clone(),
                            system_prompt: system_prompt.clone(),
                        };
                        self.spawn_reply(comment.document_id, comment.id, prompt, request);
                        continue;
                    }
                }
                cursor.after = next;
            }
            cursor.after = revision(&batch.continuation).expect("validated continuation");
            if !self.persist(&cursor).await {
                return;
            }
            if cursor.after == revision(&batch.high_water).expect("validated high water") {
                return;
            }
        }
        // The work bound limits this pass, not the history that can ever be processed.
        self.schedule(Duration::ZERO);
    }

    /// The one user message of a reply: where the mention is, the thread and the request.
    async fn request(
        &self,
        trigger: &Comment,
        lookups: &mut HashMap<String, Option<Principal>>,
    ) -> Result<String, ()> {
        let value = self
            .query("comment.thread", json!({"commentId": trigger.id}))
            .await?;
        let mut thread: Thread = serde_json::from_value(value).map_err(|_| ())?;
        if thread.document_id != trigger.document_id {
            return Err(());
        }
        if !thread
            .comments
            .iter()
            .any(|comment| comment.id == trigger.id)
        {
            thread.comments.push(trigger.clone());
        }
        let mut labels = Vec::with_capacity(thread.comments.len());
        for comment in &thread.comments {
            let principal = self.lookup(&comment.author, lookups).await?;
            labels.push(label(principal.as_ref(), &comment.author));
        }
        Ok(compose(trigger, &thread, &labels))
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

    fn spawn_reply(&mut self, document: String, trigger: String, prompt: Prompt, request: String) {
        let slot = self.slots.entry(document.clone()).or_default();
        slot.in_flight = true;
        slot.pending = true;
        if self.replies.is_empty() {
            self.inputs.status.set_replying();
        }
        let client = self.client.clone();
        let inputs = self.inputs.clone();
        let cancellation = self.work.clone();
        self.replies.spawn(async move {
            let outcome = reply(
                &client,
                &inputs,
                &document,
                &trigger,
                prompt,
                request,
                &cancellation,
            )
            .await;
            (document, outcome)
        });
    }

    async fn finished(&mut self, joined: Result<(String, ReplyOutcome), tokio::task::JoinError>) {
        let Ok((document, outcome)) = joined else {
            return;
        };
        let slot = self.slots.entry(document).or_default();
        slot.in_flight = false;
        let pending = std::mem::take(&mut slot.pending);
        if self.replies.is_empty() {
            self.inputs.status.set_idle_if_replying();
        }
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

/// Builds the reply's user message. `labels` names each thread Comment's author, in order.
fn compose(trigger: &Comment, thread: &Thread, labels: &[String]) -> String {
    let mut text = format!(
        "You were mentioned in a Comment on Document \"{}\" (id {}).\n",
        thread.title, thread.document_id
    );
    // A reply rarely quotes; the thread is then about its root's excerpt.
    let quote = trigger.quote.as_deref().or_else(|| {
        thread
            .comments
            .first()
            .filter(|root| root.parent_id.is_none())
            .and_then(|root| root.quote.as_deref())
    });
    if let Some(quote) = quote {
        text.push_str("\nQuoted text:\n");
        for line in quote.lines() {
            let _ = writeln!(text, "> {line}");
        }
    }
    text.push_str("\nThread, oldest first:\n");
    if thread.truncated {
        text.push_str("(Earlier replies are left out.)\n");
    }
    for (comment, label) in thread.comments.iter().zip(labels) {
        let mark = if comment.id == trigger.id {
            " [mentioned you]"
        } else {
            ""
        };
        let _ = writeln!(text, "- {label}{mark}: {}", comment.markdown);
    }
    let _ = write!(
        text,
        "\nRequest:\n{}\n\nTo read the Document, use Documents `document.markdown`; to change it, \
         use `document.revise`. Pass the Document id {}.",
        trigger.markdown, thread.document_id
    );
    text
}

fn system_message(agent: &BoundAgent, system_prompt: &str, tools: &ToolContext) -> String {
    let name = agent.name.as_deref().unwrap_or("the Agent");
    let mut text = format!(
        "You are {name}, an Agent in a team Workspace. A Person mentioned you in a Comment on a \
         Document; do what they ask, using your tools. Thread Comments are shown oldest first, \
         each prefixed with its author's name and kind in the form `Name (person): text`. Your \
         final answer is added once as your reply under their Comment: say briefly what you \
         changed, or why you could not change anything. Answer with the reply text only, in \
         Markdown."
    );
    let prompt = system_prompt.trim();
    if !prompt.is_empty() {
        text.push_str("\n\n");
        text.push_str(prompt);
    }
    tools.append_to(&mut text);
    text
}

/// What a reply's system message is built from once its tools are known.
struct Prompt {
    agent: BoundAgent,
    system_prompt: String,
}

async fn reply(
    client: &ModelClient,
    inputs: &HarnessInputs,
    document: &str,
    trigger: &str,
    prompt: Prompt,
    request: String,
    cancellation: &CancellationToken,
) -> ReplyOutcome {
    let history = vec![ChatMessage::user(request)];
    let markdown = match final_text(client, inputs, history, cancellation, |tools| {
        system_message(&prompt.agent, &prompt.system_prompt, tools)
    })
    .await
    {
        Ok(text) => truncate_bytes(text, MAX_COMMENT_BYTES),
        Err(outcome) => return outcome,
    };
    // The trigger is already persisted, so the add is attempted at most once and never re-sent.
    let added = tokio::select! {
        biased;
        () = cancellation.cancelled() => return ReplyOutcome::Cancelled,
        added = inputs.commands.command(
            DOCUMENTS_CONTRACT, 1,
            "comment.add",
            json!({"documentId":document,"parentId":trigger,"markdown":markdown}),
        ) => added,
    };
    match added {
        Ok(_) => ReplyOutcome::Posted,
        Err(_) => ReplyOutcome::Failed(POST_FAILED, false),
    }
}

/// At most `limit` bytes of `text`, cut at a character boundary.
fn truncate_bytes(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(after: u64) -> Cursor {
        Cursor::new("a".repeat(32), after).unwrap()
    }

    fn comment(id: &str) -> Value {
        json!({"id":id,"documentId":"d","author":"k","markdown":"m","resolved":false})
    }

    #[test]
    fn truncates_on_a_character_boundary() {
        assert_eq!(truncate_bytes("héllo".into(), 2), "h");
        assert_eq!(truncate_bytes("héllo".into(), 3), "hé");
        assert_eq!(truncate_bytes("hi".into(), 5), "hi");
    }

    #[test]
    fn a_resync_is_a_new_baseline_and_malformed_changes_acknowledge_nothing() {
        let epoch = "a".repeat(32);
        let other = "b".repeat(32);
        let Some(Read::Resync(baseline)) = parse(
            json!({"epoch":other,"events":[],"continuation":"3","highWater":"3","resyncRequired":true}),
            &cursor(9),
        ) else {
            panic!("a resync is a baseline");
        };
        assert_eq!(baseline, Cursor::new(other.clone(), 3).unwrap());
        assert!(matches!(
            parse(
                json!({"epoch":epoch,"events":[{"revision":"2","id":"x","kind":"add","status":"ready","comment":comment("x")}],"continuation":"2","highWater":"2"}),
                &cursor(1),
            ),
            Some(Read::Batch(_))
        ));
        for value in [
            // Another epoch without a resync.
            json!({"epoch":other,"events":[],"continuation":"1","highWater":"1"}),
            json!({"epoch":epoch,"events":[],"continuation":"0","highWater":"1"}),
            json!({"epoch":epoch,"events":[],"continuation":"2","highWater":"1"}),
            json!({"epoch":epoch,"events":[],"continuation":"01","highWater":"1"}),
            // Events with a resync, or that do not carry what their status says.
            json!({"epoch":epoch,"events":[{"revision":"2","id":"x","kind":"add","status":"pending"}],"continuation":"2","highWater":"2","resyncRequired":true}),
            json!({"epoch":epoch,"events":[{"revision":"2","id":"x","kind":"add","status":"ready"}],"continuation":"2","highWater":"2"}),
            json!({"epoch":epoch,"events":[{"revision":"2","id":"y","kind":"add","status":"ready","comment":comment("x")}],"continuation":"2","highWater":"2"}),
            json!({"epoch":epoch,"events":[{"revision":"2","id":"x","kind":"remove","status":"pending"}],"continuation":"2","highWater":"2"}),
            json!({"epoch":epoch,"events":[{"revision":"1","id":"x","kind":"add","status":"pending"}],"continuation":"2","highWater":"2"}),
        ] {
            assert!(parse(value.clone(), &cursor(1)).is_none(), "{value}");
        }
    }
}
