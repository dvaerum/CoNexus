//! Ollama-backed subject-suggestion helper for `agent_messages`. Port
//! of `conexus/features/message_suggestions.py`.
//!
//! When a sender doesn't supply a `subject` for a root message, this
//! module asks a local Ollama (or any OpenAI `/v1`-compatible)
//! endpoint to produce a one-line summary of the body.
//!
//! ADR-0031: reuses [`crate::completion_client`]'s config surface
//! directly now (`CONEXUS_CHAT_MODEL`/`CONEXUS_LLM_BASE_URL`) via
//! `CONEXUS_SUBJECT_MODEL`, which now defaults to the SAME model as
//! chat completion (`qwen3:1.7b`) rather than requiring its own
//! separately-configured model name -- previously this module
//! deliberately kept its config surface (`CONEXUS_SUBJECT_MODEL`)
//! separate from RAG's (`OPENAI_API_KEY`/`OLLAMA_MODEL`) so a
//! deployment's chat-completion PROVIDER choice couldn't silently
//! change subject-gen's behavior underneath it; now that there is only
//! one provider (no more OpenAI cloud branch) and one shared base URL,
//! that separation no longer buys anything, so subject-gen reuses the
//! already-warm chat model unless overridden.
//!
//! Enablement and model choice are two independent questions now
//! (previously conflated into one: "is `CONEXUS_SUBJECT_MODEL` set at
//! all"). [`subject_gen_enabled`] answers "should this feature run at
//! all" (`CONEXUS_ENABLE_SUBJECT_GEN`, default **on** -- unlike chat/
//! embedding, which are required subsystems, this is the one optional
//! feature of the three, but its cost is now zero extra config to turn
//! on: no model name to type, it just reuses the chat model). The
//! model name itself ([`resolve_subject_model`]) always has a real
//! value, the same way `CONEXUS_CHAT_MODEL`/`CONEXUS_EMBEDDING_MODEL`
//! do -- "unset" is no longer a magic "feature off" sentinel for any
//! of the three model vars.
//!
//! Same "explicit `get_env` lookup, not a hidden `std::env::var` read"
//! discipline as every other external-service module in this crate
//! (parallel-test-safety).

use crate::completion_client::{self, CompletionClient};
use crate::context_window::resolve_subject_input_chars;

const SYSTEM_PROMPT: &str = "Summarize the user's message in 6 words or fewer as an email-style \
     subject line. Return only the subject text, no quotes, no prefix, no punctuation at the \
     end.";
/// Hard ceiling regardless of what the model returns.
const MAX_SUBJECT_LEN: usize = 80;
const MAX_COMPLETION_TOKENS: u32 = 32;
const TEMPERATURE: f64 = 0.2;

fn env_nonempty(get_env: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get_env(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Whether subject-gen should run at all. Default **on** -- explicit
/// falsy values (`0`/`false`/`no`, case-insensitive) turn it off; any
/// other value (including unset) leaves it on. Same recognized-values
/// convention as `CONEXUS_DISABLE_AUTO_INDEXING`
/// (`background_tasks::rag_indexing`), just inverted polarity (this
/// one is an ENABLE flag defaulting on, not a DISABLE flag defaulting
/// off) -- the feature this flag gates is genuinely optional (RAG
/// works fine without a generated subject; the fallback body-preview
/// truncation is a legitimate steady state, not a degraded error),
/// unlike chat/embedding, which have no such flag because they aren't
/// optional.
pub fn subject_gen_enabled(get_env: &impl Fn(&str) -> Option<String>) -> bool {
    !env_nonempty(get_env, "CONEXUS_ENABLE_SUBJECT_GEN")
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
}

/// The model subject-gen calls -- `CONEXUS_SUBJECT_MODEL` if set,
/// else whatever `CONEXUS_CHAT_MODEL` resolves to (reuse, not a
/// separate default) -- always a real value, never "unset means off"
/// (see module doc).
fn resolve_subject_model(get_env: &impl Fn(&str) -> Option<String>) -> String {
    env_nonempty(get_env, "CONEXUS_SUBJECT_MODEL")
        .unwrap_or_else(|| completion_client::resolve(get_env).model)
}

/// Trim whitespace, strip enclosing quotes, collapse internal
/// newlines to spaces, and cap at [`MAX_SUBJECT_LEN`].
fn truncate(subject: &str) -> String {
    let mut out = subject.trim().to_string();
    let bytes_len = out.len();
    if bytes_len >= 2 {
        let starts_quote = out.starts_with('"') || out.starts_with('\'');
        let ends_quote = out.ends_with('"') || out.ends_with('\'');
        if starts_quote && ends_quote {
            out = out[1..out.len() - 1].trim().to_string();
        }
    }
    // Collapse internal whitespace runs (incl. newlines) to a single
    // space -- a subject is a one-liner.
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > MAX_SUBJECT_LEN {
        let truncated: String = out.chars().take(MAX_SUBJECT_LEN - 3).collect();
        out = format!("{}...", truncated.trim_end());
    }
    out
}

/// Ask the configured model for a one-line subject.
///
/// Returns `None` when: subject-gen is disabled
/// ([`subject_gen_enabled`]); the HTTP call fails for any reason; or
/// the model returns an empty/whitespace-only completion. Returns the
/// (trimmed, length-capped) subject string otherwise.
pub async fn suggest_subject(
    get_env: impl Fn(&str) -> Option<String>,
    content: &str,
) -> Option<String> {
    if !subject_gen_enabled(&get_env) {
        return None;
    }
    let model = resolve_subject_model(&get_env);
    let base_url = completion_client::resolve(&get_env).base_url;

    // Head-truncate so the input can never overflow the model's
    // context window -- see context_window's own module doc.
    let max_input_chars = resolve_subject_input_chars(&get_env, Some(&base_url)).await as usize;
    let content: String = if content.chars().count() > max_input_chars {
        content.chars().take(max_input_chars).collect()
    } else {
        content.to_string()
    };

    let client = CompletionClient {
        base_url,
        api_key: "ollama".to_string(),
        model,
    };

    let raw = client
        .chat(
            &[("system", SYSTEM_PROMPT), ("user", &content)],
            TEMPERATURE,
            Some(MAX_COMPLETION_TOKENS),
        )
        .await
        .ok()?;

    let out = truncate(&raw);
    if out.is_empty() {
        None
    } else {
        Some(out)
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
    fn subject_gen_enabled_by_default() {
        assert!(subject_gen_enabled(&env(&[])));
    }

    #[test]
    fn subject_gen_disabled_when_explicitly_turned_off() {
        for falsy in ["0", "false", "FALSE", "no", "No"] {
            assert!(
                !subject_gen_enabled(&env(&[("CONEXUS_ENABLE_SUBJECT_GEN", falsy)])),
                "expected {falsy:?} to disable subject-gen"
            );
        }
    }

    #[test]
    fn subject_gen_stays_enabled_for_any_other_value() {
        assert!(subject_gen_enabled(&env(&[(
            "CONEXUS_ENABLE_SUBJECT_GEN",
            "1"
        )])));
        assert!(subject_gen_enabled(&env(&[(
            "CONEXUS_ENABLE_SUBJECT_GEN",
            "yes"
        )])));
    }

    #[test]
    fn subject_model_defaults_to_the_chat_model_when_unset() {
        assert_eq!(resolve_subject_model(&env(&[])), "qwen3:1.7b");
    }

    #[test]
    fn subject_model_honours_its_own_explicit_override() {
        assert_eq!(
            resolve_subject_model(&env(&[("CONEXUS_SUBJECT_MODEL", "qwen2.5:3b-instruct")])),
            "qwen2.5:3b-instruct"
        );
    }

    #[test]
    fn subject_model_reuses_an_explicit_chat_model_override_when_its_own_is_unset() {
        assert_eq!(
            resolve_subject_model(&env(&[("CONEXUS_CHAT_MODEL", "llama3:8b")])),
            "llama3:8b"
        );
    }

    #[test]
    fn truncate_trims_and_strips_matching_quotes() {
        assert_eq!(truncate("  \"Deploy failed\"  "), "Deploy failed");
    }

    #[test]
    fn truncate_leaves_a_lone_quote_character_alone() {
        // Not a matching pair -- must not eat a real leading quote.
        assert_eq!(truncate("\"unbalanced"), "\"unbalanced");
    }

    #[test]
    fn truncate_collapses_internal_newlines_to_spaces() {
        assert_eq!(truncate("line one\nline two"), "line one line two");
    }

    #[test]
    fn truncate_caps_at_the_max_length_with_an_ellipsis() {
        let long = "a".repeat(200);
        let out = truncate(&long);
        assert_eq!(out.chars().count(), MAX_SUBJECT_LEN);
        assert!(out.ends_with("..."));
    }

    #[tokio::test]
    async fn suggest_subject_is_none_when_disabled() {
        let out = suggest_subject(
            env(&[("CONEXUS_ENABLE_SUBJECT_GEN", "false")]),
            "hello world",
        )
        .await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn suggest_subject_is_none_when_the_endpoint_is_unreachable() {
        // Nothing bound on this port -- connection refused, must
        // degrade to None rather than propagate an error.
        let out = suggest_subject(
            env(&[
                ("CONEXUS_SUBJECT_MODEL", "qwen2.5:3b-instruct"),
                ("CONEXUS_LLM_BASE_URL", "http://127.0.0.1:1/v1"),
            ]),
            "hello world",
        )
        .await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn suggest_subject_calls_the_real_endpoint_and_returns_a_titled_subject() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let body =
                r#"{"choices":[{"message":{"content":"  \"Deploy failed on staging\"  "}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let base_url = format!("http://{addr}/v1");
        let out = suggest_subject(
            env(&[
                ("CONEXUS_SUBJECT_MODEL", "qwen2.5:3b-instruct"),
                ("CONEXUS_LLM_BASE_URL", &base_url),
                // Pins the context window so resolve_subject_input_chars
                // skips its own `/props` HTTP probe entirely -- this
                // fake server only ever accepts ONE connection (for
                // the real /chat/completions call this test is
                // actually exercising), and an unguarded probe would
                // consume that single connection first.
                ("CONEXUS_MODEL_CONTEXT_WINDOW", "4096"),
            ]),
            "the deploy to staging just failed with a timeout",
        )
        .await;
        assert_eq!(out, Some("Deploy failed on staging".to_string()));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn suggest_subject_is_none_on_an_empty_completion() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let body = r#"{"choices":[{"message":{"content":"   "}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let base_url = format!("http://{addr}/v1");
        let out = suggest_subject(
            env(&[
                ("CONEXUS_SUBJECT_MODEL", "qwen2.5:3b-instruct"),
                ("CONEXUS_LLM_BASE_URL", &base_url),
                // See the sibling test above for why this is pinned.
                ("CONEXUS_MODEL_CONTEXT_WINDOW", "4096"),
            ]),
            "hello",
        )
        .await;
        assert_eq!(out, None);
        handle.await.unwrap();
    }
}
