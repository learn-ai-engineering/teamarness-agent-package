//! The model client: one non-streaming chat completion per reply against an OpenAI-compatible
//! endpoint.
//!
//! The request is `POST {endpointBaseUrl}/chat/completions` with the key as a bearer token, a
//! 60 s overall timeout and a 256 KiB response cap. Every failure is mapped to one of three
//! Package-owned codes here, so the harness never sees transport detail — and nothing in this
//! module keeps the key, writes a log line or puts anything but a code in an error.

use std::sync::Once;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// The endpoint could not be reached or did not answer in time; a later attempt may succeed.
pub const ENDPOINT_UNREACHABLE: &str = "agent.endpoint_unreachable";
/// The endpoint refused the request (bad key, no access); the configuration must change.
pub const ENDPOINT_REJECTED: &str = "agent.endpoint_rejected";
/// The endpoint answered but not with a usable completion.
pub const MODEL_ERROR: &str = "agent.model_error";

/// How long one completion may take from connect to the last byte.
pub const COMPLETION_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest response body the client reads.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// One message in the completion request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
}

impl ChatMessage {
    #[must_use]
    pub fn system(content: String) -> Self {
        Self {
            role: "system",
            content,
        }
    }

    #[must_use]
    pub fn user(content: String) -> Self {
        Self {
            role: "user",
            content,
        }
    }

    #[must_use]
    pub fn assistant(content: String) -> Self {
        Self {
            role: "assistant",
            content,
        }
    }
}

/// Why a completion failed, as the status shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFailure {
    pub code: &'static str,
    pub retryable: bool,
}

impl ModelFailure {
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
            .timeout(timeout)
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .build()
            .map_err(|_| ModelFailure::model_error())?;
        Ok(Self { client })
    }

    /// Requests one completion and returns the assistant's text.
    ///
    /// `api_key` is used for this one request and dropped with it.
    ///
    /// # Errors
    ///
    /// Returns the Package-owned failure: transport failures and timeouts are
    /// `agent.endpoint_unreachable` (retryable); `401`/`403` are `agent.endpoint_rejected`;
    /// `5xx`, `408` and `429` are `agent.endpoint_unreachable`; any other failure status, an
    /// oversized or malformed body, or a completion without text is `agent.model_error`.
    pub async fn complete(
        &self,
        endpoint_base_url: &str,
        model: &str,
        messages: &[ChatMessage],
        api_key: &str,
    ) -> Result<String, ModelFailure> {
        let url = format!(
            "{}/chat/completions",
            endpoint_base_url.trim_end_matches('/')
        );
        let body = serde_json::to_vec(&CompletionBody {
            model,
            messages,
            stream: false,
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
        let content = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .ok_or_else(ModelFailure::model_error)?;
        Ok(content.to_owned())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

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
    async fn a_silent_endpoint_is_unreachable_after_the_timeout() {
        let base = one_shot(None).await;
        let client = ModelClient::with_timeout(Duration::from_millis(200)).unwrap();
        let outcome = client
            .complete(&base, "m", &[ChatMessage::user("hi".into())], "sk-test")
            .await;
        assert_eq!(
            outcome,
            Err(ModelFailure {
                code: ENDPOINT_UNREACHABLE,
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
            .complete(&base, "m", &[ChatMessage::user("hi".into())], "sk-test")
            .await;
        assert_eq!(outcome.unwrap_err().code, ENDPOINT_UNREACHABLE);
        let outcome = client
            .complete("http://[::1", "m", &[ChatMessage::user("hi".into())], "sk")
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
            .complete(&base, "m", &[ChatMessage::user("hi".into())], "sk-test")
            .await;
        assert_eq!(outcome.unwrap_err().code, MODEL_ERROR);
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
