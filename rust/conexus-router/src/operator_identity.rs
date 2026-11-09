//! Axum extractor for handlers that act on a named operator (user/group
//! admin, SSO config, project stop/alias-usage).
//!
//! Single-tenant mode (ADR-0008) makes `session_gate_layer` take the
//! `PassThrough` branch, which never inserts a `GateIdentity`. A bare
//! `Extension<GateIdentity>` extractor turns that into an axum-level 500
//! before any handler code runs; this one answers with the same 410 body
//! every other single-tenant-disabled endpoint uses.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::mcp_handler::{HandlerBody, HandlerResponse};
use crate::session_gate::GateIdentity;
use crate::single_tenant::{bypasses_operator_gate, single_tenant_disabled_response};
use crate::state::RouterState;

pub struct OperatorIdentity(pub GateIdentity);

impl FromRequestParts<Arc<RouterState>> for OperatorIdentity {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<RouterState>,
    ) -> Result<Self, Self::Rejection> {
        if let Some(identity) = parts.extensions.get::<GateIdentity>() {
            return Ok(Self(identity.clone()));
        }
        let single_tenant_name = state.mcp_handler_config.single_tenant_name.as_deref();
        Err(if bypasses_operator_gate(single_tenant_name) {
            single_tenant_disabled_response(single_tenant_name)
        } else {
            // Multi-tenant mode always inserts an identity or rejects
            // earlier, so reaching here means the gate layer is missing
            // from this route.
            HandlerResponse {
                status: 401,
                headers: Vec::new(),
                body: HandlerBody::Json(serde_json::json!({
                    "success": false,
                    "error": "unauthenticated",
                })),
            }
        }
        .into_response())
    }
}
