# ADR-0008: Single-tenant URL parity over single-tenant simplicity

## Status

Accepted (2026-06-03).

Amended by ADR-0020 (2026-07-31): the `/conexus` mount prefix in the
URL examples below moves out of the application and into the reverse
proxy (`X-Forwarded-Prefix`). This ADR's goal that a build artefact
"works at any prefix" (below) is *completed* there — the prefix becomes
per-request rather than a router-owned constant. The single-tenant ↔
multi-tenant URL-parity decision itself is unchanged.

## Context

Forks of conexus can run in two modes:

- **Multi-tenant**: router-fronted, many projects per machine, dashboard and
  backends mounted under `/conexus/__dashboard/<project>/...` and
  `/conexus/<project>/...`.
- **Single-tenant**: one project per machine.

The original Phase 0 plan was to make single-tenant skip the router entirely:
the backend would bind directly to the public socket, the dashboard would be
served at `/dashboard/` with no project prefix, and the `services.conexus.router.enable`
home-manager toggle would gate which mode you were in.

A grilling session on 2026-06-03 surfaced a stronger preference for URL
parity between the two modes — the same absolute URLs should work regardless
of whether the host has one project or many.

## Decision

Single-tenant runs the router with N=1. Same absolute URLs as multi-tenant
(`/conexus/__dashboard/<name>/...` for the dashboard,
`/conexus/<name>/...` for the backend).

The previously-proposed `services.conexus.router.enable` toggle is
**replaced** by `services.conexus.multiTenant : bool`. That option is
UI-only: it controls

- whether the project picker is greyed out (single-tenant: greyed, only one
  project visible)
- whether ops endpoints (create/rename/delete project) are exposed.

The URL surface is identical in both modes.

## Consequences

- Single-tenant pays the router process tax: one extra hop through a UDS,
  lazy backend spawn. With N=1 there is no contention; cost is negligible
  in practice.
- One URL surface to test, document, and bookmark. Operators who started
  single-tenant and grew into multi-tenant don't have to update their
  bookmarks or `.mcp.json` files.
- Today's two router bugs (a SPA-fallback bug and an aiohttp
  trailing-slash bug, both in the now-deleted Python router — their
  commit hashes no longer resolve after the Rust migration's history
  squash) would have been one fix each rather than two — the
  router-less path would have re-introduced the same bug classes in a
  second code path.
- Future "deploy at a different URL prefix" works the same way in both modes
  (Phase 4 sentinel substitution at install time).
- The `multiTenant` option in home-manager stays a pure UI affordance — no
  router-vs-no-router branch in NixOS module logic.

## Alternatives considered

- **Router-less single-tenant.** Rejected: doubles the URL surface, doubles
  the test matrix, doubles the bug classes. Every router fix would need a
  paired "and also fix the direct-bind code path" PR.
- **Per-mode dashboard build** (one Next.js build for each URL layout).
  Rejected: doubles the build artefact and the dashboard cache size. Phase
  4's sentinel substitution makes per-mode builds unnecessary — the same
  artefact works at any prefix.

## Links

- Plan: originally decision #1 of the "prancy-napping-pie" working
  plan — an ephemeral Claude Code plan-mode file, never committed to
  this repo, no longer available. This ADR is the durable record.
- Phase 3 PR (when it ships) for the `multiTenant` toggle implementation.
- The router SPA-fallback fix and the router aiohttp trailing-slash
  fix (both in the deleted Python router; see the note above) —
  concrete examples of the bug class this decision prevents from
  doubling.
