//! The reply harness through the SDK harness: a scripted Conversations Provider behind
//! `teamarness.package-ports`, a scripted roster, and a stub OpenAI-compatible endpoint on
//! loopback. Nothing off this machine is contacted.

mod support;

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
const EXHIBITS: &str = "dev.teamarness.exhibits";

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
    /// `200` with an assistant message that calls these tools, as `(id, name, arguments)`.
    ToolCalls(Vec<(String, String, String)>),
    HoldToolCalls(Arc<Notify>, Vec<(String, String, String)>),
}

/// One scripted tool call, as [`Behaviour::ToolCalls`] takes it.
fn call(id: &str, name: &str, arguments: &str) -> (String, String, String) {
    (id.into(), name.into(), arguments.into())
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
        Behaviour::ToolCalls(calls) => (200, tool_calls(&calls)),
        Behaviour::HoldToolCalls(release, calls) => {
            release.notified().await;
            (200, tool_calls(&calls))
        }
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

fn tool_calls(calls: &[(String, String, String)]) -> String {
    let calls: Vec<Value> = calls
        .iter()
        .map(|(id, name, arguments)| {
            json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}})
        })
        .collect();
    json!({"id": "cmpl-1", "choices": [{"index": 0, "message": {
        "role": "assistant", "content": null, "tool_calls": calls,
    }}]})
    .to_string()
}

/// `200` with an assistant message that has both `text` and these raw `tool_calls`.
fn text_with_tool_calls(text: &str, tool_calls: &Value) -> Behaviour {
    let body = json!({"id": "cmpl-1", "choices": [{"index": 0, "message": {
        "role": "assistant", "content": text, "tool_calls": tool_calls,
    }}]});
    Behaviour::Status(200, body.to_string())
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
    Transport,
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
    /// `(port, kind, request)` of every Exhibits invoke the Agent issued.
    exhibits_calls: Vec<(String, String, Value)>,
    /// One-shot Provider errors, by Exhibits port.
    exhibits_errors: HashMap<String, VecDeque<BackendError>>,
}

#[derive(Clone, Default)]
struct FakeConversations {
    state: Arc<Mutex<ConversationsState>>,
    catalog: support::HostCatalog,
}

impl FakeConversations {
    fn new() -> Self {
        let fake = Self {
            catalog: support::HostCatalog::providers(),
            ..Self::default()
        };
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

    fn exhibits_calls(&self) -> Vec<(String, String, Value)> {
        self.state.lock().unwrap().exhibits_calls.clone()
    }

    /// The next Exhibits call to `port` fails with this Provider `code`.
    fn fail_exhibits_once(&self, port: &str, code: &'static str) {
        self.fail_exhibits_error_once(port, BackendError::new(code, false, empty()));
    }

    fn fail_exhibits_error_once(&self, port: &str, error: BackendError) {
        self.state
            .lock()
            .unwrap()
            .exhibits_errors
            .entry(port.into())
            .or_default()
            .push_back(error);
    }

    /// The scripted Exhibits Provider: its guide, one Exhibit, and any scripted error.
    fn exhibits(&self, port: &str, kind: &str, request: &Value) -> Result<Value, BackendError> {
        let mut state = self.state.lock().unwrap();
        state
            .exhibits_calls
            .push((port.into(), kind.into(), request.clone()));
        if let Some(error) = state
            .exhibits_errors
            .get_mut(port)
            .and_then(VecDeque::pop_front)
        {
            return Err(error);
        }
        match port {
            "runtime.guide" => Ok(json!({"guide": "List Exhibits with exhibit.list."})),
            "exhibit.list" => {
                Ok(json!({"exhibits": [{"exhibitId": "q3-review", "title": "Q3 review"}]}))
            }
            _ => Err(BackendError::new("exhibits.not_found", false, empty())),
        }
    }

    fn capability(&self) -> ScriptedCapability {
        let fake = self.clone();
        ScriptedCapability::new().calls(move |method, payload| {
            if let Some(answer) = fake.catalog.handle(method, &payload) { return answer; }
            if method == "invoke" && (payload["access"] != "agent" || payload["major"] != if payload["contract"] == support::UNKNOWN { 3 } else { 1 }) {
                return Err(BackendError::new("capability_request_invalid", false, empty()));
            }
            if method == "invoke" && !fake.catalog.selections.lock().unwrap().contains_key(payload["contract"].as_str().unwrap_or_default()) {
                return Err(BackendError::new("port_not_enabled", false, empty()));
            }
            if method == "invoke" && payload["contract"] == support::UNKNOWN {
                return Ok(json!({"sample":"independent-fixture-answer"}));
            }
            if method == "invoke" && payload["contract"] == EXHIBITS {
                return fake.exhibits(
                    payload["port"].as_str().unwrap_or_default(),
                    payload["kind"].as_str().unwrap_or_default(),
                    &payload["request"],
                );
            }
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
                        PostMode::Transport => Err(BackendError::new("host_unavailable", true, empty())),
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
    other_bound: Arc<Mutex<bool>>,
}

impl Roster {
    fn new() -> Self {
        Self {
            agent_name: Arc::new(Mutex::new("Scout".into())),
            bound: Arc::new(Mutex::new(false)),
            lookup_failures: Arc::default(),
            other_bound: Arc::default(),
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
        if *self.other_bound.lock().unwrap() {
            json!({"principalId":OTHER_ID,"identity":OTHER_KEY,"name":"Other"})
        } else if *self.bound.lock().unwrap() {
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
        ScriptedCapability::new().calls(move |method, payload| match method {
            "current" => Ok(json!({"agent": roster.bound_row()})),
            "bind" => {
                *roster.other_bound.lock().unwrap() = payload["principalId"] == OTHER_ID;
                *roster.bound.lock().unwrap() = true;
                Ok(json!({"agent": roster.bound_row()}))
            }
            "unbind" => {
                *roster.other_bound.lock().unwrap() = false;
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
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":support::CONVERSATIONS,"enabled":true,"major":1}),
        )
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
    assert!(system.contains("\n\nBe brief.\n\n"));
    assert!(system.contains("`conversations__runtime_guide`"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rebind_drains_a_held_completion_before_changing_actor() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    let release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(release.clone(), "old A completion".into()));
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout held").await;
    wait_until("held A request", || stub.request_count() == 1).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":OTHER_ID}))
        .await
        .unwrap();
    let status = fixture
        .harness
        .request("agent.status.get", empty())
        .await
        .unwrap();
    assert_eq!(status["agent"]["principalId"], OTHER_ID);
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        conversations.posts().is_empty(),
        "A completion posted after binding B"
    );
    fixture.ports.stream().opened().await;
    say(&conversations, &fixture.ports, BOB_KEY, "@Other fresh").await;
    wait_until("fresh B reply", || conversations.posts().len() == 1).await;
    assert_eq!(stub.request_count(), 2);
    assert!(
        stub.requests()[1].body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You are Other")
    );
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_settings_and_key_edits_clear_errors_without_hiding_active_work() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    let settings = json!({"settings": {"endpointBaseUrl":stub.base_url,"model":"stub/model-1","systemPrompt":"updated","contextMessages":0}});
    for operation in ["agent.settings.set", "agent.key.set"] {
        stub.push(Behaviour::Status(401, "rejected".into()));
        say(&conversations, &fixture.ports, BOB_KEY, "@Scout rejected").await;
        wait_status(
            &mut fixture.harness,
            "error",
            Some("agent.endpoint_rejected"),
        )
        .await;
        let payload = if operation == "agent.key.set" {
            json!({"apiKey": SECRET})
        } else {
            settings.clone()
        };
        fixture
            .harness
            .request(operation, payload.clone())
            .await
            .unwrap();
        wait_status(&mut fixture.harness, "ready", None).await;
        let release = Arc::new(Notify::new());
        stub.push(Behaviour::Hold(release.clone(), "still active".into()));
        let before = stub.request_count();
        say(&conversations, &fixture.ports, BOB_KEY, "@Scout held").await;
        wait_until("held request", || stub.request_count() == before + 1).await;
        // A second Channel fails while the first Channel remains active.
        stub.push(Behaviour::Status(401, "rejected".into()));
        conversations.append(SIDE, BOB_KEY, "@Scout side failure");
        assert!(fixture.ports.stream().send(empty()).await);
        wait_status(
            &mut fixture.harness,
            "error",
            Some("agent.endpoint_rejected"),
        )
        .await;
        fixture.harness.request(operation, payload).await.unwrap();
        wait_status(&mut fixture.harness, "replying", None).await;
        release.notify_one();
        wait_status(&mut fixture.harness, "ready", None).await;
    }
    assert_eq!(conversations.posts().len(), 2);
    assert_eq!(stub.request_count(), 6);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_binding_and_readiness_changes_leave_no_old_work() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    let release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(release.clone(), "old".into()));
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout held").await;
    wait_until("held request", || stub.request_count() == 1).await;
    let bind = fixture
        .harness
        .start_request("agent.binding.set", json!({"principalId":OTHER_ID}))
        .await;
    let unbind = fixture
        .harness
        .start_request("agent.binding.clear", empty())
        .await;
    let clear = fixture
        .harness
        .start_request("agent.key.clear", empty())
        .await;
    let bound = bind.result().await;
    let unbound = unbind.result().await;
    assert!(bound.is_ok() || unbound.is_ok());
    for refused in [bound, unbound].into_iter().filter_map(Result::err) {
        assert_eq!(refused.code(), "agent.binding_busy");
        assert!(refused.retryable());
    }
    clear.result().await.unwrap();
    // A concurrent choice may be refused, not queued indefinitely. Reissue only the
    // identity choice after the actual mutation settles; never replay the old reply.
    fixture
        .harness
        .request("agent.binding.clear", empty())
        .await
        .unwrap();
    wait_status(&mut fixture.harness, "unconfigured", None).await;
    assert!(!fixture.ports.stream().is_open());
    let status = fixture
        .harness
        .request("agent.status.get", empty())
        .await
        .unwrap();
    assert_eq!(status["agent"].is_null(), roster.bound_row().is_null());
    release.notify_one();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(conversations.posts().is_empty());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_post_failure_does_not_block_later_replies_or_binding() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;

    // The Provider's deferred error is not retried. Canonical Harness drops the scripted
    // uncertainty flag, so this is certain-error liveness, NOT uncertainty transport proof.
    conversations.set_post_mode(PostMode::Uncertain);
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout uncertain").await;
    let status = wait_status(&mut fixture.harness, "error", Some("agent.post_failed")).await;
    assert_eq!(status["lastError"]["retryable"], false);
    assert_eq!(conversations.posts().len(), 1);

    // A later trigger is answered, without a restart, and success clears the error.
    conversations.set_post_mode(PostMode::Accept);
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout again").await;
    wait_until("a reply after the failed post", || {
        conversations.posts().len() == 2
    })
    .await;
    wait_status(&mut fixture.harness, "ready", None).await;
    assert_eq!(stub.request_count(), 2);

    // Binding changes are not refused either.
    let bound = fixture
        .harness
        .request("agent.binding.set", json!({"principalId": OTHER_ID}))
        .await
        .unwrap();
    assert_eq!(bound["agent"]["principalId"], OTHER_ID);
    let cleared = fixture
        .harness
        .request("agent.binding.clear", empty())
        .await
        .unwrap();
    assert!(cleared["agent"].is_null());
    assert!(roster.bound_row().is_null());
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId": AGENT_ID}))
        .await
        .unwrap();
    assert_eq!(roster.bound_row()["principalId"], AGENT_ID);
    wait_status(&mut fixture.harness, "ready", None).await;
    fixture.ports.stream().opened().await;
    say(
        &conversations,
        &fixture.ports,
        BOB_KEY,
        "@Scout after rebind",
    )
    .await;
    wait_until("a reply after rebinding", || {
        conversations.posts().len() == 3
    })
    .await;
    assert_eq!(stub.request_count(), 3);
    assert_no_secret(&fixture.ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}

/// Polls `agent.chat.get` until `done` holds.
async fn wait_chat(
    harness: &mut Harness,
    what: &str,
    mut done: impl FnMut(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let chat = harness.request("agent.chat.get", empty()).await.unwrap();
        assert!(
            !chat.to_string().contains(SECRET),
            "the chat carries the key"
        );
        if done(&chat) {
            return chat;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; last {chat}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_answers_the_steward_directly_and_never_posts() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    harness.activate().await.unwrap();

    // Nothing is sent until endpoint, model and key are all stored.
    assert_eq!(
        harness.request("agent.chat.get", empty()).await.unwrap(),
        json!({"entries": [], "pending": false})
    );
    let refused = harness
        .request("agent.chat.send", json!({"content": "hi"}))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "agent.not_configured");
    assert!(!refused.retryable());
    harness
        .request(
            "agent.settings.set",
            json!({"settings": {
                "endpointBaseUrl": stub.base_url,
                "model": "stub/model-1",
                "systemPrompt": "Be brief.",
                "contextMessages": 20,
            }}),
        )
        .await
        .unwrap();
    let refused = harness
        .request("agent.chat.send", json!({"content": "hi"}))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "agent.not_configured");
    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
    let refused = harness
        .request("agent.chat.send", json!({"content": "  \n "}))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "agent.invalid_message");
    assert_eq!(stub.request_count(), 0);

    // Without a bound Agent the chat still works; the send answers before the model does and
    // the reply arrives on the subscription.
    let mut changes = harness
        .subscribe("agent.chat.changes", empty())
        .await
        .unwrap();
    assert_eq!(
        changes.next_event().await.unwrap().unwrap(),
        json!({"entries": [], "pending": false})
    );
    stub.push(Behaviour::Reply("Hello, Steward".into()));
    let started = harness
        .request("agent.chat.send", json!({"content": " hello there "}))
        .await
        .unwrap();
    assert_eq!(started["pending"], true);
    assert_eq!(
        started["entries"],
        json!([{"id": 1, "role": "user", "content": "hello there"}])
    );
    let answered = loop {
        let event = changes.next_event().await.unwrap().unwrap();
        if event["pending"] == false {
            break event;
        }
    };
    assert_eq!(
        answered,
        json!({"entries": [
            {"id": 1, "role": "user", "content": "hello there"},
            {"id": 2, "role": "assistant", "content": "Hello, Steward"},
        ], "pending": false})
    );
    let first = stub.requests().pop().unwrap();
    assert_eq!(first.path, "/chat/completions");
    assert_eq!(
        first.authorization.as_deref(),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert_eq!(first.body["model"], "stub/model-1");
    let messages = first.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let system = messages[0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are the Agent,"), "{system}");
    assert!(system.ends_with("Be brief."), "{system}");
    assert_eq!(
        messages[1],
        json!({"role": "user", "content": "hello there"})
    );

    // Once bound, the system message names the Agent and earlier turns are context.
    harness
        .request("agent.binding.set", json!({"principalId": AGENT_ID}))
        .await
        .unwrap();
    wait_status(harness, "ready", None).await;
    stub.push(Behaviour::Reply("Second".into()));
    harness
        .request("agent.chat.send", json!({"content": "and again"}))
        .await
        .unwrap();
    wait_chat(harness, "the second reply", |chat| {
        chat["entries"].as_array().unwrap().len() == 4
    })
    .await;
    let second = stub.requests().pop().unwrap();
    let messages = second.body["messages"].as_array().unwrap();
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You are Scout,")
    );
    assert_eq!(
        messages[2],
        json!({"role": "assistant", "content": "Hello, Steward"})
    );
    assert_eq!(messages[3], json!({"role": "user", "content": "and again"}));

    // One turn at a time; a clear drops the turn in flight and its late completion.
    let release = Arc::new(Notify::new());
    let requests = stub.request_count();
    stub.push(Behaviour::Hold(release.clone(), "too late".into()));
    harness
        .request("agent.chat.send", json!({"content": "slow one"}))
        .await
        .unwrap();
    // The held completion must have reached the stub, or its Hold would serve a later turn.
    wait_until("the slow chat request", || {
        stub.request_count() == requests + 1
    })
    .await;
    let busy = harness
        .request("agent.chat.send", json!({"content": "another"}))
        .await
        .unwrap_err();
    assert_eq!(busy.code(), "agent.chat_busy");
    assert!(busy.retryable());
    let cleared = harness.request("agent.chat.clear", empty()).await.unwrap();
    assert_eq!(cleared, json!({"entries": [], "pending": false}));
    release.notify_one();
    // negative-wait: a dropped completion must never be appended; only a bounded window shows it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        harness.request("agent.chat.get", empty()).await.unwrap(),
        json!({"entries": [], "pending": false})
    );

    // Suspend interrupts a turn in flight; the Steward can send again after resume.
    let release = Arc::new(Notify::new());
    let requests = stub.request_count();
    stub.push(Behaviour::Hold(release.clone(), "after suspend".into()));
    harness
        .request("agent.chat.send", json!({"content": "before suspend"}))
        .await
        .unwrap();
    wait_until("the held chat request", || {
        stub.request_count() == requests + 1
    })
    .await;
    harness.suspend().await.unwrap();
    release.notify_one();
    harness.recover().await.unwrap();
    let interrupted = wait_chat(harness, "the interruption", |chat| chat["pending"] == false).await;
    assert_eq!(
        interrupted["lastError"],
        json!({"code": "agent.chat_interrupted", "retryable": true})
    );
    assert_eq!(interrupted["entries"].as_array().unwrap().len(), 1);
    // negative-wait: the cancelled completion must not be appended after resume.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let kept = harness.request("agent.chat.get", empty()).await.unwrap();
    assert_eq!(kept, interrupted);

    assert!(
        conversations.posts().is_empty(),
        "the chat posted to a Channel"
    );
    assert_no_secret(ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}

// ------------------------------------------------------------------
// Package tools in chat and replies
// ------------------------------------------------------------------

/// Stores endpoint, model, prompt and key, without binding an Agent.
async fn configure(harness: &mut Harness, stub: &StubEndpoint) {
    harness
        .request(
            "agent.settings.set",
            json!({"settings": {
                "endpointBaseUrl": stub.base_url,
                "model": "stub/model-1",
                "systemPrompt": "Be brief.",
                "contextMessages": 20,
            }}),
        )
        .await
        .unwrap();
    harness
        .request("agent.key.set", json!({"apiKey": SECRET}))
        .await
        .unwrap();
}

async fn set_exhibits(harness: &mut Harness, enabled: bool) {
    harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    let view = harness
        .request(
            "agent.packages.set",
            if enabled {
                json!({"contract":EXHIBITS,"enabled":true,"major":1})
            } else {
                json!({"contract":EXHIBITS,"enabled":false})
            },
        )
        .await
        .unwrap();
    let row = view["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["contract"] == EXHIBITS)
        .unwrap();
    assert_eq!(row["enabled"], enabled, "{view}");
    assert_eq!(row["available"], true, "{view}");
}

/// Sends one chat message and waits for its turn to settle.
async fn chat(harness: &mut Harness, content: &str) -> Value {
    let before = harness.request("agent.chat.get", empty()).await.unwrap()["entries"]
        .as_array()
        .unwrap()
        .len();
    harness
        .request("agent.chat.send", json!({"content": content}))
        .await
        .unwrap();
    wait_chat(harness, "the chat turn", |chat| {
        chat["pending"] == false && chat["entries"].as_array().unwrap().len() > before
    })
    .await
}

/// The names of the tools a model request offered; `None` when it has no `tools` field.
fn offered(body: &Value) -> Option<Vec<String>> {
    body.get("tools").map(|tools| {
        tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                assert_eq!(tool["type"], "function");
                tool["function"]["name"].as_str().unwrap().to_owned()
            })
            .collect()
    })
}

fn last_message(body: &Value) -> Value {
    body["messages"].as_array().unwrap().last().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_refuses_an_oversized_file_put_before_it_reaches_the_host() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let harness = &mut fixture.harness;
    harness.activate().await.unwrap();
    configure(harness, &stub).await;
    set_exhibits(harness, true).await;

    // A put larger than the whole production message bound.
    let arguments = json!({
        "exhibitId": "e1",
        "path": "index.html",
        "chunk": "A".repeat(128 * 1024),
    })
    .to_string();
    stub.push(Behaviour::ToolCalls(vec![call(
        "call-1",
        "exhibits__file_put",
        &arguments,
    )]));
    stub.push(Behaviour::Reply(
        "That file is too large for one turn.".into(),
    ));
    let answered = chat(harness, "make a big page").await;
    assert_eq!(answered.get("lastError"), None, "{answered}");
    assert_eq!(
        answered["entries"][1],
        json!({
            "id": 2, "role": "assistant", "content": "That file is too large for one turn.",
            "activity": [{
                "package": "exhibits", "port": "file.put", "ok": false,
                "code": "agent.tool_arguments_too_large",
            }],
        })
    );
    // Nothing reached the Host.
    assert!(conversations.exhibits_calls().is_empty());

    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    let messages = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        r#"{"elided":"agent.tool_arguments_too_large"}"#
    );
    let result: Value = serde_json::from_str(messages[3]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["error"], "agent.tool_arguments_too_large");
    assert_eq!(result["retryable"], false);
    assert_eq!(result["argumentBytes"], arguments.len());
    assert!(result["remainingBytes"].as_u64().unwrap() < 96 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_uses_an_enabled_package_and_records_activity() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let harness = &mut fixture.harness;
    harness.activate().await.unwrap();
    configure(harness, &stub).await;
    set_exhibits(harness, true).await;

    stub.push(Behaviour::ToolCalls(vec![call(
        "call-1",
        "exhibits__exhibit_list",
        "{}",
    )]));
    stub.push(Behaviour::Reply("There is one Exhibit: Q3 review.".into()));
    let answered = chat(harness, "what exhibits are there?").await;
    assert_eq!(answered.get("lastError"), None, "{answered}");
    assert_eq!(
        answered["entries"][1],
        json!({
            "id": 2, "role": "assistant", "content": "There is one Exhibit: Q3 review.",
            "activity": [{"package": "exhibits", "port": "exhibit.list", "ok": true}],
        })
    );
    // The Steward's own entry carries no activity.
    assert_eq!(answered["entries"][0].get("activity"), None);
    assert_eq!(
        conversations.exhibits_calls(),
        vec![("exhibit.list".to_owned(), "query".to_owned(), json!({}))]
    );

    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    let tools = offered(&requests[0].body).expect("tools are offered");
    for name in [
        "exhibits__runtime_guide",
        "exhibits__exhibit_list",
        "skill_read",
    ] {
        assert!(tools.contains(&name.to_owned()), "{tools:?}");
    }
    assert!(
        tools
            .iter()
            .all(|name| !name.starts_with("conversations__")),
        "{tools:?}"
    );
    // File transfer is offered; the harness alone posts replies.
    assert!(
        tools.contains(&"exhibits__file_read".to_owned()),
        "{tools:?}"
    );
    assert!(
        tools.contains(&"exhibits__file_put".to_owned()),
        "{tools:?}"
    );
    assert!(!tools.contains(&"conversations__message_post".to_owned()));
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("`exhibits__runtime_guide`"), "{system}");
    assert!(system.contains("- teamarness-packages: "), "{system}");
    // The second model request carries the call and its result.
    let messages = requests[1].body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "call-1");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call-1");
    let result: Value = serde_json::from_str(messages[3]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["exhibits"][0]["title"], "Q3 review");

    // Only final text is kept as history: the next turn sees no tool exchange.
    stub.push(Behaviour::Reply("You're welcome.".into()));
    chat(harness, "thanks").await;
    let third = stub.requests().pop().unwrap();
    let messages = third.body["messages"].as_array().unwrap();
    assert_eq!(
        messages[1..],
        [
            json!({"role": "user", "content": "what exhibits are there?"}),
            json!({"role": "assistant", "content": "There is one Exhibit: Q3 review."}),
            json!({"role": "user", "content": "thanks"}),
        ]
    );
    assert!(conversations.posts().is_empty());
    assert_no_secret(&fixture.ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_package_tools_are_not_offered() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let harness = &mut fixture.harness;
    harness.activate().await.unwrap();
    configure(harness, &stub).await;
    // Installed and once enabled, then disabled again.
    set_exhibits(harness, true).await;
    set_exhibits(harness, false).await;

    // Even when the model names an Exhibits tool, no tools were offered, so the call is never
    // run and never reaches the Host.
    stub.push(text_with_tool_calls(
        "I cannot list Exhibits.",
        &json!([{"id": "call-1", "type": "function", "function": {
            "name": "exhibits__exhibit_list", "arguments": "{}",
        }}]),
    ));
    let answered = chat(harness, "what exhibits are there?").await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(offered(&requests[0].body), None, "{}", requests[0].body);
    assert!(
        !requests[0].body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("exhibits__")
    );
    assert!(conversations.exhibits_calls().is_empty());
    // No Package operation ran, so the answer shows none.
    assert_eq!(answered["entries"][1].get("activity"), None, "{answered}");
    assert_eq!(answered["entries"][1]["content"], "I cannot list Exhibits.");
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_uses_tool_then_posts_exactly_once() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    set_exhibits(&mut fixture.harness, true).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    wait_until("the first pass", || page_calls(ports, GENERAL) >= 1).await;

    stub.push(Behaviour::ToolCalls(vec![call(
        "call-1",
        "exhibits__exhibit_list",
        "{}",
    )]));
    stub.push(Behaviour::Reply("We have one Exhibit: Q3 review.".into()));
    say(
        &conversations,
        ports,
        BOB_KEY,
        "@Scout what exhibits are there?",
    )
    .await;
    wait_until("the reply", || conversations.posts().len() == 1).await;
    assert_eq!(
        conversations.posts(),
        vec![(
            GENERAL.to_owned(),
            "We have one Exhibit: Q3 review.".to_owned()
        )]
    );
    wait_status(harness, "ready", None).await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        offered(&requests[0].body)
            .unwrap()
            .contains(&"exhibits__exhibit_list".to_owned())
    );
    let system = requests[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are Scout, an Agent"), "{system}");
    assert!(system.contains("`exhibits__runtime_guide`"), "{system}");
    assert_eq!(last_message(&requests[1].body)["role"], "tool");
    assert_eq!(
        conversations.exhibits_calls(),
        vec![("exhibit.list".to_owned(), "query".to_owned(), json!({}))]
    );

    // Nothing is retried or posted again.
    settle_pass(ports).await;
    assert_eq!(conversations.posts().len(), 1);
    assert_eq!(stub.request_count(), 2);
    assert_eq!(conversations.exhibits_calls().len(), 1);
    assert_no_secret(ports, data.path());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_never_offers_message_post() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    set_exhibits(&mut fixture.harness, true).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    wait_until("the first pass", || page_calls(ports, GENERAL) >= 1).await;

    // The model tries to post by itself: refused locally, and the harness posts once.
    stub.push(Behaviour::ToolCalls(vec![call(
        "call-1",
        "conversations__message_post",
        &json!({"channelId": GENERAL, "markdown": "sneaky"}).to_string(),
    )]));
    stub.push(Behaviour::Reply("Only the harness posts.".into()));
    say(&conversations, ports, BOB_KEY, "@Scout post something").await;
    wait_until("the reply", || conversations.posts().len() == 1).await;
    wait_status(harness, "ready", None).await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    let tools = offered(&requests[0].body).unwrap();
    assert!(!tools.is_empty());
    assert!(
        tools.iter().all(|name| !name.contains("message_post")),
        "{tools:?}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(last_message(&requests[1].body)["content"].as_str().unwrap())
            .unwrap(),
        json!({"error": "agent.tool_unavailable", "retryable": false})
    );
    settle_pass(ports).await;
    assert_eq!(
        conversations.posts(),
        vec![(GENERAL.to_owned(), "Only the harness posts.".to_owned())]
    );
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_error_recovered_in_same_turn() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let harness = &mut fixture.harness;
    harness.activate().await.unwrap();
    configure(harness, &stub).await;
    set_exhibits(harness, true).await;

    conversations.fail_exhibits_once("exhibit.list", "exhibits.type_refused");
    stub.push(Behaviour::ToolCalls(vec![call(
        "call-1",
        "exhibits__exhibit_list",
        r#"{"limit": 5}"#,
    )]));
    stub.push(Behaviour::ToolCalls(vec![call(
        "call-2",
        "exhibits__exhibit_list",
        "{}",
    )]));
    stub.push(Behaviour::Reply("Recovered: Q3 review.".into()));
    let answered = chat(harness, "what exhibits are there?").await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 3);
    // The Provider's stable code reaches the model in the next request of the same turn.
    assert_eq!(
        serde_json::from_str::<Value>(last_message(&requests[1].body)["content"].as_str().unwrap())
            .unwrap(),
        json!({"error": "exhibits.type_refused", "retryable": false})
    );
    assert_eq!(answered.get("lastError"), None, "{answered}");
    assert_eq!(
        answered["entries"][1],
        json!({
            "id": 2, "role": "assistant", "content": "Recovered: Q3 review.",
            "activity": [
                {"package": "exhibits", "port": "exhibit.list", "ok": false, "code": "exhibits.type_refused"},
                {"package": "exhibits", "port": "exhibit.list", "ok": true},
            ],
        })
    );
    assert_eq!(conversations.exhibits_calls().len(), 2);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_provider_error_command_preserves_model_envelope_and_binding_liveness() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let harness = &mut fixture.harness;
    harness.activate().await.unwrap();
    configure(harness, &stub).await;
    set_exhibits(harness, true).await;

    // Script only an ordinary false-flag wrapper. True uncertainty and asynchronous held
    // commands are covered at the production owner interface, not this serialized SDK pump.
    conversations.fail_exhibits_error_once(
        "draft.delete",
        BackendError::new(
            "provider_error",
            false,
            json!({"provider":{
                "code":"qualification.refused", "retryable":false, "details":{}
            }}),
        ),
    );
    stub.push(Behaviour::ToolCalls(vec![call(
        "refused-delete",
        "exhibits__draft_delete",
        r#"{"exhibitId":"site"}"#,
    )]));
    stub.push(Behaviour::Reply("The Provider refused; no retry.".into()));
    let answered = chat(harness, "delete the draft").await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        serde_json::from_str::<Value>(last_message(&requests[1].body)["content"].as_str().unwrap())
            .unwrap(),
        json!({"error":"provider_error", "retryable":false, "details":{
            "provider":{"code":"qualification.refused", "retryable":false, "details":{}}
        }})
    );
    assert_eq!(answered.get("lastError"), None, "{answered}");
    assert_eq!(
        answered["entries"][1]["activity"],
        json!([
            {"package":"exhibits", "port":"draft.delete", "ok":false, "code":"provider_error"}
        ])
    );
    let bound = harness
        .request("agent.binding.set", json!({"principalId":OTHER_ID}))
        .await
        .unwrap();
    assert_eq!(bound["agent"]["principalId"], OTHER_ID);
    assert_eq!(roster.bound_row()["principalId"], OTHER_ID);
    assert_eq!(
        conversations.exhibits_calls(),
        vec![(
            "draft.delete".into(),
            "command".into(),
            json!({"exhibitId":"site"})
        )],
        "ordinary refusal must not be replayed"
    );
    assert!(conversations.posts().is_empty());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_chat_turn_keeps_the_activity_it_recorded() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    let harness = &mut fixture.harness;
    harness.activate().await.unwrap();
    configure(harness, &stub).await;
    set_exhibits(harness, true).await;

    // The tool call succeeds, then the endpoint fails: the turn fails, keeping that call.
    stub.push(Behaviour::ToolCalls(vec![call(
        "call-1",
        "exhibits__exhibit_list",
        "{}",
    )]));
    stub.push(Behaviour::Status(500, "{}".into()));
    let failed = chat(harness, "what exhibits are there?").await;
    assert_eq!(
        failed,
        json!({
            "entries": [{"id": 1, "role": "user", "content": "what exhibits are there?"}],
            "pending": false,
            "lastError": {"code": "agent.endpoint_unreachable", "retryable": true},
            "failedActivity": [{"package": "exhibits", "port": "exhibit.list", "ok": true}],
        })
    );
    assert_eq!(stub.request_count(), 2);
    assert_eq!(conversations.exhibits_calls().len(), 1);

    // The next turn starts clean.
    stub.push(Behaviour::Reply("Q3 review.".into()));
    let answered = chat(harness, "try again").await;
    assert_eq!(answered.get("lastError"), None, "{answered}");
    assert_eq!(answered.get("failedActivity"), None, "{answered}");
    assert!(conversations.posts().is_empty());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turn_limit_ends_reply_with_status_code_and_no_post() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 20).await;
    set_exhibits(&mut fixture.harness, true).await;
    let Fixture { harness, ports, .. } = &mut fixture;
    wait_until("the first pass", || page_calls(ports, GENERAL) >= 1).await;

    // Seventeen calls in one answer cross the per-turn tool-call budget of sixteen.
    let calls = (0..17)
        .map(|index| call(&format!("call-{index}"), "exhibits__exhibit_list", "{}"))
        .collect();
    stub.push(Behaviour::ToolCalls(calls));
    say(&conversations, ports, BOB_KEY, "@Scout list everything").await;
    let status = wait_status(harness, "error", Some("agent.turn_limit_exceeded")).await;
    assert_eq!(status["lastError"]["retryable"], false);
    assert!(conversations.posts().is_empty());
    assert!(conversations.exhibits_calls().is_empty());
    // The trigger is settled: nothing is retried.
    settle_pass(ports).await;
    assert_eq!(stub.request_count(), 1);
    assert!(conversations.posts().is_empty());

    // The next mention is a fresh turn, and success clears the error.
    say(&conversations, ports, BOB_KEY, "@Scout hello").await;
    wait_until("the next reply", || conversations.posts().len() == 1).await;
    wait_status(harness, "ready", None).await;
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_packages_direct_chat_request_is_unchanged_and_conversations_never_participates() {
    // The system texts and request bodies from before Package tools existed, byte for byte.
    const CHAT_SYSTEM: &str = "You are Scout, an Agent in a team Workspace. Your Steward is \
        chatting with you directly, outside any Channel. Answer with the reply text only, in \
        Markdown.\n\nBe brief.";
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    let Fixture { harness, ports, .. } = &mut fixture;
    assert!(conversations.catalog.selections.lock().unwrap().is_empty());
    stub.push(Behaviour::Reply("Hello, Steward".into()));
    let answered = chat(harness, "hello").await;
    assert_eq!(
        answered,
        json!({"entries": [
            {"id": 1, "role": "user", "content": "hello"},
            {"id": 2, "role": "assistant", "content": "Hello, Steward"},
        ], "pending": false})
    );
    assert_eq!(
        stub.requests()[0].body,
        json!({
            "model": "stub/model-1",
            "messages": [
                {"role": "system", "content": CHAT_SYSTEM},
                {"role": "user", "content": "hello"},
            ],
            "stream": false,
        })
    );

    // A completed suspension drains any attempted background work, not a guessed delay.
    conversations.append(GENERAL, BOB_KEY, "@Scout unchecked");
    harness.suspend().await.unwrap();
    assert!(ports.recorded_opens().is_empty());
    assert!(
        ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "invoke" && call.method != "describe")
    );
    assert_eq!(stub.request_count(), 1);
    assert!(conversations.posts().is_empty());
    harness.recover().await.unwrap();
    harness
        .request(
            "agent.packages.set",
            json!({"contract":support::CONVERSATIONS,"enabled":true,"major":1}),
        )
        .await
        .unwrap();
    ports.stream().opened().await;
    wait_until("enabled initial read", || page_calls(ports, GENERAL) >= 1).await;
    say(&conversations, ports, BOB_KEY, "@Scout now enabled").await;
    wait_until("enabled reply", || conversations.posts().len() == 1).await;
    let calls = ports.recorded_calls();
    for call in calls.iter().filter(|call| call.method == "invoke") {
        assert_eq!(call.payload["access"], "agent");
        assert_eq!(call.payload["major"], 1);
    }
    let mut opened = ports.recorded_opens()[0].clone();
    assert!(
        opened
            .as_object_mut()
            .unwrap()
            .remove("grant")
            .unwrap()
            .is_string()
    );
    assert_eq!(
        opened,
        json!({"access":"agent","contract":support::CONVERSATIONS,"major":1,"port":"messages.changed","kind":"event"})
    );
    let tools = offered(&stub.requests()[1].body).unwrap();
    assert!(tools.contains(&"conversations__runtime_guide".to_owned()));
    assert!(!tools.contains(&"conversations__message_post".to_owned()));
    assert!(tools.iter().all(|name| !name.starts_with("exhibits__")));
    settle_pass(ports).await;
    assert_eq!(conversations.posts().len(), 1);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_direct_chat_and_enabled_conversations_preserve_large_starting_history() {
    // Only the bytes a turn's own tool exchange adds count against the message bound, so a
    // history over 96 KiB is sent as it was before Package tools existed.
    const BOUND: usize = 96 * 1024;
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    assert!(conversations.catalog.selections.lock().unwrap().is_empty());
    let Fixture { harness, ports, .. } = &mut fixture;
    // Five full-size chat messages of three-byte characters: under the transcript's character
    // bound, but well over the message bound in bytes.
    let long = "中".repeat(8192);
    for round in 1..=5 {
        stub.push(Behaviour::Reply(format!("Noted {round}.")));
        let answered = chat(harness, &long).await;
        assert_eq!(answered.get("lastError"), None, "round {round}");
        assert_eq!(
            answered["entries"].as_array().unwrap().last().unwrap()["content"],
            format!("Noted {round}.")
        );
    }
    let requests = stub.requests();
    assert_eq!(requests.len(), 5);
    let last = &requests[4].body;
    assert_eq!(offered(last), None);
    assert_eq!(last["messages"].as_array().unwrap().len(), 1 + 9);
    assert!(serde_json::to_vec(&last["messages"]).unwrap().len() > BOUND);

    assert!(
        ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "invoke")
    );
    assert!(ports.recorded_opens().is_empty());
    harness
        .request(
            "agent.packages.set",
            json!({"contract":support::CONVERSATIONS,"enabled":true,"major":1}),
        )
        .await
        .unwrap();
    ports.stream().opened().await;
    wait_until("initial reads", || page_calls(ports, GENERAL) >= 1).await;
    // A reply whose Channel context is over the bound is sent and posted once.
    for _ in 0..4 {
        conversations.append(GENERAL, BOB_KEY, &"x".repeat(30_000));
    }
    say(&conversations, ports, BOB_KEY, "@Scout summarize that").await;
    wait_until("the reply", || conversations.posts().len() == 1).await;
    wait_status(harness, "ready", None).await;
    let requests = stub.requests();
    assert_eq!(requests.len(), 6);
    let reply = &requests[5].body;
    let tools = offered(reply).unwrap();
    assert!(tools.contains(&"conversations__runtime_guide".to_owned()));
    assert!(
        tools
            .iter()
            .all(|name| !name.starts_with("exhibits__") && name != "conversations__message_post")
    );
    assert!(serde_json::to_vec(&reply["messages"]).unwrap().len() > BOUND);
    assert_eq!(
        last_message(reply)["content"],
        "Bob (person): @Scout summarize that"
    );
    settle_pass(ports).await;
    assert_eq!(conversations.posts().len(), 1);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_direct_chat_ignores_both_well_formed_and_malformed_unsolicited_tools() {
    // With no tools offered, only an answer's text is read: tool calls the endpoint adds on its
    // own, well formed or not, never start another model call.
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    assert!(conversations.catalog.selections.lock().unwrap().is_empty());
    let Fixture { harness, ports, .. } = &mut fixture;
    let well_formed = json!([{"id": "call-1", "type": "function", "function": {
        "name": "exhibits__exhibit_list", "arguments": "{}",
    }}]);
    stub.push(text_with_tool_calls("Hello, Steward", &well_formed));
    let answered = chat(harness, "hello").await;
    assert_eq!(answered.get("lastError"), None, "{answered}");
    assert_eq!(
        answered["entries"][1],
        json!({"id": 2, "role": "assistant", "content": "Hello, Steward"})
    );
    assert_eq!(stub.request_count(), 1);
    assert_eq!(offered(&stub.requests()[0].body), None);

    let malformed = json!([{"id": "", "function": {"arguments": {}}}]);
    stub.push(text_with_tool_calls("Hi Bob", &malformed));
    let answered = chat(harness, "hello again").await;
    assert_eq!(answered["entries"][3]["content"], "Hi Bob");
    assert_eq!(answered.get("lastError"), None);
    assert_eq!(answered["entries"][3].get("activity"), None);
    assert_eq!(stub.request_count(), 2);
    assert_eq!(offered(&stub.requests()[1].body), None);
    harness.suspend().await.unwrap();
    assert_eq!(stub.request_count(), 2);
    assert!(ports.recorded_opens().is_empty());
    assert!(
        ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "invoke" && call.method != "describe")
    );
    assert!(conversations.posts().is_empty());
    assert!(conversations.exhibits_calls().is_empty());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_unknown_metadata_becomes_a_major_pinned_tool_with_complete_schema_not_guide_inference()
 {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    conversations
        .catalog
        .documents
        .lock()
        .unwrap()
        .insert(support::UNKNOWN.into(), support::unknown_document());
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":support::UNKNOWN,"enabled":true,"major":3}),
        )
        .await
        .unwrap();
    stub.push(Behaviour::ToolCalls(vec![call(
        "invalid",
        "survey__sample_read",
        r#"{"label":"x"}"#,
    )]));
    stub.push(Behaviour::ToolCalls(vec![call(
        "valid",
        "survey__sample_read",
        r#"{"label":"north"}"#,
    )]));
    stub.push(Behaviour::Reply("The independent fixture answered.".into()));
    let answered = chat(&mut fixture.harness, "survey north").await;
    assert_eq!(
        answered["entries"][1]["content"],
        "The independent fixture answered."
    );
    let requests = stub.requests();
    let definitions = requests[0].body["tools"].as_array().unwrap();
    let names: Vec<_> = definitions
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"survey__runtime_guide"));
    assert!(names.contains(&"survey__sample_read"));
    assert!(
        !names.contains(&"survey__sample_watch") && !names.contains(&"survey__samples_changed")
    );
    let schema = &definitions
        .iter()
        .find(|tool| tool["function"]["name"] == "survey__sample_read")
        .unwrap()["function"]["parameters"];
    assert_eq!(
        schema,
        &json!({"type":"object","additionalProperties":false,"required":["label"],"properties":{"label":{"type":"string","minLength":2,"maxLength":12}}})
    );
    assert_eq!(
        serde_json::from_str::<Value>(last_message(&requests[1].body)["content"].as_str().unwrap())
            .unwrap()["error"],
        "agent.tool_arguments_invalid"
    );
    assert!(
        last_message(&requests[2].body)["content"]
            .as_str()
            .unwrap()
            .contains("independent-fixture-answer")
    );
    let calls = fixture.ports.recorded_calls();
    let invokes: Vec<_> = calls
        .iter()
        .filter(|call| call.method == "invoke")
        .collect();
    assert_eq!(
        invokes.len(),
        1,
        "invalid arguments never dispatch, guide is not a schema oracle"
    );
    assert_eq!(
        invokes[0].payload,
        json!({"access":"agent","contract":support::UNKNOWN,"major":3,"port":"sample.read","kind":"query","request":{"label":"north"}})
    );
    let chunks: Vec<_> = calls
        .iter()
        .filter(|call| call.method == "describe")
        .collect();
    assert!(chunks.len() > 1);
    assert_eq!(chunks[0].payload["offset"], 0);
    assert!(chunks.iter().all(|call| call.payload["access"] == "agent"));
    let manifest: Value =
        serde_json::from_str(include_str!("../../teamarness.package.json")).unwrap();
    assert!(
        manifest["consumedPorts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|contract| contract["identity"] != support::UNKNOWN)
    );
    assert!(conversations.posts().is_empty());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_host_metadata_is_refused_by_actual_sdk_before_model_or_dispatch() {
    for corruption in ["offset", "digest", "total", "base64"] {
        let data = tempfile::tempdir().unwrap();
        let stub = StubEndpoint::start().await;
        let roster = Roster::new();
        let conversations = FakeConversations::new();
        *conversations.catalog.corruption.lock().unwrap() = Some(corruption);
        let mut fixture = start(data.path(), &roster, &conversations).await;
        fixture.harness.activate().await.unwrap();
        configure(&mut fixture.harness, &stub).await;
        set_exhibits(&mut fixture.harness, true).await;
        let error = fixture
            .harness
            .request("agent.chat.send", json!({"content":"read exhibits"}))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "agent.capability_unavailable", "{corruption}");
        assert_eq!(
            error.details()["causeCode"],
            "package.invalid_capability_response"
        );
        assert_eq!(stub.request_count(), 0);
        assert!(
            fixture
                .ports
                .recorded_calls()
                .iter()
                .all(|call| call.method != "invoke")
        );
        fixture.harness.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disable_cancels_affected_reply_and_chat_then_explicit_enable_reconnects_without_replay() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    let reply_release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(
        reply_release.clone(),
        "old reply must not post".into(),
    ));
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout held reply").await;
    wait_until("reply request accepted by external model", || {
        stub.request_count() == 1
    })
    .await;
    let chat_release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(
        chat_release.clone(),
        "old chat must not finish".into(),
    ));
    fixture
        .harness
        .request("agent.chat.send", json!({"content":"held chat"}))
        .await
        .unwrap();
    wait_until("chat request accepted by external model", || {
        stub.request_count() == 2
    })
    .await;
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":support::CONVERSATIONS,"enabled":false}),
        )
        .await
        .unwrap();
    let interrupted = fixture
        .harness
        .request("agent.chat.get", empty())
        .await
        .unwrap();
    assert_eq!(interrupted["pending"], false);
    assert_eq!(interrupted["lastError"]["code"], "agent.chat_interrupted");
    assert!(!fixture.ports.stream().is_open());
    reply_release.notify_one();
    chat_release.notify_one();
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":support::CONVERSATIONS,"enabled":true,"major":1}),
        )
        .await
        .unwrap();
    fixture.ports.stream().opened().await;
    say(
        &conversations,
        &fixture.ports,
        BOB_KEY,
        "@Scout fresh reply",
    )
    .await;
    wait_until("fresh reply", || conversations.posts().len() == 1).await;
    settle_pass(&fixture.ports).await;
    assert_eq!(stub.request_count(), 3);
    assert_eq!(
        conversations.posts(),
        vec![(GENERAL.into(), "Hello from Scout".into())]
    );
    let transcript = fixture
        .harness
        .request("agent.chat.get", empty())
        .await
        .unwrap();
    assert_eq!(transcript["entries"].as_array().unwrap().len(), 1);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_terminal_drains_reply_before_explicit_new_stream_and_never_replays_trigger() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    let release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(
        release.clone(),
        "lost generation reply".into(),
    ));
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout held").await;
    wait_until("model accepted old-generation turn", || {
        stub.request_count() == 1
    })
    .await;
    fixture.ports.stream().close("provider_exited").await;
    fixture.ports.stream().opened().await;
    release.notify_one();
    say(
        &conversations,
        &fixture.ports,
        BOB_KEY,
        "@Scout new generation",
    )
    .await;
    wait_until("fresh generation post", || conversations.posts().len() == 1).await;
    settle_pass(&fixture.ports).await;
    assert_eq!(stub.request_count(), 2);
    assert_eq!(fixture.ports.recorded_opens().len(), 2);
    assert_eq!(
        conversations.posts(),
        vec![(GENERAL.into(), "Hello from Scout".into())]
    );
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn selection_and_binding_changes_preserve_an_independent_no_tools_chat() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    let release = Arc::new(Notify::new());
    stub.push(Behaviour::Hold(
        release.clone(),
        "independent direct answer".into(),
    ));
    fixture
        .harness
        .request("agent.chat.send", json!({"content":"independent"}))
        .await
        .unwrap();
    wait_until("unbound direct model request", || stub.request_count() == 1).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":EXHIBITS,"enabled":false}),
        )
        .await
        .unwrap();
    fixture
        .harness
        .request("agent.binding.clear", empty())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .harness
            .request("agent.chat.get", empty())
            .await
            .unwrap()["pending"],
        true
    );
    release.notify_one();
    let answered = wait_chat(&mut fixture.harness, "independent direct answer", |chat| {
        chat["pending"] == false
    })
    .await;
    assert_eq!(
        answered["entries"][1]["content"],
        "independent direct answer"
    );
    assert!(fixture.ports.recorded_opens().is_empty());
    assert!(
        fixture
            .ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "invoke" && call.method != "describe")
    );
    assert_eq!(stub.request_count(), 1);
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_turn_metadata_never_substitutes_for_host_authority_on_a_later_dispatch() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    conversations
        .catalog
        .documents
        .lock()
        .unwrap()
        .insert(support::UNKNOWN.into(), support::unknown_document());
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":support::UNKNOWN,"enabled":true,"major":3}),
        )
        .await
        .unwrap();
    let release = Arc::new(Notify::new());
    stub.push(Behaviour::ToolCalls(vec![call(
        "first",
        "survey__sample_read",
        r#"{"label":"north"}"#,
    )]));
    stub.push(Behaviour::HoldToolCalls(
        release.clone(),
        vec![call(
            "second",
            "survey__sample_read",
            r#"{"label":"south"}"#,
        )],
    ));
    stub.push(Behaviour::Reply(
        "Host access ended during this turn.".into(),
    ));
    fixture
        .harness
        .request("agent.chat.send", json!({"content":"survey both"}))
        .await
        .unwrap();
    wait_until("first result reached the model", || {
        stub.request_count() == 2
    })
    .await;
    // External Host state changes after discovery and first dispatch, not our UI cancellation path.
    conversations
        .catalog
        .selections
        .lock()
        .unwrap()
        .remove(support::UNKNOWN);
    release.notify_one();
    let answer = wait_chat(
        &mut fixture.harness,
        "refused second dispatch settles",
        |chat| chat["pending"] == false,
    )
    .await;
    assert_eq!(answer["entries"][1]["activity"][1]["ok"], false);
    assert_eq!(
        answer["entries"][1]["activity"][1]["code"],
        "port_not_enabled"
    );
    let result: Value = serde_json::from_str(
        last_message(&stub.requests()[2].body)["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        result,
        json!({"error":"port_not_enabled","retryable":false})
    );
    let calls = fixture.ports.recorded_calls();
    let dispatched: Vec<_> = calls
        .iter()
        .filter(|call| call.method == "invoke")
        .collect();
    assert_eq!(dispatched.len(), 2);
    assert!(
        dispatched
            .iter()
            .all(|call| call.payload["access"] == "agent" && call.payload["major"] == 3)
    );
    assert!(conversations.posts().is_empty());
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transport_post_failure_refuses_identity_change_without_replaying_or_stopping_chat() {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut fixture = start(data.path(), &roster, &conversations).await;
    make_ready(&mut fixture, &stub, 0).await;
    conversations.set_post_mode(PostMode::Transport);
    say(&conversations, &fixture.ports, BOB_KEY, "@Scout transport").await;
    wait_status(&mut fixture.harness, "error", Some("agent.post_failed")).await;
    let refused = fixture
        .harness
        .request("agent.binding.set", json!({"principalId":OTHER_ID}))
        .await;
    assert_eq!(refused.unwrap_err().code(), "agent.binding_uncertain");
    assert_eq!(roster.bound_row()["principalId"], AGENT_ID);
    let answer = chat(&mut fixture.harness, "independent answer").await;
    assert_eq!(answer["entries"][1]["content"], "Hello from Scout");
    assert_eq!(conversations.posts().len(), 1);
    fixture.harness.shutdown().await.unwrap();
}

/// Compact, valid annotation DAG. Width four/seven tiers has reference depth eight and
/// 16,384 leaves (371,422 bytes including the request wrapper), never constructed by this test.
fn schema_preparation_document(
    identity: &str,
    width: usize,
    tiers: usize,
    leaf: Value,
    tools: usize,
) -> Value {
    let mut defs = serde_json::Map::new();
    defs.insert("d0".into(), leaf);
    for tier in 1..=tiers {
        defs.insert(
            format!("d{tier}"),
            json!({"default":vec![json!({"$ref":format!("#/$defs/d{}",tier-1)}) ;width]}),
        );
    }
    let mut ports = vec![
        json!({"name":"runtime.guide","kind":"query","description":"Guide.",
        "request":{"type":"object","additionalProperties":false},
        "response":{"type":"object","additionalProperties":false,"required":["markdown"],"properties":{"markdown":{"type":"string","maxLength":1024}}}}),
    ];
    for tool in 0..tools {
        ports.push(json!({"name":if tool==0 {"sample.read".to_owned()} else {format!("sample.read{tool}")},
            "kind":"query","description":"Read.","request":{"type":"object","additionalProperties":false,"default":{"$ref":format!("#/$defs/d{tiers}")}},"response":{"type":"object","additionalProperties":false}}));
    }
    json!({"schemaVersion":1,"$defs":defs,"contracts":[{"identity":identity,"major":3,"minor":2,"description":"Bounded preparation fixture.","ports":ports}]})
}

async fn schema_preparation_refusal(documents: Vec<Value>) {
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    let mut identities = Vec::new();
    for document in documents {
        let projected = support::project_document(document).await;
        let identity = projected["contracts"][0]["identity"]
            .as_str()
            .unwrap()
            .to_owned();
        conversations
            .catalog
            .documents
            .lock()
            .unwrap()
            .insert(identity.clone(), projected);
        identities.push(identity);
    }
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    for identity in &identities {
        fixture
            .harness
            .request(
                "agent.packages.set",
                json!({"contract":identity,"enabled":true,"major":3}),
            )
            .await
            .unwrap();
    }
    let refused = chat(&mut fixture.harness, "must refuse the entire toolset").await;
    assert_eq!(
        refused["lastError"],
        json!({"code":"agent.tool_unavailable","retryable":false}),
        "{refused}"
    );
    assert_eq!(
        stub.request_count(),
        0,
        "no partial toolset or model request"
    );
    assert!(
        fixture
            .ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "invoke")
    );
    assert!(
        fixture
            .ports
            .recorded_calls()
            .iter()
            .any(|call| call.method == "describe")
    );
    assert!(conversations.posts().is_empty());
    // A preparation refusal neither latches identity uncertainty nor retries the turn.
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":OTHER_ID}))
        .await
        .unwrap();
    for identity in &identities {
        fixture
            .harness
            .request(
                "agent.packages.set",
                json!({"contract":identity,"enabled":false}),
            )
            .await
            .unwrap();
    }
    let supported = chat(&mut fixture.harness, "independent work after local refusal").await;
    assert_eq!(
        supported["entries"].as_array().unwrap().last().unwrap()["content"],
        "Hello from Scout"
    );
    assert_eq!(supported.get("lastError"), None);
    assert_eq!(stub.request_count(), 1, "no automatic retry");
    assert_eq!(offered(&stub.requests()[0].body), None);
    assert!(
        fixture
            .ports
            .recorded_calls()
            .iter()
            .all(|call| call.method != "invoke")
    );
    fixture.harness.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_preparation_refuses_canonical_depth_eight_breadth_before_model_or_dispatch() {
    schema_preparation_refusal(vec![schema_preparation_document(
        support::UNKNOWN,
        4,
        7,
        json!({"type":"string"}),
        1,
    )])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_preparation_refuses_scalar_and_key_bytes_independently_of_node_limit() {
    schema_preparation_refusal(vec![schema_preparation_document(
        support::UNKNOWN,
        2,
        5,
        json!({"type":"string","description":"s".repeat(20000)}),
        1,
    )])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_preparation_refuses_large_keys_before_model_or_dispatch() {
    schema_preparation_refusal(vec![schema_preparation_document(
        support::UNKNOWN,
        2,
        5,
        json!({"default":{"k".repeat(20000):null}}),
        1,
    )])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_preparation_charges_repeated_validator_definition_clones() {
    let mut document = schema_preparation_document(
        support::UNKNOWN,
        1,
        0,
        json!({"type":"string","description":"s".repeat(100000)}),
        12,
    );
    for port in document["contracts"][0]["ports"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .skip(1)
    {
        port["request"]["default"] = Value::Null;
        port["response"] = json!({"$ref":"#/$defs/d0"});
    }
    schema_preparation_refusal(vec![document]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_preparation_shares_work_across_tools_and_contracts() {
    schema_preparation_refusal(vec![
        schema_preparation_document(
            support::UNKNOWN,
            1,
            0,
            json!({"default":vec![Value::Null;2200]}),
            2,
        ),
        schema_preparation_document(
            "com.example.second",
            1,
            0,
            json!({"default":vec![Value::Null;2200]}),
            1,
        ),
    ])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_preparation_preserves_bounded_dag_exact_parameters_and_original_validation() {
    let document = schema_preparation_document(support::UNKNOWN, 2, 3, json!({"type":"string"}), 1);
    let projected = support::project_document(document).await;
    let data = tempfile::tempdir().unwrap();
    let stub = StubEndpoint::start().await;
    let roster = Roster::new();
    let conversations = FakeConversations::new();
    conversations
        .catalog
        .documents
        .lock()
        .unwrap()
        .insert(support::UNKNOWN.into(), projected);
    let mut fixture = start(data.path(), &roster, &conversations).await;
    fixture.harness.activate().await.unwrap();
    configure(&mut fixture.harness, &stub).await;
    fixture
        .harness
        .request("agent.binding.set", json!({"principalId":AGENT_ID}))
        .await
        .unwrap();
    fixture
        .harness
        .request(
            "agent.packages.set",
            json!({"contract":support::UNKNOWN,"enabled":true,"major":3}),
        )
        .await
        .unwrap();
    stub.push(Behaviour::ToolCalls(vec![call(
        "invalid",
        "survey__sample_read",
        r#"{"extra":1}"#,
    )]));
    stub.push(Behaviour::ToolCalls(vec![call(
        "valid",
        "survey__sample_read",
        "{}",
    )]));
    stub.push(Behaviour::Reply("bounded DAG worked".into()));
    let answered = chat(&mut fixture.harness, "use the bounded DAG").await;
    assert_eq!(answered["entries"][1]["content"], "bounded DAG worked");
    let requests = stub.requests();
    let parameters = &requests[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "survey__sample_read")
        .unwrap()["function"]["parameters"];
    let pair = json!({"default":[{"type":"string"},{"type":"string"}]});
    let four = json!({"default":[pair.clone(),pair]});
    let eight = json!({"default":[four.clone(),four]});
    assert_eq!(
        parameters,
        &json!({"type":"object","additionalProperties":false,"default":eight})
    );
    assert_eq!(
        serde_json::from_str::<Value>(last_message(&requests[1].body)["content"].as_str().unwrap())
            .unwrap()["error"],
        "agent.tool_arguments_invalid"
    );
    let calls = fixture.ports.recorded_calls();
    let invokes: Vec<_> = calls
        .iter()
        .filter(|call| call.method == "invoke")
        .collect();
    assert_eq!(invokes.len(), 1);
    assert_eq!(
        invokes[0].payload,
        json!({"access":"agent","contract":support::UNKNOWN,"major":3,"port":"sample.read","kind":"query","request":{}})
    );
    fixture.harness.shutdown().await.unwrap();
}
