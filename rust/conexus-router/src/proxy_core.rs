//! The reverse-proxy body -- port of `conexus/router/app.py::
//! _proxy_to_backend` (Phase E2 PR 8). A dedicated research pass
//! flagged this as "the single highest-risk trap for the whole
//! phase" because of the R7-F3 body-buffer invariant: the full
//! request body MUST be materialized before any forwarding-header
//! injection or upstream connect, closing a real, previously-
//! exploited timing vulnerability (a slow-drip caller holding the
//! body-read open across a concurrent role demotion, then having the
//! STALE pre-demotion role forwarded and trusted for the header's
//! full TTL).
//!
//! **Scope, deliberately narrower than the full Python function**:
//! Python's `_proxy_to_backend` takes an `inject_header_resolver`
//! parameter -- an async CLOSURE, re-invoked AFTER the body-read yield
//! point, so a slow-drip caller's pre-demotion role can't survive a
//! concurrent revocation for the header's full TTL (the R7-F3 fix this
//! module's own doc discusses above). This port takes a plain
//! `forwarding_header_value: Option<&str>` instead -- an ALREADY-signed
//! value, not a resolver -- because [`ProxyRequest::body`] being a
//! structurally pre-buffered `Bytes` (this module's own R7-F3
//! invariant) means there is no yield point on attacker-controlled I/O
//! between resolving the header and attaching it in the first place;
//! the closure-based re-check Python needs to close that window has
//! nothing to protect against here. `inject_bearer` is dropped
//! entirely -- Python's own docstring already states it has no
//! production caller.
//!
//! **The R7-F3 invariant is structural here, not conventional**:
//! [`ProxyRequest::body`] is a plain `Bytes` -- there is no code path
//! by which this function could receive an unbuffered/streaming
//! request body; the caller (an axum extractor, in the eventual
//! app-wiring PR) must already have materialized it before
//! constructing a `ProxyRequest` at all. This mirrors PR7's own
//! `proxy_client::UdsRequestBody = Full<Bytes>` choice for exactly
//! the same reason.

use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request};
use sha2::{Digest, Sha256};

use crate::orchestrator::ensure::{ensure, EnsureConfig, EnsureError};
use crate::orchestrator::runtime::RuntimeStore;
use crate::project_registry::ProjectRegistry;
use crate::proxy_client::{self, UdsClientError};

/// The `(alias_name, expires_at)` pair Python calls `alias_info` --
/// present when the inbound request arrived on an alias URL, so the
/// backend can later render a deprecation warning (Phase 1c).
#[derive(Debug, Clone)]
pub struct AliasInfo {
    pub name: String,
    pub expires_at: String,
}

/// The header name the router alone is authoritative for signing --
/// stripped from every inbound request unconditionally (see
/// [`filter_headers`]).
///
/// Lowercase twin of [`conexus_auth::forwarding_header::HEADER_NAME`]
/// (this crate needs the lowercase wire form for `HeaderName::
/// from_static`/header-map comparison; that crate needs the
/// mixed-case display form for `sign`/`verify`'s wire format) -- a
/// `#[cfg(test)]` assertion below keeps the two from silently
/// re-diverging the way `X-CoNexus-Forwarded-Operator` vs.
/// `x-conexus-forwarded-operator` once did.
pub const FORWARDING_HEADER_NAME: &str = "x-conexus-forwarded-operator";

const HOP_BY_HOP_HEADERS: [&str; 6] = [
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Port of `_is_hop_by_hop_header` -- `transfer-encoding` is the
/// load-bearing entry: aiohttp's (and hyper's) client derives
/// `Content-Length` from the already-materialized body, so a
/// forwarded chunked `Transfer-Encoding` collides and the backend
/// hard-4xxs.
pub fn is_hop_by_hop_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lower.as_str()) || lower.starts_with("proxy-")
}

/// Port of the header-filter step of `_proxy_to_backend`: strip
/// `host`, `content-length`, the forwarding-header name, and every
/// hop-by-hop header -- the router is the ONLY authoritative source
/// of the forwarding header, so it's removed unconditionally
/// regardless of whether this call ever re-attaches one.
pub fn filter_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "host"
            || lower == "content-length"
            || lower == FORWARDING_HEADER_NAME
            || is_hop_by_hop_header(&lower)
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// The three exact GET paths that are unconditionally treated as
/// streaming BEFORE the upstream even opens -- port of
/// `is_stream_request`'s pre-check.
pub fn is_stream_request(method: &Method, path: &str) -> bool {
    method == Method::GET && matches!(path, "/mcp" | "/api/delivery/stream" | "/api/events")
}

/// Port of `_sse_agent_key`, PLUS the cookie-authenticated
/// (`op:`-prefixed) branch this function's own doc used to say was
/// deferred (F11 fix): a bearer token hashes to a stable per-caller
/// key; failing that, the router's OWN signed forwarding header
/// (`X-Conexus-Forwarded-Operator`) yields a stable per-OPERATOR key;
/// only a caller with NEITHER credential at all falls back to the
/// shared `"anon"` bucket.
///
/// **Before this fix**: every cookie-authenticated dashboard operator,
/// across EVERY project, shared the SAME `"anon"` bucket (this
/// function only ever inspected `Authorization`, which a cookie-
/// authenticated request never carries) -- confirmed live, a brand
/// new unprivileged user's very first SSE stream got a `429` purely
/// because a DIFFERENT operator's streams on a DIFFERENT project had
/// already filled the shared 4-slot cap. See
/// `sse_agent_key_isolates_cookie_authenticated_operators_by_identity`
/// below.
///
/// **Call-site precondition this relies on**: [`proxy_to_backend`]
/// calls this function on its OWN `headers` value AFTER it has
/// already attached the freshly-signed forwarding header (see that
/// function's body) -- so the header is already present here whenever
/// the caller came in through the cookie-forwarding bridge, no extra
/// parameter needs threading through.
///
/// Parsing `operator_id` out of the header WITHOUT re-verifying its
/// HMAC is safe here, not a shortcut: [`filter_headers`] unconditionally
/// strips any CLIENT-supplied header of this exact name before this
/// point (see that function's own doc) -- the ONLY value this map can
/// ever carry by construction is one THIS router process just signed,
/// moments ago, for the current request's own resolved identity. There
/// is no attacker-controlled input on this path to spoof; verifying
/// the MAC again here would just re-check a fact this process itself
/// already established.
pub fn sse_agent_key(headers: &HeaderMap) -> String {
    if let Some(auth) = headers.get(hyper::header::AUTHORIZATION) {
        if let Ok(s) = auth.to_str() {
            let digest = Sha256::digest(s.as_bytes());
            let hex = format!("{digest:x}");
            return hex[..16].to_string();
        }
    }
    if let Some(fwd) = headers.get(FORWARDING_HEADER_NAME) {
        if let Ok(s) = fwd.to_str() {
            // Wire format is `"<operator_id>.<role>.<expiry>.<mac>"`
            // (see `conexus_auth::forwarding_header`'s own doc) --
            // `operator_id` is everything before the first '.'.
            if let Some((operator_id, _rest)) = s.split_once('.') {
                if !operator_id.is_empty() {
                    let digest = Sha256::digest(operator_id.as_bytes());
                    let hex = format!("{digest:x}");
                    return format!("op:{}", &hex[..16]);
                }
            }
        }
    }
    "anon".to_string()
}

/// Per-agent / global concurrent-SSE admission control -- port of
/// `project_orchestrator._track_streaming_proxy`/`MAX_STREAMS_*`.
/// Deliberately its own type here rather than folded into
/// `RuntimeStore`: this is proxy-CONNECTION admission, not backend
/// LIFECYCLE state (the distinction PR 6's own research drew when it
/// deferred this exact mechanism to "whichever PR ports
/// `_proxy_to_backend`" -- this one).
pub struct StreamCapRegistry {
    per_agent: Mutex<std::collections::HashMap<String, u32>>,
    global: AtomicU32,
    max_per_agent: u32,
    max_global: u32,
}

impl StreamCapRegistry {
    pub fn new(max_per_agent: u32, max_global: u32) -> Self {
        Self {
            per_agent: Mutex::new(std::collections::HashMap::new()),
            global: AtomicU32::new(0),
            max_per_agent,
            max_global,
        }
    }

    /// Try to admit one more stream for `agent_key`. Returns a guard
    /// that releases the slot on drop, or `None` if either cap is
    /// already saturated -- checked and incremented atomically w.r.t.
    /// other callers via the per-agent map's own mutex (the global
    /// counter is only ever incremented alongside a successful
    /// per-agent admission, so the mutex covers both).
    pub fn try_admit(self: &Arc<Self>, agent_key: &str) -> Option<StreamCapGuard> {
        let mut per_agent = self.per_agent.lock().expect("stream cap mutex poisoned");
        let current_global = self.global.load(Ordering::SeqCst);
        let current_for_agent = *per_agent.get(agent_key).unwrap_or(&0);
        if current_global >= self.max_global || current_for_agent >= self.max_per_agent {
            return None;
        }
        *per_agent.entry(agent_key.to_string()).or_insert(0) += 1;
        self.global.fetch_add(1, Ordering::SeqCst);
        Some(StreamCapGuard {
            registry: self.clone(),
            agent_key: agent_key.to_string(),
        })
    }

    fn release(&self, agent_key: &str) {
        let mut per_agent = self.per_agent.lock().expect("stream cap mutex poisoned");
        if let Some(count) = per_agent.get_mut(agent_key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                per_agent.remove(agent_key);
            }
        }
        self.global.fetch_sub(1, Ordering::SeqCst);
    }
}

/// RAII admission slot -- releases both counters on drop, however the
/// holding stream ends (fully consumed, dropped early by the caller,
/// or the connection errors out).
pub struct StreamCapGuard {
    registry: Arc<StreamCapRegistry>,
    agent_key: String,
}

impl Drop for StreamCapGuard {
    fn drop(&mut self) {
        self.registry.release(&self.agent_key);
    }
}

/// The inbound request this module proxies -- `body` is ALREADY
/// materialized (see the module doc's R7-F3 note).
pub struct ProxyRequest {
    pub method: Method,
    /// Path + query string, exactly as the backend expects it (the
    /// caller translates the router's own URL shape into the
    /// backend's beforehand -- same division of labor as Python's
    /// `backend_path` parameter).
    pub path_and_query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Debug)]
pub enum ProxyError {
    /// Port of `_client_gone_response` (499) -- reserved for the
    /// eventual caller that reads the inbound body itself (this
    /// function receives an already-buffered body, so it can't
    /// observe a mid-upload disconnect directly; kept as a variant so
    /// the app-wiring PR's error mapping has a home for it).
    ///
    /// Confirmed still unconstructed, not masked, while removing this
    /// module's stale `#![allow(dead_code)]` during a docs audit; see
    /// git blame.
    #[allow(dead_code)]
    ClientGone,
    /// Port of the socket-poll-timeout/systemctl-failure surface via
    /// `ensure()`.
    Ensure(EnsureError),
    Registry(crate::project_registry::RegistryError),
    /// Port of the `ClientConnectorError` branch (502) -- the backend
    /// vanished between `ensure()` succeeding and the connect actually
    /// landing (a concurrent stop/delete/rename, or a crash).
    BackendUnavailable(std::io::Error),
    UdsClient(UdsClientError),
    /// Over the per-agent/global concurrent-SSE cap (429).
    TooManyStreams,
    /// A genuine, unexpected error reading the upstream's own
    /// streamed body frames mid-flight.
    UpstreamStream(hyper::Error),
    Http(hyper::http::Error),
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyError::ClientGone => write!(f, "client disconnected"),
            ProxyError::Ensure(e) => write!(f, "{e}"),
            ProxyError::Registry(e) => write!(f, "{e}"),
            ProxyError::BackendUnavailable(e) => write!(f, "backend unavailable: {e}"),
            ProxyError::UdsClient(e) => write!(f, "{e}"),
            ProxyError::TooManyStreams => write!(f, "too many concurrent streams"),
            ProxyError::UpstreamStream(e) => write!(f, "upstream stream error: {e}"),
            ProxyError::Http(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProxyError {}

impl From<EnsureError> for ProxyError {
    fn from(e: EnsureError) -> Self {
        ProxyError::Ensure(e)
    }
}

impl From<crate::project_registry::RegistryError> for ProxyError {
    fn from(e: crate::project_registry::RegistryError) -> Self {
        ProxyError::Registry(e)
    }
}

impl From<hyper::http::Error> for ProxyError {
    fn from(e: hyper::http::Error) -> Self {
        ProxyError::Http(e)
    }
}

/// Either a fully-buffered response body (the common case -- a plain
/// JSON-RPC/REST call) or a live byte stream (an SSE hold) -- port of
/// the buffered-vs-`_stream_upstream_to_client` fork.
pub enum ProxyResponseBody {
    Buffered(Bytes),
    Streaming(Pin<Box<dyn Stream<Item = Result<Bytes, ProxyError>> + Send>>),
}

// Manual impls -- a boxed `dyn Stream` has no `Debug` of its own, but
// `Result::unwrap_err()` (used throughout this module's own tests)
// requires the `Ok` side to be `Debug` too.
impl std::fmt::Debug for ProxyResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyResponseBody::Buffered(b) => f.debug_tuple("Buffered").field(b).finish(),
            ProxyResponseBody::Streaming(_) => f.write_str("Streaming(..)"),
        }
    }
}

#[derive(Debug)]
pub struct ProxyResponse {
    pub status: u16,
    /// `(name, value)` pairs, already stripped of `transfer-encoding`/
    /// `content-length` (the caller sets these fresh based on which
    /// [`ProxyResponseBody`] variant it ends up serializing).
    pub headers: Vec<(String, String)>,
    pub body: ProxyResponseBody,
}

/// Proxy `req` to the backend for `name`, asking it for `req.path_and_
/// query`. Faithful port of `_proxy_to_backend`'s bearer-path control
/// flow -- see the module doc for the scope this deliberately omits.
#[allow(clippy::too_many_arguments)]
pub async fn proxy_to_backend(
    store: &RuntimeStore,
    stream_caps: &Arc<StreamCapRegistry>,
    registry: &ProjectRegistry,
    sock_dir: &Path,
    name: &str,
    ensure_cfg: &EnsureConfig,
    req: ProxyRequest,
    alias_info: Option<&AliasInfo>,
    forwarding_header_value: Option<&str>,
) -> Result<ProxyResponse, ProxyError> {
    let sock = ensure(store, registry, sock_dir, name, "backend", ensure_cfg).await?;

    let mut headers = filter_headers(&req.headers);
    // The caller's own resolver already proved membership and signed
    // this value (see `cookie_forwarding::resolve_cookie_project_role`
    // + `mcp_handler::backend_api_handler`) -- `filter_headers` just
    // stripped any CLIENT-supplied header of the same name above, so
    // this is the only place a forwarding header can end up on the
    // outbound request, exactly as intended (the router alone is
    // authoritative for signing it).
    if let Some(value) = forwarding_header_value {
        if let Ok(header_value) = HeaderValue::from_str(value) {
            headers.insert(
                HeaderName::from_static(FORWARDING_HEADER_NAME),
                header_value,
            );
        }
    }
    // Python's `_proxy_to_backend` builds the outbound aiohttp request
    // against `f"http://localhost{backend_path}"`, so aiohttp
    // implicitly synthesizes `Host: localhost` for the backend leg
    // even though the CLIENT's real Host was stripped just above.
    // This hyper-based port builds a bare-path `Request` with no
    // authority to derive a Host from, so it must be set explicitly
    // -- omitting it entirely made `conexus-backend`'s rmcp layer
    // reject every request via its own DNS-rebinding-protection check
    // (rmcp's `StreamableHttpService` hard-requires `Host` on HTTP/1.1).
    headers.insert(
        HeaderName::from_static("host"),
        HeaderValue::from_static("localhost"),
    );
    if let Some(alias) = alias_info {
        headers.insert(
            HeaderName::from_static("x-conexus-alias"),
            HeaderValue::from_str(&format!("{},{}", alias.name, alias.expires_at))
                .expect("alias name/expires_at are always valid header-value bytes"),
        );
    }

    let stream_request = is_stream_request(&req.method, &req.path_and_query);
    let agent_key = sse_agent_key(&headers);
    let mut guard = if stream_request {
        match stream_caps.try_admit(&agent_key) {
            Some(g) => Some(g),
            None => return Err(ProxyError::TooManyStreams),
        }
    } else {
        None
    };

    let mut builder = Request::builder()
        .method(req.method)
        .uri(req.path_and_query.clone());
    for (k, v) in headers.iter() {
        builder = builder.header(k, v);
    }
    let hyper_req = builder.body(Full::new(req.body))?;

    let upstream = match proxy_client::send(&sock, hyper_req).await {
        Ok(resp) => resp,
        Err(UdsClientError::Connect(e)) => return Err(ProxyError::BackendUnavailable(e)),
        Err(e) => return Err(ProxyError::UdsClient(e)),
    };

    store.with_runtime_mut(name, |rt| {
        rt.last_active
            .insert("backend".to_string(), SystemTime::now());
    });

    let ctype = upstream
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_streaming = stream_request || ctype.starts_with("text/event-stream");

    let out_headers: Vec<(String, String)> = upstream
        .headers()
        .iter()
        .filter(|(k, _)| {
            let lower = k.as_str().to_ascii_lowercase();
            lower != "transfer-encoding" && lower != "content-length"
        })
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let status = upstream.status().as_u16();

    if is_streaming {
        if guard.is_none() {
            // R4-F2: a POST that turned out streaming (e.g.
            // wait_for_events' uncapped heartbeat hold) retroactively
            // acquires the SAME cap, now keyed off the just-opened
            // upstream -- left uncapped here, MAX_STREAMS_GLOBAL is
            // unenforced for the common real-agent modality.
            guard = match stream_caps.try_admit(&agent_key) {
                Some(g) => Some(g),
                None => return Err(ProxyError::TooManyStreams),
            };
        }
        let byte_stream = incoming_to_byte_stream(upstream.into_body());
        let guarded = GuardedStream {
            inner: byte_stream,
            _guard: guard,
        };
        Ok(ProxyResponse {
            status,
            headers: out_headers,
            body: ProxyResponseBody::Streaming(Box::pin(guarded)),
        })
    } else {
        let body = upstream
            .into_body()
            .collect()
            .await
            .map_err(ProxyError::UpstreamStream)?
            .to_bytes();
        Ok(ProxyResponse {
            status,
            headers: out_headers,
            body: ProxyResponseBody::Buffered(body),
        })
    }
}

/// Adapt `hyper::body::Incoming`'s frame stream into a plain
/// `Stream<Item = Result<Bytes, ProxyError>>` -- trailer frames (SSE
/// never sends any) are silently dropped, matching Python's own
/// `up.content.iter_any()` (a pure byte-chunk iterator with no
/// trailer concept at all).
fn incoming_to_byte_stream(
    incoming: hyper::body::Incoming,
) -> impl Stream<Item = Result<Bytes, ProxyError>> + Send + Unpin {
    Box::pin(
        http_body_util::BodyStream::new(incoming).filter_map(|frame_result| async move {
            match frame_result {
                Ok(frame) => frame.into_data().ok().map(Ok),
                Err(e) => Some(Err(ProxyError::UpstreamStream(e))),
            }
        }),
    )
}

/// Keeps a [`StreamCapGuard`] alive for exactly as long as the wrapped
/// stream is -- released on drop regardless of whether the stream ran
/// to completion or was abandoned early (the caller's connection
/// dropped, the router shut down mid-stream, ...).
struct GuardedStream<S> {
    inner: S,
    _guard: Option<StreamCapGuard>,
}

impl<S: Stream + Unpin> Stream for GuardedStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_registry::ProjectRegistry;
    use chrono::{DateTime, Utc};
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper::{Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use std::time::Duration;

    #[test]
    fn forwarding_header_name_stays_in_sync_with_conexus_auth() {
        assert_eq!(
            FORWARDING_HEADER_NAME,
            conexus_auth::forwarding_header::HEADER_NAME.to_ascii_lowercase()
        );
    }

    fn registry_with(dir: &Path, name: &str) -> ProjectRegistry {
        let registry = ProjectRegistry::new(dir.join("projects.local.json"));
        let now: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
        registry
            .register(name, "/ws/proj-a", "python", now)
            .unwrap();
        registry
    }

    fn fast_ensure_cfg() -> EnsureConfig {
        EnsureConfig {
            systemctl_program: "true".to_string(),
            systemctl_mode: crate::orchestrator::primitives::SystemctlMode::User,
            systemctl_timeout: Duration::from_secs(5),
            ensure_failure_cooldown: Duration::from_millis(200),
            boot_grace: Duration::from_millis(150),
            socket_poll_attempts: 5,
            max_restart_attempts: 5,
            giveup_cooldown: Duration::from_secs(600),
        }
    }

    #[test]
    fn is_hop_by_hop_header_matches_the_fixed_set_and_the_proxy_prefix() {
        assert!(is_hop_by_hop_header("Connection"));
        assert!(is_hop_by_hop_header("Transfer-Encoding"));
        assert!(is_hop_by_hop_header("Proxy-Authenticate"));
        assert!(!is_hop_by_hop_header("Authorization"));
        assert!(!is_hop_by_hop_header("Content-Type"));
    }

    #[test]
    fn filter_headers_strips_host_content_length_and_the_forwarding_header() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("example.test"));
        headers.insert("content-length", HeaderValue::from_static("5"));
        headers.insert(
            "X-Conexus-Forwarded-Operator",
            HeaderValue::from_static("stale"),
        );
        headers.insert("authorization", HeaderValue::from_static("Bearer abc"));

        let out = filter_headers(&headers);
        assert!(!out.contains_key("host"));
        assert!(!out.contains_key("content-length"));
        assert!(!out.contains_key("x-conexus-forwarded-operator"));
        assert!(out.contains_key("authorization"));
    }

    #[test]
    fn is_stream_request_matches_exactly_the_three_get_paths() {
        assert!(is_stream_request(&Method::GET, "/mcp"));
        assert!(is_stream_request(&Method::GET, "/api/delivery/stream"));
        assert!(is_stream_request(&Method::GET, "/api/events"));
        assert!(!is_stream_request(&Method::POST, "/mcp"));
        assert!(!is_stream_request(&Method::GET, "/api/agents"));
    }

    #[test]
    fn sse_agent_key_hashes_a_bearer_and_falls_back_to_anon() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer abc"));
        let key1 = sse_agent_key(&headers);
        assert_eq!(key1.len(), 16);

        let mut headers2 = HeaderMap::new();
        headers2.insert("authorization", HeaderValue::from_static("Bearer abc"));
        assert_eq!(
            key1,
            sse_agent_key(&headers2),
            "the same bearer must hash identically"
        );

        assert_eq!(sse_agent_key(&HeaderMap::new()), "anon");
    }

    /// F11: a cookie-authenticated caller (no `Authorization` header at
    /// all, only the router's own signed forwarding header) must get a
    /// key derived from THEIR resolved operator identity, not the
    /// shared `"anon"` fallback -- and that key must be STABLE across
    /// requests for the same operator even though `sign()` mints a
    /// fresh MAC/expiry every call (see `backend_api_handler`'s own
    /// doc: it re-signs with a fresh clock read on every request), and
    /// DISTINCT for a different operator.
    #[test]
    fn sse_agent_key_isolates_cookie_authenticated_operators_by_identity() {
        let mut alice1 = HeaderMap::new();
        alice1.insert(
            FORWARDING_HEADER_NAME,
            HeaderValue::from_static("alice.viewer.1000.aaaa"),
        );
        let mut alice2 = HeaderMap::new();
        alice2.insert(
            // Same operator, but a DIFFERENT role/expiry/mac -- as a
            // real second request's fresh `sign()` call would produce.
            FORWARDING_HEADER_NAME,
            HeaderValue::from_static("alice.operator.2000.bbbb"),
        );
        let mut bob = HeaderMap::new();
        bob.insert(
            FORWARDING_HEADER_NAME,
            HeaderValue::from_static("bob.viewer.1000.cccc"),
        );

        let alice_key1 = sse_agent_key(&alice1);
        let alice_key2 = sse_agent_key(&alice2);
        let bob_key = sse_agent_key(&bob);

        assert_ne!(
            alice_key1, "anon",
            "a resolved operator must never fall back to anon"
        );
        assert!(
            alice_key1.starts_with("op:"),
            "a forwarding-header-derived key must be distinguishable from a bearer-derived one, got {alice_key1:?}"
        );
        assert_eq!(
            alice_key1, alice_key2,
            "the SAME operator must hash to the SAME key across two independently-signed requests"
        );
        assert_ne!(
            alice_key1, bob_key,
            "two DIFFERENT operators must never share a bucket key"
        );
    }

    #[test]
    fn stream_cap_registry_admits_up_to_the_per_agent_and_global_caps() {
        let registry = Arc::new(StreamCapRegistry::new(2, 3));
        let g1 = registry.try_admit("alice").unwrap();
        let g2 = registry.try_admit("alice").unwrap();
        assert!(
            registry.try_admit("alice").is_none(),
            "per-agent cap of 2 must reject a 3rd"
        );

        let g3 = registry.try_admit("bob").unwrap();
        drop(g1);
        // Global cap of 3: alice(1) + bob(1) = 2 in flight after dropping g1;
        // a 3rd (any agent) must still be admitted.
        let g4 = registry.try_admit("bob").unwrap();
        assert!(
            registry.try_admit("carol").is_none(),
            "global cap of 3 must reject a 4th concurrent stream"
        );
        drop(g2);
        drop(g3);
        drop(g4);
        // Everything released -- a fresh admission must succeed again.
        assert!(registry.try_admit("carol").is_some());
    }

    /// A real HTTP/1 server bound to a Unix socket, used to prove
    /// `proxy_to_backend` end to end -- not a mock.
    async fn spawn_backend(
        sock_path: std::path::PathBuf,
        response_builder: impl Fn(&hyper::Request<Incoming>) -> Response<Full<Bytes>>
            + Send
            + Sync
            + 'static,
    ) {
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let response_builder = Arc::new(response_builder);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let response_builder = response_builder.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req: hyper::Request<Incoming>| {
                        let response_builder = response_builder.clone();
                        async move { Ok::<_, std::convert::Infallible>(response_builder(&req)) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn proxy_to_backend_forwards_a_buffered_json_rpc_call_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |req| {
            assert_eq!(req.uri().path(), "/mcp");
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer tok"));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::POST,
                path_and_query: "/mcp".to_string(),
                headers,
                body: Bytes::from_static(b"{}"),
            },
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(response.status, 200);
        match response.body {
            ProxyResponseBody::Buffered(b) => assert_eq!(b.as_ref(), b"{\"ok\":true}"),
            ProxyResponseBody::Streaming(_) => panic!("expected a buffered response"),
        }
    }

    #[tokio::test]
    async fn proxy_to_backend_sets_host_localhost_for_the_backend_leg() {
        // Regression test for a real production bug: `filter_headers`
        // strips the client's real `Host` (matching Python's own
        // `_proxy_to_backend`), but nothing re-added one for the
        // backend leg -- Python's aiohttp implicitly synthesizes
        // `Host: localhost` from its own `f"http://localhost{path}"`
        // request URL; this hyper-based port built a bare-path
        // `Request` with no authority, so the backend received NO
        // Host header at all. `conexus-backend`'s rmcp layer enforces
        // DNS-rebinding protection and hard-rejects any HTTP/1.1
        // request missing `Host` -- so every real MCP call through
        // the deployed router->backend proxy 400'd, caught only by a
        // real end-to-end MCP call against a live production cutover,
        // not by any unit test (every prior test/live-verify hit
        // conexus-backend directly over UDS, bypassing this proxy).
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |req| {
            let host = req
                .headers()
                .get("host")
                .map(|v| v.to_str().unwrap().to_string());
            Response::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from(host.unwrap_or_default())))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));
        let mut headers = HeaderMap::new();
        // The real client's Host must NOT leak through to the backend
        // (that would be the client-supplied `Host: evil.example`,
        // not the synthesized `localhost`).
        headers.insert("host", HeaderValue::from_static("evil.example"));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::POST,
                path_and_query: "/mcp".to_string(),
                headers,
                body: Bytes::from_static(b"{}"),
            },
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(response.status, 200);
        match response.body {
            ProxyResponseBody::Buffered(b) => assert_eq!(b.as_ref(), b"localhost"),
            ProxyResponseBody::Streaming(_) => panic!("expected a buffered response"),
        }
    }

    #[tokio::test]
    async fn proxy_to_backend_attaches_the_alias_header() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |req| {
            let alias_header = req
                .headers()
                .get("x-conexus-alias")
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default();
            Response::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from(alias_header)))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/api/agents".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            Some(&AliasInfo {
                name: "old-name".to_string(),
                expires_at: "2026-02-01T00:00:00Z".to_string(),
            }),
            None,
        )
        .await
        .unwrap();

        match response.body {
            ProxyResponseBody::Buffered(b) => {
                assert_eq!(b.as_ref(), b"old-name,2026-02-01T00:00:00Z")
            }
            ProxyResponseBody::Streaming(_) => panic!("expected a buffered response"),
        }
    }

    #[tokio::test]
    async fn proxy_to_backend_attaches_a_provided_forwarding_header_value() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |req| {
            let header = req
                .headers()
                .get("x-conexus-forwarded-operator")
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default();
            Response::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from(header)))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/api/agents".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            Some("op1.operator.9999999999.deadbeef"),
        )
        .await
        .unwrap();

        match response.body {
            ProxyResponseBody::Buffered(b) => {
                assert_eq!(b.as_ref(), b"op1.operator.9999999999.deadbeef")
            }
            ProxyResponseBody::Streaming(_) => panic!("expected a buffered response"),
        }
    }

    #[tokio::test]
    async fn proxy_to_backend_never_forwards_a_client_supplied_forwarding_header() {
        // The router alone is authoritative for this header (see
        // `filter_headers`'s own doc) -- a client that sneaks one in
        // must never reach the backend, whether or not the caller ALSO
        // supplies a genuinely-resolved `forwarding_header_value`.
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |req| {
            let header = req
                .headers()
                .get("x-conexus-forwarded-operator")
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default();
            Response::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from(header)))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-conexus-forwarded-operator",
            HeaderValue::from_static("mallory.operator.9999999999.forged"),
        );

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/api/agents".to_string(),
                headers,
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await
        .unwrap();

        match response.body {
            ProxyResponseBody::Buffered(b) => assert_eq!(
                b.as_ref(),
                b"",
                "a client-supplied forwarding header must never reach the backend"
            ),
            ProxyResponseBody::Streaming(_) => panic!("expected a buffered response"),
        }
    }

    #[tokio::test]
    async fn proxy_to_backend_streams_a_get_mcp_sse_response() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |_req| {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from_static(b"data: hello\n\n")))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/mcp".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await
        .unwrap();

        match response.body {
            ProxyResponseBody::Streaming(mut s) => {
                let mut collected = Vec::new();
                while let Some(chunk) = s.next().await {
                    collected.extend_from_slice(&chunk.unwrap());
                }
                assert_eq!(collected, b"data: hello\n\n");
            }
            ProxyResponseBody::Buffered(_) => {
                panic!("a GET /mcp response must stream, never buffer")
            }
        }
    }

    #[tokio::test]
    async fn proxy_to_backend_rejects_a_get_mcp_stream_over_the_per_agent_cap() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |_req| {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from_static(b"data: x\n\n")))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        // Cap of 0 -- every GET /mcp request is over budget immediately.
        let stream_caps = Arc::new(StreamCapRegistry::new(0, 64));

        let err = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/mcp".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProxyError::TooManyStreams));
    }

    /// F11 end-to-end, through the REAL `StreamCapRegistry` admission
    /// path `proxy_to_backend` drives (not just the pure `sse_agent_key`
    /// unit above): confirmed live exploit was a brand-new operator's
    /// very first stream 429ing purely because a DIFFERENT operator's
    /// streams (on a different project) had already filled the shared
    /// `"anon"` bucket. Both callers here are cookie-authenticated (a
    /// forwarding header, no `Authorization` header at all) -- exactly
    /// the caller shape that used to collapse onto one shared bucket.
    #[tokio::test]
    async fn proxy_to_backend_isolates_stream_caps_by_forwarded_operator_identity() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |_req| {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from_static(b"data: x\n\n")))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        // Per-agent cap of 4 (this crate's real default), a global cap
        // generous enough that it never interferes with this test.
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));

        let open_stream = |fwd_header: &'static str| {
            let store = &store;
            let stream_caps = &stream_caps;
            let registry = &registry;
            let sock_dir = &sock_dir;
            async move {
                proxy_to_backend(
                    store,
                    stream_caps,
                    registry,
                    sock_dir,
                    "proj-a",
                    &fast_ensure_cfg(),
                    ProxyRequest {
                        method: Method::GET,
                        path_and_query: "/mcp".to_string(),
                        headers: HeaderMap::new(),
                        body: Bytes::new(),
                    },
                    None,
                    Some(fwd_header),
                )
                .await
            }
        };

        // Operator alice fills HER OWN cap of 4 -- kept alive (not
        // drained/dropped) for the rest of the test so the slots stay
        // held, matching a real caller with 4 concurrent live streams.
        let mut alice_streams = Vec::new();
        for _ in 0..4 {
            alice_streams.push(
                open_stream("alice.viewer.9999999999.deadbeef")
                    .await
                    .unwrap(),
            );
        }

        // Alice's OWN 5th stream must still be correctly rejected --
        // per-operator isolation must not accidentally turn into NO
        // cap at all.
        let alice_5th = open_stream("alice.viewer.9999999999.deadbeef").await;
        assert!(
            matches!(alice_5th, Err(ProxyError::TooManyStreams)),
            "alice's own 5th stream must still hit her own cap of 4, got {alice_5th:?}"
        );

        // A DIFFERENT operator (bob), on the same project, must be
        // completely unaffected by alice's exhausted bucket -- before
        // the F11 fix, both hashed to the shared "anon" bucket and
        // bob's very first stream would ALSO have been rejected here.
        let bob_stream = open_stream("bob.viewer.9999999999.deadbeef").await;
        assert!(
            bob_stream.is_ok(),
            "a different operator's first stream must not be rejected by \
             another operator's exhausted cap, got {bob_stream:?}"
        );

        drop(alice_streams);
        drop(bob_stream);
    }

    #[tokio::test]
    async fn proxy_to_backend_releases_the_stream_cap_once_the_stream_is_fully_drained() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |_req| {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from_static(b"data: x\n\n")))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(1, 64));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/mcp".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await
        .unwrap();

        let ProxyResponseBody::Streaming(mut s) = response.body else {
            panic!("expected a streaming response");
        };
        while s.next().await.is_some() {}
        drop(s);

        // The slot must be free again -- a second request must succeed
        // even though max_per_agent is 1.
        let response2 = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/mcp".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await;
        assert!(
            response2.is_ok(),
            "the cap slot must be released once the prior stream was drained and dropped"
        );
    }

    #[tokio::test]
    async fn proxy_to_backend_reports_backend_unavailable_for_a_missing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        // No backend spawned at all -- ensure() will start a
        // (nonexistent, "true"-stubbed) unit but the socket never
        // appears, so this proves the ensure()-level timeout maps
        // into a ProxyError, not a panic.
        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));

        let err = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/api/agents".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProxyError::Ensure(_)));
    }

    #[tokio::test]
    async fn proxy_to_backend_strips_transfer_encoding_and_content_length_from_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("sockets");
        std::fs::create_dir_all(sock_dir.join("proj-a")).unwrap();
        spawn_backend(sock_dir.join("proj-a").join("backend.sock"), |_req| {
            Response::builder()
                .status(StatusCode::OK)
                .header("x-custom", "kept")
                .body(Full::new(Bytes::from_static(b"body")))
                .unwrap()
        })
        .await;

        let registry = registry_with(dir.path(), "proj-a");
        let store = RuntimeStore::new();
        let stream_caps = Arc::new(StreamCapRegistry::new(4, 64));

        let response = proxy_to_backend(
            &store,
            &stream_caps,
            &registry,
            &sock_dir,
            "proj-a",
            &fast_ensure_cfg(),
            ProxyRequest {
                method: Method::GET,
                path_and_query: "/api/agents".to_string(),
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            None,
            None,
        )
        .await
        .unwrap();

        assert!(response
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-custom") && v == "kept"));
        assert!(!response
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("transfer-encoding")
                || k.eq_ignore_ascii_case("content-length")));
    }
}
