# Dashboard known gaps (unfixed, tracked)

Migrated 2026-09-06 from the deploy repo's (`home-manager-config`,
formerly `nixos-developer-system`) `docs/LEARNINGS.md` §9 and
`docs/UPSTREAM_ISSUES.md` §F — real, still-open CoNexus dashboard
bugs found while operating the deployment, misfiled as deploy notes
since this fork had no place to track them at the time. Confirmed
still relevant (not a duplicate of anything already tracked here)
before moving. Not superseded by the Rust migration — these are
dashboard/frontend-side, language-independent of the backend.

## Agents tab counters don't match the row statuses — RESOLVED

**Verified fixed** while investigating a related agent-count-mismatch
bug (project-list "Agents: N" card disagreeing with this same Agents
tab; see `rust/conexus-router/src/project_reads.rs::project_counts`).
`agentPresence()` (`conexus/dashboard/lib/api/agents.ts`) is now a
total function over every `Agent.status` — it always returns one of
`online`/`offline`/`pending`/`terminated`, with `pending` as the
default fallback rather than an unbucketed gap. The Agents tab's
`stats` (`conexus/dashboard/components/dashboard/agents-dashboard.tsx`)
sets `total: agents.length` and buckets every one of those same
`agents` through `agentPresence()`, so `online + pending + offline +
terminated` sums to `total` by construction — there is no longer a
status value that falls outside all four buckets. Reproduced the
original repro shape (create 3, terminate 2) against current code: the
Total card and the four buckets agree.

## "System Online" sidebar indicator is hardcoded

The dashboard sidebar's online/offline indicator is hardcoded true
upstream; the dashboard actually polls REST `/all-data`, which always
succeeds because the router cold-starts the backend on demand
regardless of whether anything is actually healthy. A real indicator
needs a dedicated health endpoint backed by genuine systemd/process
state, not "the aggregate endpoint didn't 500."

## "Server Management" sidebar item is vestigial

Left over from a host:port multi-server selection UI that doesn't
apply to this project's actual deployment shape (one router, lazily
spawned per-project backends). Harmless but confusing; hide it or
repurpose it rather than leaving a dead menu entry visible.
