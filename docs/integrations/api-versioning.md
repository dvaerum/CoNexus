# CoNexus REST API versioning

The CoNexus REST surface under `/conexus/api/<name>/...` (PR-B
renamed from `/__api/`) requires every request to carry an explicit,
version-pinned `Accept` header. A request without it (or with a
generic `application/json`, `*/*`, etc.) is rejected with HTTP 406 and
a structured error body.

## TL;DR for clients

Send this header on every REST request:

```
Accept: application/vnd.conexus.v1+json
```

If you forget, you get a 406 response that tells you exactly what to
add. Parse the body for `error == "version_required"` to detect this
case programmatically.

## Why a strict gate?

Two reasons:

1. **Explicit opt-in.** When v2 ships under
   `application/vnd.agent-mcp.v2+json`, the v1 wire contract keeps
   working unchanged for callers that asked for v1. A client that
   sent `Accept: application/json` would have no way to tell which
   version it got — fragile.
2. **Subsumes the §3.7 audit finding.** Previously, the
   `/conexus/api/<name>/tokens` endpoint applied its
   `Authorization: Bearer` admin-role check only when a header was
   present, leaving the response readable to a request that sent no
   credentials at all. The Accept gate runs first, so an unauthenticated
   request now fails at the gate before reaching any per-endpoint
   auth logic.

## Error body shape

```json
{
  "error": "version_required",
  "message": "agent-mcp REST endpoints require an Accept header specifying the API version. Resend with: Accept: application/vnd.conexus.v1+json",
  "supported_versions": ["v1"],
  "current_default": "v1",
  "docs": "https://github.com/dvaerum/CoNexus/blob/main/docs/integrations/api-versioning.md"
}
```

| Field                | Type           | Meaning                                                   |
|----------------------|----------------|-----------------------------------------------------------|
| `error`              | string literal | Always `"version_required"` for this response             |
| `message`            | string         | Human-readable one-liner naming the exact header to add   |
| `supported_versions` | list[string]   | Every API version the router currently accepts            |
| `current_default`    | string         | Recommended version for new integrations                  |
| `docs`               | URL            | Stable link to this document                              |

## Accept header parsing rules

The gate is forgiving about the *shape* of the Accept header, strict
about the *content*:

- Bare media type: `application/vnd.conexus.v1+json` — accepted.
- With parameters: `application/vnd.conexus.v1+json;q=0.9` — accepted.
- Multi-value list: `text/plain, application/vnd.conexus.v1+json` —
  accepted (the v1 media type must appear somewhere).
- Wildcards: `*/*`, `application/*`, `application/json` — **rejected**.
- Missing header: **rejected**.

## What's NOT gated

- `/conexus/<name>/mcp` — the MCP transport has its own version
  negotiation inside `initialize.protocolVersion`. Adding our Accept
  gate would break every MCP client.
- `/conexus/app/...` and `/conexus/assets/...` — dashboard HTML
  and Next.js static assets are loaded by browsers, which don't send
  our private media type.
- CORS preflights (`OPTIONS`) — exempted so browsers can complete
  preflight before sending the real request. The Accept gate runs on
  the request itself.

## Service descriptor

`GET /conexus/` returns a JSON discovery document so a plain HTTP
client can find the endpoint layout without scraping HTML (port of
`_service_descriptor`, `rust/conexus-router/src/dashboard_static.rs`;
the internal package version is deliberately NOT echoed — SEC,
owner-authorised — pure attacker-useful build fingerprinting no
operator consumes):

```json
{
  "service": "agent-mcp",
  "mode": "multi-tenant",
  "endpoints": {
    "api": "/conexus/api",
    "app": "/conexus/app",
    "assets": "/conexus/assets",
    "mcp": "/conexus/mcp"
  },
  "projects_url": "/conexus/api/router/projects",
  "overview_url": "/conexus/api/router/overview",
  "health_url": "/conexus/api/router/health",
  "single_tenant_project": null
}
```

(The old `/conexus/__projects`, `/conexus/__overview`,
`/conexus/__create`, `/conexus/__rename` dunder endpoints named in
an earlier draft of this doc were retired by ADR-0014 — the real
project-lifecycle/admin surface now lives entirely under
`/conexus/api/router/*`.)

PR-B (v4.0.0) renamed the top-level prefixes from
`/__api` / `/__dashboard` / `/__dashboard/_next` to `/api` / `/app` /
`/assets`. The old paths 308-redirect to the new ones during a 30-day
grace period for external services and bookmarks.

The descriptor is Accept-negotiated: a browser (`Accept: text/html`)
sees the existing 302 → `/conexus/app/`; everything else
sees the JSON above. The descriptor itself does NOT require the v1
media type — discovery is the entry point, not a v1-specific
operation.

## Versioning policy

- A new version ships as a new media-type subtype:
  `application/vnd.agent-mcp.v2+json`.
- The router accepts ALL supported versions in parallel; the response
  shape matches the requested version.
- `supported_versions` in the 406 error body always lists what the
  router can serve right now.
- `current_default` names the version recommended for new clients
  (used to be `v1`; bump in lockstep with the major).
- Old versions are removed only across a fork-major bump (current:
  `3.x` accepts v1; `4.x` will still accept v1, with v2 alongside;
  `5.x` may drop v1).
