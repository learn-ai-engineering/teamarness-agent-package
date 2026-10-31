//! The model client: one non-streaming chat completion per model call against an
//! OpenAI-compatible endpoint, optionally offering tools the model may call.
//!
//! The request is `POST {endpointBaseUrl}/chat/completions` with the key as a bearer token, a
//! 5 min overall timeout and a 256 KiB response cap. Every failure is mapped to one of four
//! Package-owned codes here, so the harness never sees transport detail — and nothing in this
//! module keeps the key, writes a log line or puts anything but a code in an error.
//!
//! With no tools the request body is exactly `model`, `messages` and `stream`. With tools it adds
//! `tools` only: never `tool_choice` or `parallel_tool_calls`, which endpoints disagree about.

use std::sync::Once;
use std::time::Duration;

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::Value;

/// The endpoint could not be reached or answered with a temporary failure; a later attempt may
/// succeed.
pub const ENDPOINT_UNREACHABLE: &str = "agent.endpoint_unreachable";
/// The endpoint did not finish the completion within [`COMPLETION_TIMEOUT`]; a later attempt,
/// or a smaller request, may succeed.
pub const ENDPOINT_TIMED_OUT: &str = "agent.endpoint_timed_out";
/// The endpoint refused the request (bad key, no access); the configuration must change.
pub const ENDPOINT_REJECTED: &str = "agent.endpoint_rejected";
/// The endpoint answered but not with a usable completion.
pub const MODEL_ERROR: &str = "agent.model_error";

/// How long one completion may take from connect to the last byte.
pub const COMPLETION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest response body the client reads.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// One message in the completion request, serialized in the OpenAI shape for its role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    /// Text, tool calls, or both. `content` is `null` when the model only called tools.
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// The result of one tool call, answered by its id.
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl ChatMessage {
    #[must_use]
    pub fn system(content: String) -> Self {
        Self::System { content }
    }

    #[must_use]
    pub fn user(content: String) -> Self {
        Self::User { content }
    }

    #[must_use]
    pub fn assistant(content: String) -> Self {
        Self::Assistant {
            content: Some(content),
            tool_calls: Vec::new(),
        }
    }

    /// The assistant's turn that asked for `tool_calls`, replayed so their results can follow.
    #[must_use]
    pub fn assistant_tool_calls(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant {
            content,
            tool_calls,
        }
    }

    #[must_use]
    pub fn tool(tool_call_id: String, content: String) -> Self {
        Self::Tool {
            tool_call_id,
            content,
        }
    }
}

/// A function the model may call; serialized as `{"type":"function","function":{…}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// The JSON Schema of the call's arguments.
    pub parameters: Value,
}

impl Serialize for ToolDefinition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Function<'a> {
            name: &'a str,
            description: &'a str,
            parameters: &'a Value,
        }
        let mut tool = serializer.serialize_struct("ToolDefinition", 2)?;
        tool.serialize_field("type", "function")?;
        tool.serialize_field(
            "function",
            &Function {
                name: &self.name,
                description: &self.description,
                parameters: &self.parameters,
            },
        )?;
        tool.end()
    }
}

/// One call the model asked for. `arguments` is the model's JSON text, not yet parsed or
/// validated; serialized back as `{"id","type":"function","function":{"name","arguments"}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl Serialize for ToolCall {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Function<'a> {
            name: &'a str,
            arguments: &'a str,
        }
        let mut call = serializer.serialize_struct("ToolCall", 3)?;
        call.serialize_field("id", &self.id)?;
        call.serialize_field("type", "function")?;
        call.serialize_field(
            "function",
            &Function {
                name: &self.name,
                arguments: &self.arguments,
            },
        )?;
        call.end()
    }
}

/// What the model answered: non-empty text, tool calls, or both — never neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assistant {
    /// Trimmed; `None` when the model sent no text.
    pub content: Option<String>,
    /// In the order the model gave them.
    pub tool_calls: Vec<ToolCall>,
}

/// Why a completion failed, as the status shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFailure {
    pub code: &'static str,
    pub retryable: bool,
}

impl ModelFailure {
    const fn timed_out() -> Self {
        Self {
            code: ENDPOINT_TIMED_OUT,
            retryable: true,
        }
    }

    const fn unreachable() -> Self {
        Self {
            code: ENDPOINT_UNREACHABLE,
            retryable: true,
        }
    }

    const fn rejected() -> Self {
        Self {
            code: ENDPOINT_REJECTED,
            retryable: false,
        }
    }

    const fn model_error() -> Self {
        Self {
            code: MODEL_ERROR,
            retryable: false,
        }
    }
}

#[derive(Serialize)]
struct CompletionBody<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "<[ToolDefinition]>::is_empty")]
    tools: &'a [ToolDefinition],
}

/// Installs the `ring` crypto provider once per process. `reqwest` is built with
/// `rustls-no-provider`, the feature the lockfile already resolves, so the process must choose.
fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // A provider installed earlier by this process is as good as ours.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A client for one Package Instance. Endpoint, model and key arrive per request.
#[derive(Clone)]
pub struct ModelClient {
    client: reqwest::Client,
}

impl ModelClient {
    /// A client with the production timeout.
    ///
    /// # Errors
    ///
    /// Returns `agent.model_error` when the TLS stack cannot be built.
    pub fn new() -> Result<Self, ModelFailure> {
        Self::with_timeout(COMPLETION_TIMEOUT)
    }

    /// A client whose overall request timeout is `timeout`; tests shorten it.
    ///
    /// # Errors
    ///
    /// Returns `agent.model_error` when the TLS stack cannot be built.
    pub fn with_timeout(timeout: Duration) -> Result<Self, ModelFailure> {
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .build()
            .map_err(|_| ModelFailure::model_error())?;
        Ok(Self { client })
    }

    /// Requests one completion offering `tools` (none when empty) and returns the assistant's
    /// text and tool calls. With no tools offered, only the text is read.
    ///
    /// `api_key` is used for this one request and dropped with it.
    ///
    /// # Errors
    ///
    /// Returns the Package-owned failure: a timeout is `agent.endpoint_timed_out` (retryable);
    /// other transport failures are `agent.endpoint_unreachable` (retryable); `401`/`403` are `agent.endpoint_rejected`;
    /// `5xx`, `408` and `429` are `agent.endpoint_unreachable`; any other failure status, an
    /// oversized or malformed body, malformed tool calls, or a completion with neither text nor
    /// tool calls is `agent.model_error`.
    pub async fn respond(
        &self,
        endpoint_base_url: &str,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        api_key: &str,
    ) -> Result<Assistant, ModelFailure> {
        let url = format!(
            "{}/chat/completions",
            endpoint_base_url.trim_end_matches('/')
        );
        let body = serde_json::to_vec(&CompletionBody {
            model,
            messages,
            stream: false,
            tools,
        })
        .map_err(|_| ModelFailure::model_error())?;
        let response = self
            .client
            .post(url)
            .bearer_auth(api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(transport_failure)?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_failure(status));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ModelFailure::model_error());
        }
        let bytes = read_capped(response).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| ModelFailure::model_error())?;
        parse_assistant(&value, !tools.is_empty()).ok_or_else(ModelFailure::model_error)
    }
}

/// The first choice's message, or `None` when it is unusable: no text and no tool calls, or a
/// tool call without an id or name or with non-string arguments.
///
/// Without `tools_offered` only the text is read, and any `tool_calls`, well formed or not, are
/// ignored.
fn parse_assistant(value: &Value, tools_offered: bool) -> Option<Assistant> {
    let message = value.get("choices")?.as_array()?.first()?.get("message")?;
    let content = match message.get("content") {
        None | Some(Value::Null) => None,
        Some(content) => Some(content.as_str()?.trim()),
    }
    .filter(|content| !content.is_empty())
    .map(str::to_owned);
    let tool_calls = match message.get("tool_calls") {
        Some(calls) if tools_offered && !calls.is_null() => calls
            .as_array()?
            .iter()
            .map(parse_tool_call)
            .collect::<Option<Vec<_>>>()?,
        _ => Vec::new(),
    };
    if content.is_none() && tool_calls.is_empty() {
        return None;
    }
    Some(Assistant {
        content,
        tool_calls,
    })
}

fn parse_tool_call(call: &Value) -> Option<ToolCall> {
    let non_empty = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let function = call.get("function")?;
    Some(ToolCall {
        id: non_empty(call.get("id"))?,
        name: non_empty(function.get("name"))?,
        arguments: function.get("arguments")?.as_str()?.to_owned(),
    })
}

/// Reads the body up to the cap; a longer body is a model error, not a truncated reply.
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, ModelFailure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_failure)? {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ModelFailure::model_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn transport_failure(error: reqwest::Error) -> ModelFailure {
    if error.is_builder() {
        // The configured endpoint cannot be turned into a request; no retry will change that.
        ModelFailure::rejected()
    } else if error.is_timeout() {
        ModelFailure::timed_out()
    } else {
        ModelFailure::unreachable()
    }
}

fn status_failure(status: reqwest::StatusCode) -> ModelFailure {
    match status.as_u16() {
        401 | 403 => ModelFailure::rejected(),
        408 | 429 | 500..=599 => ModelFailure::unreachable(),
        _ => ModelFailure::model_error(),
    }
}

/// The configured endpoint, model and key as the turn engine's [`crate::turn::Model`].
///
/// Holds the key for one turn; it is dropped with this value and never logged, so this type has
/// no `Debug`.
pub struct ConfiguredModel {
    client: ModelClient,
    endpoint_base_url: String,
    model: String,
    api_key: String,
}

impl ConfiguredModel {
    #[must_use]
    pub fn new(
        client: ModelClient,
        endpoint_base_url: String,
        model: String,
        api_key: String,
    ) -> Self {
        Self {
            client,
            endpoint_base_url,
            model,
            api_key,
        }
    }
}

impl crate::turn::Model for ConfiguredModel {
    fn respond(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
    ) -> impl Future<Output = Result<Assistant, ModelFailure>> + Send {
        self.client.respond(
            &self.endpoint_base_url,
            &self.model,
            messages,
            tools,
            &self.api_key,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = socket.read(&mut chunk).await.unwrap();
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let line = line.to_ascii_lowercase();
                        line.strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        request
    }

    /// Answers one connection with `response` after reading its request, or holds it open.
    async fn one_shot(response: Option<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 65536];
            let _ = socket.read(&mut buffer).await;
            match response {
                Some(response) => {
                    tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                        .await
                        .unwrap();
                }
                None => {
                    // simulated-delay: the endpoint never answers; the client's timeout is the
                    // subject of the test.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        });
        base
    }

    #[tokio::test]
    async fn redirects_never_forward_completion_context() {
        for status in [307, 308] {
            let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target_addr = target.local_addr().unwrap();
            let target_task = tokio::spawn(async move {
                match target.accept().await {
                    Ok((mut socket, _)) => {
                        let _ = read_http_request(&mut socket).await;
                        let body = r#"{"choices":[{"message":{"content":"redirected"}}]}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                            .await
                            .unwrap();
                        true
                    }
                    Err(_) => false,
                }
            });

            let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let source_addr = source.local_addr().unwrap();
            let source_task = tokio::spawn(async move {
                let (mut socket, _) = source.accept().await.unwrap();
                let request = read_http_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: http://{target_addr}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                    .await
                    .unwrap();
                String::from_utf8_lossy(&request).into_owned()
            });

            let client = ModelClient::with_timeout(Duration::from_secs(2)).unwrap();
            let outcome = client
                .respond(
                    &format!("http://{source_addr}"),
                    "m",
                    &[ChatMessage::user("private transcript sentinel".into())],
                    &[],
                    "sk-test",
                )
                .await;
            let sent_request = source_task.await.unwrap();
            assert!(sent_request.contains("private transcript sentinel"));
            assert_eq!(outcome.unwrap_err().code, MODEL_ERROR);
            assert!(
                tokio::time::timeout(Duration::from_millis(250), target_task)
                    .await
                    .is_err(),
                "redirect target received the transcript"
            );
        }
    }

    #[tokio::test]
    async fn configured_loopback_http_completion_still_succeeds() {
        let base = one_shot(Some(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 45\r\nConnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"hello\"}}]}",
        ))
        .await;
        let client = ModelClient::with_timeout(Duration::from_secs(2)).unwrap();
        let result = client
            .respond(
                &base,
                "m",
                &[ChatMessage::user("hi".into())],
                &[],
                "sk-test",
            )
            .await;
        assert_eq!(result.unwrap().content.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn a_silent_endpoint_times_out() {
        let base = one_shot(None).await;
        let client = ModelClient::with_timeout(Duration::from_millis(200)).unwrap();
        let outcome = client
            .respond(
                &base,
                "m",
                &[ChatMessage::user("hi".into())],
                &[],
                "sk-test",
            )
            .await;
        assert_eq!(
            outcome,
            Err(ModelFailure {
                code: ENDPOINT_TIMED_OUT,
                retryable: true
            })
        );
    }

    #[tokio::test]
    async fn a_closed_port_is_unreachable_and_a_bad_url_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = ModelClient::with_timeout(Duration::from_secs(5)).unwrap();
        let outcome = client
            .respond(
                &base,
                "m",
                &[ChatMessage::user("hi".into())],
                &[],
                "sk-test",
            )
            .await;
        assert_eq!(outcome.unwrap_err().code, ENDPOINT_UNREACHABLE);
        let outcome = client
            .respond(
                "http://[::1",
                "m",
                &[ChatMessage::user("hi".into())],
                &[],
                "sk",
            )
            .await;
        assert_eq!(outcome.unwrap_err().code, ENDPOINT_REJECTED);
    }

    #[tokio::test]
    async fn an_oversized_body_is_a_model_error() {
        let base = one_shot(Some(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 300000\r\nConnection: close\r\n\r\n{}",
        ))
        .await;
        let client = ModelClient::with_timeout(Duration::from_secs(5)).unwrap();
        let outcome = client
            .respond(
                &base,
                "m",
                &[ChatMessage::user("hi".into())],
                &[],
                "sk-test",
            )
            .await;
        assert_eq!(outcome.unwrap_err().code, MODEL_ERROR);
    }

    /// Answers one connection with `body` as a `200` JSON response and hands back the request
    /// body the client sent.
    async fn capturing(body: String) -> (String, tokio::task::JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                .await
                .unwrap();
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            serde_json::from_slice(&request[header_end + 4..]).unwrap()
        });
        (base, task)
    }

    fn lookup_tool() -> ToolDefinition {
        ToolDefinition {
            name: "exhibits__exhibit_list".into(),
            description: "Lists exhibits.".into(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
        }
    }

    #[tokio::test]
    async fn no_tools_body_is_unchanged() {
        let client = ModelClient::with_timeout(Duration::from_secs(2)).unwrap();
        let history = [
            ChatMessage::system("rules".into()),
            ChatMessage::user("hi".into()),
            ChatMessage::assistant("hello".into()),
        ];
        let expected = serde_json::json!({
            "model": "m",
            "messages": [
                {"role":"system","content":"rules"},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello"},
            ],
            "stream": false,
        });

        let (base, request) =
            capturing(r#"{"choices":[{"message":{"content":"ok"}}]}"#.into()).await;
        let answer = client.respond(&base, "m", &history, &[], "sk").await;
        assert_eq!(answer.unwrap().content.as_deref(), Some("ok"));
        assert_eq!(request.await.unwrap(), expected);

        let (base, request) =
            capturing(r#"{"choices":[{"message":{"content":" ok "}}]}"#.into()).await;
        let answer = client.respond(&base, "m", &history, &[], "sk").await;
        assert_eq!(
            answer.unwrap(),
            Assistant {
                content: Some("ok".into()),
                tool_calls: Vec::new(),
            }
        );
        assert_eq!(request.await.unwrap(), expected);
    }

    #[tokio::test]
    async fn tools_are_sent_and_tool_calls_parsed() {
        let client = ModelClient::with_timeout(Duration::from_secs(2)).unwrap();
        let earlier = ToolCall {
            id: "call_0".into(),
            name: "exhibits__runtime_guide".into(),
            arguments: "{}".into(),
        };
        let history = [
            ChatMessage::user("what exhibits?".into()),
            ChatMessage::assistant_tool_calls(None, vec![earlier]),
            ChatMessage::tool("call_0".into(), "guide text".into()),
        ];
        let (base, request) = capturing(
            r#"{"choices":[{"message":{"content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"exhibits__exhibit_list","arguments":"{\"limit\":5}"}},
                {"id":"call_2","type":"function","function":{"name":"exhibits__runtime_guide","arguments":"{}"}}
            ]}}]}"#
                .into(),
        )
        .await;
        let answer = client
            .respond(&base, "m", &history, &[lookup_tool()], "sk")
            .await
            .unwrap();
        assert_eq!(
            answer,
            Assistant {
                content: None,
                tool_calls: vec![
                    ToolCall {
                        id: "call_1".into(),
                        name: "exhibits__exhibit_list".into(),
                        arguments: r#"{"limit":5}"#.into(),
                    },
                    ToolCall {
                        id: "call_2".into(),
                        name: "exhibits__runtime_guide".into(),
                        arguments: "{}".into(),
                    },
                ],
            }
        );
        let body = request.await.unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "model": "m",
                "messages": [
                    {"role":"user","content":"what exhibits?"},
                    {"role":"assistant","content":null,"tool_calls":[
                        {"id":"call_0","type":"function","function":{"name":"exhibits__runtime_guide","arguments":"{}"}}
                    ]},
                    {"role":"tool","tool_call_id":"call_0","content":"guide text"},
                ],
                "stream": false,
                "tools": [{"type":"function","function":{
                    "name":"exhibits__exhibit_list",
                    "description":"Lists exhibits.",
                    "parameters":{"type":"object","properties":{}}
                }}],
            })
        );
    }

    #[tokio::test]
    async fn empty_or_malformed_tool_calls_are_model_errors() {
        let client = ModelClient::with_timeout(Duration::from_secs(2)).unwrap();
        let bodies = [
            r#"{"choices":[{"message":{"content":"  "}}]}"#,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[]}}]}"#,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"type":"function","function":{"name":"t","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c","type":"function","function":{"arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"t","arguments":{}}}]}}]}"#,
            r#"{"choices":[{"message":{"content":"text","tool_calls":{"id":"c"}}}]}"#,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"","type":"function","function":{"name":"t","arguments":"{}"}}]}}]}"#,
        ];
        for body in bodies {
            let (base, request) = capturing(body.into()).await;
            let outcome = client
                .respond(
                    &base,
                    "m",
                    &[ChatMessage::user("hi".into())],
                    &[lookup_tool()],
                    "sk",
                )
                .await;
            request.await.unwrap();
            assert_eq!(outcome.unwrap_err().code, MODEL_ERROR, "{body}");
        }
    }

    #[tokio::test]
    async fn tool_calls_ignored_when_no_tools_offered() {
        let client = ModelClient::with_timeout(Duration::from_secs(2)).unwrap();
        let answered = [
            r#"{"choices":[{"message":{"content":"ok","tool_calls":[{"id":"c","type":"function","function":{"name":"t","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"message":{"content":" ok ","tool_calls":[{"id":"","function":{"arguments":{}}}]}}]}"#,
            r#"{"choices":[{"message":{"content":"ok","tool_calls":{"id":"c"}}}]}"#,
        ];
        for body in answered {
            let (base, request) = capturing(body.into()).await;
            let answer = client
                .respond(&base, "m", &[ChatMessage::user("hi".into())], &[], "sk")
                .await;
            request.await.unwrap();
            assert_eq!(
                answer.unwrap(),
                Assistant {
                    content: Some("ok".into()),
                    tool_calls: Vec::new(),
                },
                "{body}"
            );
        }

        // Without usable text the answer is still a model error, whatever the tool calls say.
        let unusable = [
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"t","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"message":{"content":"  ","tool_calls":[{"id":"c"}]}}]}"#,
        ];
        for body in unusable {
            let (base, request) = capturing(body.into()).await;
            let outcome = client
                .respond(&base, "m", &[ChatMessage::user("hi".into())], &[], "sk")
                .await;
            request.await.unwrap();
            assert_eq!(outcome.unwrap_err().code, MODEL_ERROR, "{body}");
        }
    }

    #[test]
    fn status_mapping_is_the_documented_one() {
        use reqwest::StatusCode;
        assert_eq!(
            status_failure(StatusCode::UNAUTHORIZED).code,
            ENDPOINT_REJECTED
        );
        assert_eq!(
            status_failure(StatusCode::FORBIDDEN).code,
            ENDPOINT_REJECTED
        );
        assert!(!status_failure(StatusCode::FORBIDDEN).retryable);
        assert_eq!(
            status_failure(StatusCode::BAD_GATEWAY).code,
            ENDPOINT_UNREACHABLE
        );
        assert!(status_failure(StatusCode::BAD_GATEWAY).retryable);
        assert_eq!(
            status_failure(StatusCode::TOO_MANY_REQUESTS).code,
            ENDPOINT_UNREACHABLE
        );
        assert_eq!(status_failure(StatusCode::BAD_REQUEST).code, MODEL_ERROR);
        assert_eq!(status_failure(StatusCode::NOT_FOUND).code, MODEL_ERROR);
    }
}
