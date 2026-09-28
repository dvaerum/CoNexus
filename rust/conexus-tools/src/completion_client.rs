//! Chat-completion HTTP client, Ollama-shaped (OpenAI-compatible
//! `/v1/chat/completions` wire format -- the same shape Ollama,
//! llama.cpp, and (if ever added) OpenRouter all speak). Port of
//! `conexus/external/completion_service.py`.
//!
//! Same design departures as [`crate::embedding_client`] (see that
//! module's doc for the full rationale): no env-mutation bootstrap to
//! port, [`resolve`] takes an explicit `get_env` lookup rather than
//! reading the process environment (parallel-test-safety), and no
//! per-config client cache (`HTTP_CLIENT` is one process-wide
//! `reqwest::Client`).
//!
//! ADR-0031: the OpenAI CLOUD-PROVIDER branch (`OPENAI_API_KEY`/
//! `OPENAI_MODEL`/`OPENAI_BASE_URL`, `api.openai.com` as a fallback
//! base URL) is REMOVED, not just renamed -- this module now resolves
//! exactly one way, always: `CONEXUS_CHAT_MODEL` (default
//! `qwen3:1.7b`) against `CONEXUS_LLM_BASE_URL` (default the local
//! Ollama address, shared with [`crate::embedding_client`] and
//! [`crate::message_suggestions`] -- one endpoint for every LLM seam,
//! not one per seam). [`resolve`] is consequently infallible now: the
//! only config-error case that existed (`OPENAI_API_KEY` set without
//! `OPENAI_MODEL`) no longer exists because there is no separate
//! model-required cloud branch to misconfigure.

use std::sync::LazyLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";
const OLLAMA_DEFAULT_MODEL: &str = "qwen3:1.7b";

/// `CONEXUS_COMPLETION_CLIENT_TIMEOUT_SECONDS` default. Was 30s, which
/// live-measurement against this deployment's real llama.cpp instance
/// (qwen2.5:3b-instruct, `/slots`-endpoint-confirmed genuine inference,
/// not a hang) showed to be too short: real `ask_project_rag`-shaped
/// completions took 26-67s, so every real query was aborted client-side
/// before the model finished -- `ask_project_rag` was non-functional on
/// this deployment. 120s covers that measured range with headroom;
/// override via env when a deployment's model is slower still.
const DEFAULT_CLIENT_TIMEOUT_SECS: u64 = 120;

/// Resolve the chat-completion HTTP client timeout from an env-lookup
/// function -- same "explicit input over hidden `std::env::var`" style
/// as [`resolve`] (see module doc), kept unit-testable in isolation
/// from the process-wide [`HTTP_CLIENT`] static that consumes it.
/// Unset, empty, or unparseable falls back to
/// [`DEFAULT_CLIENT_TIMEOUT_SECS`] -- a bad env value must never crash
/// client construction.
fn resolve_client_timeout_secs(get_env: impl Fn(&str) -> Option<String>) -> u64 {
    env_nonempty(&get_env, "CONEXUS_COMPLETION_CLIENT_TIMEOUT_SECONDS")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CLIENT_TIMEOUT_SECS)
}

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let timeout_secs = resolve_client_timeout_secs(|key| std::env::var(key).ok());
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .expect("reqwest client with a plain timeout always builds")
});

/// The process-wide chat-completion HTTP client, for callers (e.g.
/// [`crate::message_suggestions`]) that need to hit a DIFFERENT path
/// on the same host than [`CompletionClient::chat`]'s own
/// `/chat/completions` -- reusing one connection pool rather than
/// spinning up a second `reqwest::Client` (and a second, possibly
/// inconsistent, timeout) for what is still logically the same
/// deployment's LLM endpoint.
pub(crate) fn http_client() -> &'static reqwest::Client {
    &HTTP_CLIENT
}

pub(crate) fn env_nonempty(get_env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get_env(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A resolved chat-completion endpoint, ready to call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionClient {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Resolve a [`CompletionClient`] from an env-lookup function.
/// `CONEXUS_CHAT_MODEL` overrides the model (default `qwen3:1.7b`),
/// `CONEXUS_LLM_BASE_URL` overrides the endpoint (default the local
/// Ollama address) -- infallible; there is no longer a config shape
/// that can fail to resolve.
pub fn resolve(get_env: impl Fn(&str) -> Option<String>) -> CompletionClient {
    let model =
        env_nonempty(&get_env, "CONEXUS_CHAT_MODEL").unwrap_or_else(|| OLLAMA_DEFAULT_MODEL.to_string());
    let base_url = env_nonempty(&get_env, "CONEXUS_LLM_BASE_URL")
        .unwrap_or_else(|| OLLAMA_DEFAULT_BASE_URL.to_string());
    CompletionClient {
        base_url,
        api_key: "ollama".to_string(),
        model,
    }
}

/// The one real call site: resolve from the actual process
/// environment. Every test drives [`resolve`] directly instead.
pub fn resolve_from_process_env() -> CompletionClient {
    resolve(|key| std::env::var(key).ok())
}

/// The chat/completion endpoint base URL, for introspection (used by
/// `context_window` to discover the chat model's context window).
/// `None` when `CONEXUS_LLM_BASE_URL` is unset -- a probe with no URL
/// just can't run (matches the pre-ADR-0031 behavior; this function
/// never applied the Ollama default the way [`resolve`] does).
pub fn resolve_chat_base_url(get_env: impl Fn(&str) -> Option<String>) -> Option<String> {
    env_nonempty(&get_env, "CONEXUS_LLM_BASE_URL")
}


#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

/// Error calling the chat-completion endpoint. Deliberately opaque, no
/// transport internals -- same SD-R9-1 discipline as
/// [`crate::embedding_client::EmbedError`].
#[derive(Debug)]
pub struct ChatError(String);

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chat completion request failed: {}", self.0)
    }
}
impl std::error::Error for ChatError {}

impl CompletionClient {
    /// Send a chat-completion request, return the assistant text (or
    /// `""` if the provider's `content` field was null, matching
    /// Python's `content or ""`).
    ///
    /// `max_tokens`, when given, caps the completion length -- needed
    /// by callers with a small, fixed output budget (e.g. a one-line
    /// subject suggestion); `None` leaves the provider's own default.
    ///
    /// `think`, when `Some`, sets Ollama's `"think"` request field --
    /// the switch reasoning models (Qwen3, DeepSeek-R1, ...) use to
    /// skip their hidden chain-of-thought phase. Found live: with a
    /// small `max_tokens` budget, a reasoning model spends the ENTIRE
    /// budget on `reasoning` and never emits real `content` -- silent
    /// empty-string output, not an error, so this went unnoticed until
    /// subject-gen actually started running (ADR-0031 turned it on by
    /// default; previously the whole call path was dead). `None`
    /// omits the field entirely -- callers with a large/no `max_tokens`
    /// budget (RAG's own chat call has room for both phases) are
    /// unaffected either way, so they pass `None` rather than
    /// asserting an opinion this module has no business having for
    /// them.
    pub async fn chat(
        &self,
        messages: &[(&str, &str)],
        temperature: f64,
        max_tokens: Option<u32>,
        think: Option<bool>,
    ) -> Result<String, ChatError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let messages_json: Vec<Value> = messages
            .iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect();
        let mut body = json!({
            "model": self.model,
            "messages": messages_json,
            "temperature": temperature,
        });
        if let Some(max_tokens) = max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(think) = think {
            body["think"] = json!(think);
        }
        let resp = HTTP_CLIENT
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ChatError(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ChatError(format!("HTTP {}", resp.status())));
        }
        let parsed: ChatResponse = resp.json().await.map_err(|e| ChatError(e.to_string()))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ChatError("no choices in response".to_string()))?
            .message
            .content
            .unwrap_or_default();
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn no_env_resolves_to_ollama_defaults() {
        let client = resolve(env(&[]));
        assert_eq!(
            client,
            CompletionClient {
                base_url: OLLAMA_DEFAULT_BASE_URL.to_string(),
                api_key: "ollama".to_string(),
                model: OLLAMA_DEFAULT_MODEL.to_string(),
            }
        );
    }

    #[test]
    fn conexus_chat_model_env_var_overrides_the_default() {
        let client = resolve(env(&[("CONEXUS_CHAT_MODEL", "llama3:8b")]));
        assert_eq!(client.model, "llama3:8b");
    }

    #[test]
    fn conexus_llm_base_url_overrides_the_default_endpoint() {
        let client = resolve(env(&[("CONEXUS_LLM_BASE_URL", "http://fast-igpu:11435/v1")]));
        assert_eq!(client.base_url, "http://fast-igpu:11435/v1");
    }

    // ── resolve_client_timeout_secs ──────────────────────────────────

    #[test]
    fn client_timeout_defaults_to_120_seconds_when_unset() {
        assert_eq!(resolve_client_timeout_secs(env(&[])), 120);
    }

    #[test]
    fn client_timeout_env_var_overrides_the_default() {
        assert_eq!(
            resolve_client_timeout_secs(env(&[(
                "CONEXUS_COMPLETION_CLIENT_TIMEOUT_SECONDS",
                "45"
            )])),
            45
        );
    }

    #[test]
    fn client_timeout_falls_back_to_default_on_unparseable_value() {
        assert_eq!(
            resolve_client_timeout_secs(env(&[(
                "CONEXUS_COMPLETION_CLIENT_TIMEOUT_SECONDS",
                "not-a-number"
            )])),
            120
        );
    }

    #[test]
    fn client_timeout_falls_back_to_default_on_empty_value() {
        assert_eq!(
            resolve_client_timeout_secs(env(&[("CONEXUS_COMPLETION_CLIENT_TIMEOUT_SECONDS", "")])),
            120
        );
    }

    // ── resolve_chat_base_url ────────────────────────────────────────

    #[test]
    fn chat_base_url_is_none_when_nothing_is_set() {
        assert_eq!(resolve_chat_base_url(env(&[])), None);
    }

    #[test]
    fn chat_base_url_is_conexus_llm_base_url_when_set() {
        assert_eq!(
            resolve_chat_base_url(env(&[("CONEXUS_LLM_BASE_URL", "http://a")])),
            Some("http://a".to_string())
        );
    }

    // ── chat() against a real local HTTP server ─────────────────────

    async fn spawn_fake_chat_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{addr}"), handle)
    }

    /// Like [`spawn_fake_chat_server`], but hands the raw received
    /// request bytes back through the returned `JoinHandle` instead of
    /// discarding them -- needed to assert on the request BODY (does
    /// it carry a `"think"` field, and what value) rather than just
    /// the parsed response.
    async fn spawn_fake_chat_server_capturing_request(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            request
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn chat_omits_the_think_field_when_not_specified() {
        let (base_url, handle) = spawn_fake_chat_server_capturing_request(
            200,
            r#"{"choices":[{"message":{"content":"ok"}}]}"#,
        )
        .await;
        let client = CompletionClient {
            base_url,
            api_key: "test".to_string(),
            model: "test-model".to_string(),
        };
        client.chat(&[("user", "hi")], 0.4, None, None).await.unwrap();
        let request = handle.await.unwrap();
        assert!(!request.contains("\"think\""), "request was: {request}");
    }

    #[tokio::test]
    async fn chat_sends_think_false_when_specified() {
        let (base_url, handle) = spawn_fake_chat_server_capturing_request(
            200,
            r#"{"choices":[{"message":{"content":"ok"}}]}"#,
        )
        .await;
        let client = CompletionClient {
            base_url,
            api_key: "test".to_string(),
            model: "test-model".to_string(),
        };
        client
            .chat(&[("user", "hi")], 0.4, Some(32), Some(false))
            .await
            .unwrap();
        let request = handle.await.unwrap();
        assert!(
            request.contains("\"think\":false"),
            "request was: {request}"
        );
    }

    #[tokio::test]
    async fn chat_parses_the_openai_compatible_response_shape() {
        let (base_url, handle) =
            spawn_fake_chat_server(200, r#"{"choices":[{"message":{"content":"the answer"}}]}"#)
                .await;
        let client = CompletionClient {
            base_url,
            api_key: "test".to_string(),
            model: "test-model".to_string(),
        };
        let answer = client.chat(&[("user", "hi")], 0.4, None, None).await.unwrap();
        assert_eq!(answer, "the answer");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn chat_returns_empty_string_for_a_null_content_not_an_error() {
        let (base_url, handle) =
            spawn_fake_chat_server(200, r#"{"choices":[{"message":{"content":null}}]}"#).await;
        let client = CompletionClient {
            base_url,
            api_key: "test".to_string(),
            model: "test-model".to_string(),
        };
        let answer = client.chat(&[("user", "hi")], 0.4, None, None).await.unwrap();
        assert_eq!(answer, "");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn chat_returns_an_error_on_a_non_success_status() {
        let (base_url, handle) = spawn_fake_chat_server(500, r#"{"error":"boom"}"#).await;
        let client = CompletionClient {
            base_url,
            api_key: "test".to_string(),
            model: "test-model".to_string(),
        };
        let err = client.chat(&[("user", "hi")], 0.4, None, None).await.unwrap_err();
        assert!(err.to_string().contains("500"));
        handle.await.unwrap();
    }
}
