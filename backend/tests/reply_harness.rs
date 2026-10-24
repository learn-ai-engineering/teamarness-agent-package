//! The reply harness through the SDK harness: a scripted Conversations Provider behind
//! `teamarness.package-ports`, a scripted roster, and a stub OpenAI-compatible endpoint on
//! loopback. Nothing off this machine is contacted.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use teamarness_agent_package::router;
use teamarness_package_sdk::BackendError;
use teamarness_package_sdk::testing::{Harness, HarnessBuilder, ScriptedCapability};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

const AGENT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AGENT_KEY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OTHER_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const OTHER_KEY: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const STEWARD_ID: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const STEWARD_KEY: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const BOB_ID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const BOB_KEY: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const STRANGER_KEY: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const WORKSPACE: &str = "01234567890123456789012345678901";
const GENERAL: &str = "00000000000000000000000000000000";
const SIDE: &str = "10000000000000000000000000000001";
const SECRET: &str = "sk-or-v1-SENTINEL-NEVER-SHOWN-0123456789";
const DEADLINE: Duration = Duration::from_secs(15);

fn empty() -> Value {
    json!({})
}

// ------------------------------------------------------------------
// Stub completion endpoint
// ------------------------------------------------------------------

#[derive(Clone)]
enum Behaviour {
    /// `200` with a completion carrying this text.
    Reply(String),
    /// This status with this body.
    Status(u16, String),
    /// Hold the connection until the notify fires, then reply with this text.
    Hold(Arc<Notify>, String),
    /// Close the connection without answering.
    Drop,
}

#[derive(Clone, Debug)]
struct StubRequest {
    path: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct StubEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<StubRequest>>>,
    script: Arc<Mutex<VecDeque<Behaviour>>>,
}

impl StubEndpoint {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<StubRequest>>> = Arc::default();
        let script: Arc<Mutex<VecDeque<Behaviour>>> = Arc::default();
        let accept_requests = requests.clone();
        let accept_script = script.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(
                    socket,
                    accept_requests.clone(),
                    accept_script.clone(),
                ));
            }
        });
        Self {
            base_url,
            requests,
            script,
        }
    }

    fn push(&self, behaviour: Behaviour) {
        self.script.lock().unwrap().push_back(behaviour);
    }

    fn requests(&self) -> Vec<StubRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    requests: Arc<Mutex<Vec<StubRequest>>>,
    script: Arc<Mutex<VecDeque<Behaviour>>>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() > 64 * 1024 {
            return;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let mut lines = head.lines();
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let mut authorization = None;
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => authorization = Some(value.trim().to_owned()),
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    while bytes.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .unwrap_or(Value::Null);
    requests.lock().unwrap().push(StubRequest {
        path,
        authorization,
        body,
    });
    let behaviour = script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Behaviour::Reply("Hello from Scout".into()));
    let (status, body) = match behaviour {
        Behaviour::Drop => return,
        Behaviour::Reply(text) => (200, completion(&text)),
        Behaviour::Status(status, body) => (status, body),
        Behaviour::Hold(release, text) => {
            release.notified().await;
            (200, completion(&text))
        }
    };
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn completion(text: &str) -> String {
    json!({"id": "cmpl-1", "choices": [{"index": 0, "message": {"role": "assistant", "content": text}}]})
        .to_string()
}

// ------------------------------------------------------------------
// Scripted Conversations Provider behind teamarness.package-ports
// ------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Row {
    id: String,
    position: String,
    author: String,
    markdown: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PostMode {
    Accept,
    Uncertain,
}

#[derive(Clone)]
struct Change {
    revision: u64,
    channel: String,
    kind: &'static str,
    row: Row,
}

#[derive(Default)]
struct ConversationsState {
    channels: Vec<(String, String)>,
    messages: HashMap<String, Vec<Row>>,
    next: u64,
    changes: Vec<Change>,
    statuses: HashMap<String, &'static str>,
    fail_reads: HashMap<String, usize>,
    epoch: u64,
    invalid_response: bool,
    /// `(channelId, markdown)` of every `message.post` the harness issued, accepted or not.
    posts: Vec<(String, String)>,
    post_mode: Option<PostMode>,
}

#[derive(Clone, Default)]
struct FakeConversations {
    state: Arc<Mutex<ConversationsState>>,
}

impl FakeConversations {
    fn new() -> Self {
        let fake = Self::default();
        {
            let mut state = fake.state.lock().unwrap();
            state.channels.push((GENERAL.into(), "general".into()));
            state.channels.push((SIDE.into(), "side".into()));
            state.post_mode = Some(PostMode::Accept);
            state.epoch = 1;
        }
        fake
    }

    /// Appends a Message and returns its position.
    fn append(&self, channel: &str, author: &str, markdown: &str) -> String {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let row = Row {
            id: format!("{:064x}", state.next),
            position: format!("{:082x}", state.next),
            author: author.into(),
            markdown: markdown.into(),
        };
        let position = row.position.clone();
        let revision = state.next;
        state.changes.push(Change {
            revision,
            channel: channel.into(),
            kind: "insert",
            row: row.clone(),
        });
        state.messages.entry(channel.into()).or_default().push(row);
        position
    }

    fn append_at(&self, channel: &str, author: &str, markdown: &str, order: u64) {
        self.append(channel, author, markdown);
        let mut state = self.state.lock().unwrap();
        let position = format!("{order:082x}");
        state
            .messages
            .get_mut(channel)
            .unwrap()
            .last_mut()
            .unwrap()
            .position = position.clone();
        state.changes.last_mut().unwrap().row.position = position;
    }

    fn fail_once(&self, port: &str) {
        self.state.lock().unwrap().fail_reads.insert(port.into(), 1);
    }

    fn remove(&self, channel: &str, position: &str) {
        let mut state = self.state.lock().unwrap();
        let rows = state.messages.get_mut(channel).unwrap();
        let index = rows
            .iter()
            .position(|row| row.position == position)
            .unwrap();
        let row = rows.remove(index);
        state.next += 1;
        let revision = state.next;
        state.changes.push(Change {
            revision,
            channel: channel.into(),
            kind: "remove",
            row,
        });
    }

    fn set_post_mode(&self, mode: PostMode) {
        self.state.lock().unwrap().post_mode = Some(mode);
    }

    fn posts(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().posts.clone()
    }

    fn capability(&self) -> ScriptedCapability {
        let fake = self.clone();
        ScriptedCapability::new().calls(move |method, payload| {
            if method != "invoke" {
                return Err(BackendError::new(
                    "capability_request_invalid",
                    false,
                    empty(),
                ));
            }
            assert_eq!(payload["contract"], "dev.teamarness.conversations");
            let request = &payload["request"];
            let port = payload["port"].as_str().unwrap_or_default();
            {
                let mut state = fake.state.lock().unwrap();
                if let Some(remaining) = state.fail_reads.get_mut(port) && *remaining > 0 {
                    *remaining -= 1;
                    return Err(BackendError::new("overloaded", true, empty()));
                }
            }
            match port {
                "channel.list" => {
                    let state = fake.state.lock().unwrap();
                    let channels = state
                        .channels
                        .iter()
                        .map(|(id, name)| json!({"id": id, "name": name}))
                        .collect::<Vec<_>>();
                    Ok(json!({"channels": channels}))
                }
                "message.page" => {
                    let state = fake.state.lock().unwrap();
                    let channel = request["channelId"].as_str().unwrap_or_default();
                    let limit = request["limit"].as_u64().unwrap_or(64) as usize;
                    assert!((1..=64).contains(&limit));
                    let before = request["before"].as_str();
                    let mut rows = state.messages.get(channel).cloned().unwrap_or_default();
                    rows.sort_by(|left, right| left.position.cmp(&right.position));
                    let mut older = rows
                        .iter()
                        .rev()
                        .filter(|row| before.is_none_or(|before| row.position.as_str() < before))
                        .peekable();
                    let mut page = Vec::new();
                    while page.len() < limit {
                        let Some(row) = older.next() else { break };
                        page.push(json!({
                            "id": row.id, "position": row.position, "author": row.author,
                            "markdown": row.markdown, "isLocal": row.author == STEWARD_KEY,
                        }));
                    }
                    let mut response = json!({
                        "messages": page, "pending": [], "epoch": format!("{:032x}", state.epoch), "revision": state.next.to_string(),
                    });
                    if older.peek().is_some()
                        && let Some(last) =
                            response["messages"].as_array().and_then(|rows| rows.last())
                    {
                        response["nextBefore"] = last["position"].clone();
                    }
                    Ok(response)
                }
                "message.changes" => {
                    let state = fake.state.lock().unwrap();
                    let channel = request["channelId"].as_str().unwrap();
                    let after: u64 = request["after"].as_str().unwrap().parse().unwrap();
                    let limit = request["limit"].as_u64().unwrap_or(32) as usize;
                    assert!((1..=32).contains(&limit));
                    if request["epoch"] != format!("{:032x}", state.epoch) || after > state.next {
                        return Ok(json!({"events":[],"continuation":after.to_string(),"highWater":after.to_string(),"resyncRequired":true}));
                    }
                    let events = state.changes.iter().filter(|change| change.channel == channel && change.revision > after)
                        .take(limit).map(|change| {
                            let row = &change.row;
                            let mut event = json!({"revision":change.revision.to_string(),"kind":change.kind,"id":row.id,"position":row.position});
                            if change.kind == "insert" {
                                let present = state.messages.get(channel).is_some_and(|rows| rows.iter().any(|current| current.id == row.id));
                                let status = if present { state.statuses.get(&row.id).copied().unwrap_or("ready") } else { "unavailable" };
                                event["status"] = json!(status);
                                if status == "ready" {
                                    event["message"] = json!({"id":row.id,"position":row.position,"author":row.author,
                                        "markdown":row.markdown,"isLocal":row.author == STEWARD_KEY});
                                }
                            }
                            event
                        }).collect::<Vec<_>>();
                    let continuation = events.last().map_or(state.next.to_string(), |event| event["revision"].as_str().unwrap().to_owned());
                    Ok(json!({"events":events,"continuation":if state.invalid_response { after.to_string() } else { continuation },"highWater":state.next.to_string()}))
                }
                "message.post" => {
                    let channel = request["channelId"].as_str().unwrap_or_default().to_owned();
                    let markdown = request["markdown"].as_str().unwrap_or_default().to_owned();
                    assert!(!markdown.is_empty() && markdown.chars().count() <= 32_768);
                    let mode = {
                        let mut state = fake.state.lock().unwrap();
                        state.posts.push((channel.clone(), markdown.clone()));
                        state.post_mode.unwrap_or(PostMode::Accept)
                    };
                    match mode {
                        PostMode::Accept => {
                            fake.append(&channel, AGENT_KEY, &markdown);
                            Ok(json!({"ok": true}))
                        }
                        PostMode::Uncertain => {
                            Err(BackendError::new("conversations.deferred", false, empty())
                                .with_outcome_uncertain(true))
                        }
                    }
                }
                _ => Err(BackendError::new("ungranted_port", false, empty())),
            }
        })
    }
}

// ------------------------------------------------------------------
// Roster and binding doubles
// ------------------------------------------------------------------

#[derive(Clone)]
struct Roster {
    agent_name: Arc<Mutex<String>>,
    bound: Arc<Mutex<bool>>,
    lookup_failures: Arc<std::sync::atomic::AtomicUsize>,
}

impl Roster {
    fn new() -> Self {
        Self {
            agent_name: Arc::new(Mutex::new("Scout".into())),
            bound: Arc::new(Mutex::new(false)),
            lookup_failures: Arc::default(),
        }
    }

    fn principal(&self, id: &str, key: &str, kind: &str, name: &str) -> Value {
        json!({
            "principalId": id, "kind": kind,
            "stewardPrincipalId": if kind == "agent" { json!(STEWARD_ID) } else { Value::Null },
            "keys": [key], "name": name, "fingerprint": &key[..8],
        })
    }

    fn rows(&self) -> Vec<Value> {
        let agent_name = self.agent_name.lock().unwrap().clone();
        vec![
            self.principal(STEWARD_ID, STEWARD_KEY, "person", "Alice"),
            self.principal(BOB_ID, BOB_KEY, "person", "Bob"),
            self.principal(AGENT_ID, AGENT_KEY, "agent", &agent_name),
            self.principal(OTHER_ID, OTHER_KEY, "agent", "Other"),
        ]
    }

    fn snapshot(&self, cursor: u64) -> Value {
        json!({"workspaceId": WORKSPACE, "cursor": cursor.to_string(), "principals": self.rows()})
    }

    fn bound_row(&self) -> Value {
        if *self.bound.lock().unwrap() {
            let name = self.agent_name.lock().unwrap().clone();
            json!({"principalId": AGENT_ID, "identity": AGENT_KEY, "name": name})
        } else {
            Value::Null
        }
    }

    fn principals_capability(&self) -> ScriptedCapability {
        let roster = self.clone();
        ScriptedCapability::new().calls(move |method, payload| match method {
            "current" => Ok(roster.snapshot(1)),
            "lookup" => {
                if roster
                    .lookup_failures
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |remaining| remaining.checked_sub(1),
                    )
                    .is_ok()
                {
                    return Err(BackendError::new("overloaded", true, empty()));
                }
                let identity = payload["identity"].as_str().unwrap_or_default();
                let found = roster
                    .rows()
                    .into_iter()
                    .find(|row| row["keys"][0] == identity);
                Ok(json!({"principal": found}))
            }
            _ => Err(BackendError::new(
                "capability_request_invalid",
                false,
                empty(),
            )),
        })
    }

    fn binding_capability(&self) -> ScriptedCapability {
        let roster = self.clone();
        ScriptedCapability::new().calls(move |method, _| match method {
            "current" => Ok(json!({"agent": roster.bound_row()})),
            "bind" => {
                *roster.bound.lock().unwrap() = true;
                Ok(json!({"agent": roster.bound_row()}))
            }
            "unbind" => {
                *roster.bound.lock().unwrap() = false;
                Ok(json!({"agent": null}))
            }
            _ => Err(BackendError::new(
                "capability_request_invalid",
                false,
                empty(),
            )),
        })
    }
}

// ------------------------------------------------------------------
// Fixture
// ------------------------------------------------------------------

struct Fixture {
    harness: Harness,
    ports: ScriptedCapability,
    principals: ScriptedCapability,
}

async fn start(directory: &Path, roster: &Roster, conversations: &FakeConversations) -> Fixture {
    let ports = conversations.capability();
    let principals = roster.principals_capability();
    let harness = HarnessBuilder::new(router().into_backend())
        .package_identity("dev.teamarness.agent")
        .workspace(WORKSPACE, "Harness Workspace")
        .data_directory(directory)
        .capability("teamarness.workspace-context", ScriptedCapability::new())
        .capability("teamarness.principals", principals.clone())
        .capability("teamarness.agent-binding", roster.binding_capability())
        .capability("teamarness.package-ports", ports.clone())
        .start()
        .await;
    Fixture {
        harness,
        ports,
        principals,
    }
}

/// Configures the Package against `stub` and binds the Agent; returns once the status is ready
/// and the harness has subscribed to `messages.changed`.
async fn make_ready(fixture: &mut Fixture, stub: &StubEndpoint, context_messages: u32) {
    fixture.harness.activate().await.unwrap();
    fixture
        .harness
        .request(
            "agent.settings.set",
            json!({"settings": {
                "endpointBaseUrl": stub.base_url,
                "model": "stub/model-1",
                "systemPrompt": "Be brief.",
                "contextMessages": context_messages,
            }}),
        )
        .await
        .unwrap();
    fixture
        .harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId": AGENT_ID}))
        .await
        .unwrap();
    wait_status(&mut fixture.harness, "ready", None).await;
    fixture.ports.stream().opened().await;
    wait_until("the Channel baselines", || {
        page_calls(&fixture.ports, SIDE) >= 1
    })
    .await;
}

/// Polls until `condition` holds or the deadline passes.
async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls `agent.status.get` until it shows `state` (and `code` when given).
async fn wait_status(harness: &mut Harness, state: &str, code: Option<&str>) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let status = harness.request("agent.status.get", empty()).await.unwrap();
        assert!(
            !status.to_string().contains(SECRET),
            "the status carries the key"
        );
        if status["state"] == state && code.is_none_or(|code| status["lastError"]["code"] == code) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for status {state} {code:?}; last {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls `agent.status.get` until the bound Agent carries `name`.
async fn wait_agent_name(harness: &mut Harness, name: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let status = harness.request("agent.status.get", empty()).await.unwrap();
        if status["agent"]["name"] == name {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the name {name}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn acknowledged(data: &Path, channel: &str) -> Option<u64> {
    let value: Value =
        serde_json::from_slice(&std::fs::read(data.join("progress.json")).ok()?).ok()?;
    value["channels"][channel]["after"].as_u64()
}

fn page_calls(ports: &ScriptedCapability, channel: &str) -> usize {
    ports
        .recorded_calls()
        .iter()
        .filter(|call| {
            call.payload["port"] == "message.page"
                && call.payload["request"]["channelId"] == channel
        })
        .count()
}

fn change_calls(ports: &ScriptedCapability, channel: &str) -> usize {
    ports
        .recorded_calls()
        .iter()
        .filter(|call| {
            call.payload["port"] == "message.changes"
                && call.payload["request"]["channelId"] == channel
        })
        .count()
}

fn list_calls(ports: &ScriptedCapability) -> usize {
    ports
        .recorded_calls()
        .iter()
        .filter(|call| call.payload["port"] == "channel.list")
        .count()
}

/// Appends a Message and tells the harness the projection changed.
async fn say(
    conversations: &FakeConversations,
    ports: &ScriptedCapability,
    author: &str,
    text: &str,
) -> String {
    let position = conversations.append(GENERAL, author, text);
    assert!(
        ports.stream().send(empty()).await,
        "the event stream is open"
    );
    position
}

/// Waits for one catch-up pass after an invalidation, without relying on display paging.
async fn settle_pass(ports: &ScriptedCapability) {
    let lists = list_calls(ports);
    let changes = change_calls(ports, GENERAL);
    assert!(ports.stream().send(empty()).await);
    wait_until("a pass to complete", || {
        list_calls(ports) > lists && change_calls(ports, GENERAL) > changes
    })
    .await;
    // negative-wait: a reply would start after the scan; a bounded window is the only way to
    // show that none did.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn assert_no_secret(ports: &ScriptedCapability, data: &Path) {
    for call in ports.recorded_calls() {
        assert!(
            !call.payload.to_string().contains(SECRET),
            "a Port call carries the key"
        );
    }
    let positions = std::fs::read_to_string(data.join("progress.json")).unwrap();
    assert!(!positions.contains(SECRET));
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

// Verifies: TEAM.SYS.CONV-R19
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_late_lower_sort_message_is_answered_once() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    conversations.append_at(GENERAL, STEWARD_KEY, "already observed", 100);
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    wait_until("initial baseline", || page_calls(&fixture.ports, SIDE) >= 1).await;
    conversations.append_at(GENERAL, BOB_KEY, "@Scout delivered later", 2);
    assert!(fixture.ports.stream().send(empty()).await);
    wait_until("reply to late lower-sort arrival", || {
        conversations.posts().len() == 1
    })
    .await;
    assert_eq!(stub.request_count(), 1);
    assert_eq!(conversations.posts()[0].0, GENERAL);
    fixture.harness.shutdown().await.unwrap();
}

// Verifies: TEAM.SYS.CONV-R19
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_restart_drains_backlog_without_a_new_notification() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    conversations.append(GENERAL, STEWARD_KEY, "baseline");
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    wait_until("initial baseline", || page_calls(&fixture.ports, SIDE) >= 1).await;
    fixture.harness.shutdown().await.unwrap();
    conversations.append(GENERAL, BOB_KEY, "@Scout oldest queued mention");
    for _ in 0..256 {
        conversations.append(GENERAL, BOB_KEY, "ordinary queued message");
    }
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    wait_until("oldest backlog mention", || {
        conversations.posts().len() == 1
    })
    .await;
    let head = conversations.state.lock().unwrap().next;
    wait_until("all queued revisions, not just the first mention", || {
        acknowledged(data.path(), GENERAL).is_some_and(|after| after >= head)
    })
    .await;
    assert_eq!(stub.request_count(), 1);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_safe_read_recovers_without_another_event() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    for (index, port) in ["channel.list", "message.changes", "message.page", "lookup"]
        .into_iter()
        .enumerate()
    {
        if port == "lookup" {
            roster
                .lookup_failures
                .store(1, std::sync::atomic::Ordering::SeqCst);
        } else {
            conversations.fail_once(port);
        }
        say(
            &conversations,
            &fixture.ports,
            BOB_KEY,
            "@Scout only notification",
        )
        .await;
        wait_until("reply after safe-read recovery", || {
            conversations.posts().len() == index + 1
        })
        .await;
        assert_eq!(stub.request_count(), index + 1, "{port}");
        let head = conversations.state.lock().unwrap().next;
        wait_until("settled attempt", || {
            acknowledged(data.path(), GENERAL).is_some_and(|after| after >= head)
        })
        .await;
    }
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_multiple_mentions_across_batches_are_not_abandoned() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    fixture.harness.shutdown().await.unwrap();
    for index in 0..300 {
        let text = if [0, 140, 290].contains(&index) {
            format!("@Scout queued {index}")
        } else {
            format!("ordinary {index}")
        };
        conversations.append(GENERAL, BOB_KEY, &text);
    }
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    wait_until("all three separated mentions", || {
        conversations.posts().len() == 3
    })
    .await;
    let head = conversations.state.lock().unwrap().next;
    wait_until("complete backlog settlement", || {
        acknowledged(data.path(), GENERAL).is_some_and(|after| after >= head)
    })
    .await;
    assert_eq!(stub.request_count(), 3);
    for (request, index) in stub.requests().iter().zip([0, 140, 290]) {
        assert_eq!(
            request.body["messages"][1]["content"],
            format!("Bob (person): @Scout queued {index}")
        );
    }
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_pending_content_blocks_ack_until_safe_retry_and_removes_never_trigger() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    conversations.append(GENERAL, BOB_KEY, "@Scout unavailable");
    let removed = conversations.append(GENERAL, BOB_KEY, "@Scout removed");
    conversations.remove(GENERAL, &removed);
    conversations.append(GENERAL, BOB_KEY, "@Scout pending");
    let pending_id = {
        let mut state = conversations.state.lock().unwrap();
        let unavailable = state.changes[0].row.id.clone();
        let pending = state.changes[3].row.id.clone();
        state.statuses.insert(unavailable, "unavailable");
        state.statuses.insert(pending.clone(), "pending");
        pending
    };
    conversations.append(GENERAL, BOB_KEY, "@Scout after pending");
    assert!(fixture.ports.stream().send(empty()).await);
    wait_until("prefix before pending revision", || {
        acknowledged(data.path(), GENERAL) == Some(3)
    })
    .await;
    assert_eq!(stub.request_count(), 0);
    conversations
        .state
        .lock()
        .unwrap()
        .statuses
        .remove(&pending_id);
    // No invalidation: pending-content recovery must schedule its own safe query.
    wait_until("pending and subsequent mention", || {
        conversations.posts().len() == 2
    })
    .await;
    assert_eq!(stub.request_count(), 2);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_invalid_epoch_or_continuation_never_replays() {
    for change_epoch in [true, false] {
        let data = tempfile::tempdir().unwrap();
        let stub = StubEndpoint::start().await;
        let roster = Roster::new();
        let conversations = FakeConversations::new();
        let mut fixture = start(data.path(), &roster, &conversations).await;
        make_ready(&mut fixture, &stub, 0).await;
        {
            let mut state = conversations.state.lock().unwrap();
            if change_epoch {
                state.epoch += 1;
            } else {
                state.invalid_response = true;
            }
        }
        say(
            &conversations,
            &fixture.ports,
            BOB_KEY,
            "@Scout unsafe progress",
        )
        .await;
        wait_status(
            &mut fixture.harness,
            "error",
            Some("agent.progress_invalid"),
        )
        .await;
        assert_eq!(stub.request_count(), 0);
        assert!(conversations.posts().is_empty());
        assert_eq!(acknowledged(data.path(), GENERAL), Some(0));
        fixture.harness.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_once_when_addressed_and_only_then() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    // History from before the Package ran, including a mention: never answered.
    conversations.append(GENERAL, STEWARD_KEY, "welcome everyone");
    conversations.append(GENERAL, BOB_KEY, "@Scout are you there?");
    conversations.append(SIDE, BOB_KEY, "Scout, side channel history");
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    let Fixture {
        harness,
        ports,
        principals,
        ..
    } = &mut fixture;
    wait_until("the first pass", || {
        page_calls(ports, GENERAL) >= 1 && page_calls(ports, SIDE) >= 1
    })
    .await;
    settle_pass(ports).await;
    assert_eq!(stub.request_count(), 0, "history was answered");
    assert!(conversations.posts().is_empty());

    // Bob addresses the Agent: one completion, one post, bounded labelled context.
    conversations.append(GENERAL, STEWARD_KEY, "context one");
    conversations.append(GENERAL, STEWARD_KEY, "context two");
    say(&conversations, ports, BOB_KEY, "@Scout hello").await;
    wait_until("the first reply", || conversations.posts().len() == 1).await;
    assert_eq!(
        conversations.posts(),
        vec![(GENERAL.to_owned(), "Hello from Scout".to_owned())]
    );
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/chat/completions");
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some(format!("Bearer {SECRET}").as_str())
    );
    let body = &requests[0].body;
    assert_eq!(body["model"], "stub/model-1");
    assert_eq!(body["stream"], false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6, "{messages:?}");
    assert_eq!(messages[0]["role"], "system");
    let system = messages[0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are Scout, an Agent"));
    assert!(system.ends_with("Be brief."));
    assert_eq!(messages[1]["content"], "Alice (person): welcome everyone");
    assert_eq!(
        messages[2]["content"],
        "Bob (person): @Scout are you there?"
    );
    assert_eq!(messages[3]["content"], "Alice (person): context one");
    assert_eq!(messages[4]["content"], "Alice (person): context two");
    assert_eq!(messages[5]["role"], "user");
    assert_eq!(messages[5]["content"], "Bob (person): @Scout hello");
    wait_status(harness, "ready", None).await;

    // Not addressed: a substring, another Agent, the Agent itself, an unknown key, a side word.
    conversations.append(GENERAL, BOB_KEY, "Scouting report attached");
    conversations.append(GENERAL, OTHER_KEY, "@Scout ping from another agent");
    conversations.append(GENERAL, AGENT_KEY, "@Scout talking to myself");
    conversations.append(GENERAL, STRANGER_KEY, "@Scout from nobody");
    conversations.append(GENERAL, BOB_KEY, "scout_bot and boyscout");
    settle_pass(ports).await;
    assert_eq!(stub.request_count(), 1);
    assert_eq!(conversations.posts().len(), 1);

    // Plain name, any case, and a mention in another Channel is answered there.
    say(&conversations, ports, BOB_KEY, "hey SCOUT, status?").await;
    wait_until("the second reply", || conversations.posts().len() == 2).await;
    assert_eq!(conversations.posts()[1].0, GENERAL);
    conversations.append(SIDE, STEWARD_KEY, "Scout: side question");
    assert!(ports.stream().send(empty()).await);
    wait_until("the side reply", || conversations.posts().len() == 3).await;
    assert_eq!(conversations.posts()[2].0, SIDE);
    let side_body = &stub.requests()[2].body;
    let side_messages = side_body["messages"].as_array().unwrap();
    assert_eq!(side_messages.len(), 3, "{side_messages:?}");
    assert_eq!(
        side_messages[2]["content"],
        "Alice (person): Scout: side question"
    );
    // The Agent's own earlier reply in #general is presented as the assistant; the other Agent
    // and the unknown key are labelled by kind.
    let second_messages = stub.requests()[1].body["messages"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        second_messages
            .iter()
            .any(|message| message["role"] == "assistant"
                && message["content"] == "Hello from Scout"),
        "{second_messages:?}"
    );
    assert!(second_messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"] == "Other (agent): @Scout ping from another agent"
    }));
    assert!(
        second_messages
            .iter()
            .any(|message| { message["content"] == "33333333 (unknown): @Scout from nobody" })
    );

    // A rename changes the trigger word at once.
    *roster.agent_name.lock().unwrap() = "Pathfinder".into();
    principals.stream().opened().await;
    assert!(principals.stream().send(roster.snapshot(2)).await);
    wait_agent_name(harness, "Pathfinder").await;
    conversations.append(GENERAL, BOB_KEY, "@Scout still there?");
    settle_pass(ports).await;
    assert_eq!(conversations.posts().len(), 3);
    stub.push(Behaviour::Reply("Pathfinder here".into()));
    say(&conversations, ports, BOB_KEY, "Pathfinder, are you there?").await;
    wait_until("the reply under the new name", || {
        conversations.posts().len() == 4
    })
    .await;
    assert_eq!(conversations.posts()[3].1, "Pathfinder here");
    assert!(
        stub.requests()[3].body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You are Pathfinder")
    );
    assert_eq!(stub.request_count(), 4);

    assert_no_secret(ports, data.path());
    let settings = harness
        .request("agent.settings.get", empty())
        .await
        .unwrap();
    assert!(!settings.to_string().contains(SECRET));
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_during_a_reply_collapses_and_a_restart_never_re_replies() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    wait_until("the first pass", || page_calls(ports, GENERAL) >= 1).await;

    let release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(release.clone(), "one".into()));
    say(&conversations, ports, BOB_KEY, "@Scout one").await;
    wait_until("the held request", || stub.request_count() == 1).await;
    wait_status(harness, "replying", None).await;
    let pages_before = page_calls(ports, GENERAL);
    let changes_before = change_calls(ports, GENERAL);
    let lists_before = list_calls(ports);

    // A burst while the reply is in flight: every event runs a pass, none pages the busy
    // Channel, and the second mention waits.
    conversations.append(GENERAL, STEWARD_KEY, "chatter");
    assert!(ports.stream().send(empty()).await);
    conversations.append(GENERAL, BOB_KEY, "@Scout two");
    assert!(ports.stream().send(empty()).await);
    assert!(ports.stream().send(empty()).await);
    wait_until("three passes", || list_calls(ports) >= lists_before + 3).await;
    assert_eq!(page_calls(ports, GENERAL), pages_before);
    assert_eq!(conversations.posts().len(), 0);

    stub.push(Behaviour::Reply("two".into()));
    release.notify_one();
    wait_until("both replies", || conversations.posts().len() == 2).await;
    assert_eq!(conversations.posts()[0].1, "one");
    assert_eq!(conversations.posts()[1].1, "two");
    wait_status(harness, "ready", None).await;
    // One rescan found the second mention; one more after its reply found only the Agent's own
    // posts. Nothing else is pending.
    wait_until("the final rescan", || {
        change_calls(ports, GENERAL) == changes_before + 2
    })
    .await;
    // negative-wait: proving no further scan or request happens needs a bounded window.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(change_calls(ports, GENERAL), changes_before + 2);
    assert_eq!(stub.request_count(), 2);
    assert!(
        stub.requests()[1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"] == "Bob (person): @Scout two")
    );
    fixture.harness.shutdown().await.unwrap();

    // A new process generation reads the marks back: nothing is answered again, and the
    // Agent's own replies in the projection are not triggers.
    let positions = std::fs::read_to_string(data.path().join("progress.json")).unwrap();
    assert!(positions.contains(GENERAL));
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    wait_status(&mut fixture.harness, "ready", None).await;
    fixture.ports.stream().opened().await;
    wait_until("the restarted pass", || {
        change_calls(&fixture.ports, GENERAL) >= 1
    })
    .await;
    settle_pass(&fixture.ports).await;
    assert_eq!(stub.request_count(), 2);
    assert_eq!(conversations.posts().len(), 2);
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout three").await;
    wait_until("a reply after restart", || conversations.posts().len() == 3).await;
    assert_no_secret(&fixture.ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoint_and_post_failures_map_to_package_codes_and_are_never_retried() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    wait_until("the first pass", || page_calls(ports, GENERAL) >= 1).await;

    // Neighbouring cases map to different codes, so each status read is this case's own.
    let cases: [(Behaviour, &str, bool); 7] = [
        (
            Behaviour::Status(401, r#"{"error":{"message":"bad key"}}"#.into()),
            "agent.endpoint_rejected",
            false,
        ),
        (
            Behaviour::Status(500, "upstream down".into()),
            "agent.endpoint_unreachable",
            true,
        ),
        (
            Behaviour::Status(200, "this is not json".into()),
            "agent.model_error",
            false,
        ),
        (Behaviour::Drop, "agent.endpoint_unreachable", true),
        (
            Behaviour::Status(200, r#"{"choices":[]}"#.into()),
            "agent.model_error",
            false,
        ),
        (
            Behaviour::Status(403, "forbidden".into()),
            "agent.endpoint_rejected",
            false,
        ),
        (
            Behaviour::Status(200, completion("   ")),
            "agent.model_error",
            false,
        ),
    ];
    for (index, (behaviour, code, retryable)) in cases.into_iter().enumerate() {
        stub.push(behaviour);
        say(
            &conversations,
            ports,
            BOB_KEY,
            &format!("@Scout attempt {index}"),
        )
        .await;
        wait_until("the attempt to reach the stub", || {
            stub.request_count() == index + 1
        })
        .await;
        let status = wait_status(harness, "error", Some(code)).await;
        assert_eq!(status["lastError"]["retryable"], retryable, "{code}");
        assert!(conversations.posts().is_empty());
    }
    // Nothing is retried on its own.
    settle_pass(ports).await;
    assert_eq!(stub.request_count(), 7);
    assert!(conversations.posts().is_empty());

    // The next mention is a fresh attempt, and success clears the error.
    say(&conversations, ports, BOB_KEY, "@Scout once more").await;
    wait_until("a successful reply", || conversations.posts().len() == 1).await;
    wait_status(harness, "ready", None).await;

    // An uncertain post: the mark had moved first, so the trigger is settled for good.
    conversations.set_post_mode(PostMode::Uncertain);
    let trigger = say(&conversations, ports, BOB_KEY, "@Scout uncertain").await;
    let status = wait_status(harness, "error", Some("agent.post_failed")).await;
    assert_eq!(status["lastError"]["retryable"], false);
    assert_eq!(conversations.posts().len(), 2);
    let positions: Value =
        serde_json::from_str(&std::fs::read_to_string(data.path().join("progress.json")).unwrap())
            .unwrap();
    assert!(
        positions["channels"][GENERAL]["after"].as_u64().unwrap()
            >= u64::from_str_radix(&trigger, 16).unwrap(),
        "the mark did not pass the trigger: {positions}"
    );
    settle_pass(ports).await;
    settle_pass(ports).await;
    assert_eq!(
        conversations.posts().len(),
        2,
        "the uncertain post was retried"
    );
    assert_eq!(stub.request_count(), 9);
    assert_no_secret(ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suspend_cancels_a_reply_in_flight_and_nothing_is_posted() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    wait_until("the first pass", || page_calls(ports, GENERAL) >= 1).await;

    let release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(release.clone(), "late".into()));
    say(&conversations, ports, BOB_KEY, "@Scout wait for it").await;
    wait_until("the held request", || stub.request_count() == 1).await;
    harness.suspend().await.unwrap();
    assert!(!ports.stream().is_open(), "suspend closed the subscription");
    release.notify_one();
    // negative-wait: the cancelled reply must not post; only a bounded window can show that.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(conversations.posts().is_empty());

    // Resume restarts the harness; the cancelled mention was marked and is not re-attempted.
    harness.recover().await.unwrap();
    wait_status(harness, "ready", None).await;
    ports.stream().opened().await;
    settle_pass(ports).await;
    assert_eq!(stub.request_count(), 1);
    assert!(conversations.posts().is_empty());
    say(&conversations, ports, BOB_KEY, "@Scout now").await;
    wait_until("a reply after resume", || conversations.posts().len() == 1).await;
    assert_eq!(stub.request_count(), 2);

    // Clearing the key stops the harness and closes the subscription. A mention posted while
    // it is stopped is still new once the key is back, so it is answered then.
    harness.request("agent.key.clear", empty()).await.unwrap();
    wait_status(harness, "unconfigured", None).await;
    wait_until("the subscription to close", || !ports.stream().is_open()).await;
    conversations.append(GENERAL, BOB_KEY, "@Scout while unconfigured");
    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    wait_status(harness, "ready", None).await;
    ports.stream().opened().await;
    wait_until("the mention from the stopped period", || {
        conversations.posts().len() == 2
    })
    .await;
    assert_eq!(stub.request_count(), 3);
    assert_no_secret(ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}
