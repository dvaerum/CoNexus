# ADR 0030: Index agent-to-agent messages into RAG, scoped strictly to sender/recipient

**Status**: Accepted, implemented 2026-09-25.
**Date**: 2026-09-25.
**Builds on**: `rag_repository::search_similar`'s `source_type_filter` +
over-fetch mechanism (already used to scope "markdown"/"context"
sources), and the `task_ownership` post-search filtering pattern
established by ADR-0027 for scoping `ask_project_rag`'s live-task
retrieval to what the caller may see.
**Requested by**: dvv (operator), while testing `ask_project_rag`
after ADR-not-yet-numbered's Ollama/RAG infra fix. Design worked out
directly with dvv in the same session (not delegated to a worker's
own judgment call), given the privacy sensitivity.

## Context

`ask_project_rag` indexes exactly two `source_type`s today:
`"markdown"` (repo files) and `"context"` (`project_context` rows).
Task titles/descriptions are excluded from embedding entirely and
read LIVE instead (`fetch_live_tasks`, scoped by `task_ownership`,
per ADR-0027).

`agent_messages` (the `send_agent_message`/`get_agent_messages`/
`broadcast_admin_message` backing table) is invisible to RAG in both
directions -- not embedded, not read live. An operator testing a
real multi-agent session found this made `ask_project_rag` blind to
everything that happened via agent-to-agent conversation (hardware
support discussions, API design work, etc.) -- only
`project_context` and task metadata were represented in answers.

The natural fix is a third `source_type` (e.g. `"agent_message"`).
The obvious risk: `agent_messages` rows are two-party private
conversations (`sender_id`, `recipient_id`), unlike markdown/context,
which are already project-wide-visible content (ADR-0017's framing).
Naively embedding and searching them the same way `"markdown"`/
`"context"` are searched would let ANY agent's `ask_project_rag`
query surface ANY other pair of agents' private DM content -- a real
disclosure regression, not a hypothetical one.

## Decision

1. **Index `agent_messages` as a new RAG source, `source_type =
   "agent_message"`.** Same chunking/embedding pipeline as
   `"markdown"`/`"context"` (background_tasks.rs's existing indexer
   loop gains a third source), same `rag_chunks` table, same
   `search_similar` call path.

2. **Retrieval is scoped by a hard DB-column predicate, not content
   inference**: a chunk is visible to caller `C` iff `sender_id = C`
   OR `recipient_id = C` on the `agent_messages` row it was chunked
   from. Implemented the same way `source_type_filter` already works
   in `search_similar` (over-fetch a larger K when the filter is
   active, filter post-search, before assembly into the LLM prompt) --
   no new retrieval mechanism, a second application of the existing
   one.

3. **No exposure is ever derived from anything other than
   `sender_id`/`recipient_id` on the row itself.** Explicitly, and
   deliberately, ruled out:
   - A message that mentions a task by name/ID does NOT inherit that
     task's ownership or visibility. There is no foreign key from
     `agent_messages` to `tasks`, and none is being added by this
     ADR -- scoping never reads task-assignment state to decide
     message visibility.
   - Task comments (`add_task_comment`, already its own
     independently-scoped feature per ADR-0027) are explicitly OUT
     OF SCOPE for this ADR -- not touched, not merged into this
     source_type.
   - No later reassignment, role change, or task-history event ever
     retroactively grants or revokes visibility of a message already
     sent. Scoping is evaluated fresh against the row's own
     `sender_id`/`recipient_id` on every query, but those two columns
     never change after the message is sent, so the answer is stable
     over time by construction.

4. **Broadcasts need no special-case rule.** Confirmed directly from
   `broadcast_admin_message`'s implementation
   (`agent_communication_tools.rs`): a broadcast fans out to one
   `agent_messages` ROW PER RECIPIENT (matching Python's own fan-out
   behavior), each with that recipient's real `agent_id` in
   `recipient_id`. The plain `sender = caller OR recipient = caller`
   predicate already covers this correctly -- every agent's own copy
   of a broadcast satisfies `recipient_id = caller` on its own row.
   No sentinel "visible to everyone" value, no OR-branch for
   broadcast message_type, nothing extra to build or get wrong.

## Why this widening is different from ADR-0027's

ADR-0027 widened DEFAULT task visibility between cooperating workers
on the same project -- a deliberate relaxation the operator asked
for, on content the operator already treats as project-shared
(titles/descriptions/status).

This ADR does the opposite: it ADDS a new RAG source, but keeps its
default visibility as narrow as the source data's own real ownership
(exactly the two parties a message was between) -- there is no
"foreign message" toggle being proposed, and none is planned. A
private DM between two agents stays exactly as private under RAG as
it already is under `get_agent_messages` itself (which only ever
returns a caller's own sent/received messages) -- RAG must not grant
a caller a search-shaped side channel into a visibility the direct
tool already denies them.

## Consequences

### Positive

- `ask_project_rag` answers can now draw on real conversational
  history (hardware/API discussions, decisions made in DMs) that was
  previously invisible to it, closing a real blind spot an operator
  hit in practice.
- Each agent's own message history becomes RAG-searchable (a
  standalone win independent of the multi-agent blind-spot fix --
  "what did I discuss about X three days ago" becomes answerable).
- Zero new retrieval mechanism -- reuses the exact `source_type_filter`
  + over-fetch pattern already proven for `"markdown"`/`"context"`,
  and the exact post-search-filter shape ADR-0027 established for
  live tasks. Low structural risk of a new class of scoping bug.

### Negative / trade-offs

- A third embedding job adds to the background indexer's steady-state
  work (chunking + embedding cost scales with message volume, which
  is likely to be the busiest of the three sources over time in an
  active multi-agent project).
- `agent_messages` rows are currently unbounded-growth (per
  `background_tasks.rs`'s own existing doc comment on that table) --
  indexing them means the RAG corpus inherits that growth
  characteristic too. Retention/pruning for `agent_messages` is an
  existing, separate concern this ADR does not change or solve.

## Alternatives considered

- **Separate per-agent vector index/table instead of a shared index
  with post-search filtering** -- rejected. Stronger isolation in
  theory, but this codebase has no existing precedent for
  per-principal index partitioning, `sqlite-vec`'s `vec0` virtual
  table has no native per-row ACL to build on cheaply, and the
  post-search-filter pattern is already proven correct (in production)
  for an analogous ownership-scoping problem (ADR-0027's task
  retrieval). Introducing a second, structurally different scoping
  mechanism for a single new source_type is more surface to get wrong,
  not less.
- **Content-based inference for edge cases** (e.g. "a message
  mentioning a task inherits that task's visibility") -- rejected,
  explicitly, per dvv: "we are not making any exposure we can not
  hard link in a db." Matches ADR-0017's own rejection of
  content-based reasoning for a different problem (secret
  redaction) -- unreliable in both directions, and here the FK simply
  doesn't exist, so there is nothing to infer FROM in the first
  place.
- **Exclude agent_messages from RAG entirely, treat the blind spot as
  by-design** -- rejected once flagged; the operator characterized it
  as "no obvious reason it should be excluded if it's just an
  oversight," and confirmed indexing is the right call once the
  scoping rule (above) was worked out.

## Links

- `rust/conexus-db/src/rag_repository.rs` -- `search_similar`'s
  `source_type_filter` + over-fetch mechanism this ADR's retrieval
  scoping reuses.
- `rust/conexus-db/src/schema.rs` -- `agent_messages`' actual columns
  (`sender_id`, `recipient_id`, no FK to `tasks`).
- `rust/conexus-tools/src/agent_communication_tools.rs` --
  `broadcast_admin_message`'s per-recipient fan-out (confirms broadcast
  needs no special-case rule).
- `rust/conexus-backend/src/background_tasks.rs` -- the existing
  markdown/context indexer loop this ADR's implementation adds a third
  source to.
- [ADR-0027](0027-cross-agent-task-visibility-default-on.md) -- the
  post-search ownership-filter pattern this ADR is a second
  application of, and the contrast in widening-direction described
  above.
- [ADR-0017](0017-no-content-secret-redaction.md) -- prior rejection
  of content-based inference, for a different (secret-detection)
  problem; this ADR's "no exposure without a hard FK" rule is the same
  underlying principle applied to authorization instead of secrecy.

## Status of implementation

Implemented 2026-09-25:
- `background_tasks.rs::rag_indexing` gained a third always-on scan
  step (agent_messages, mirroring `"context"`'s treatment), each
  message becoming one `ScannedSource` with `metadata =
  {"sender_id", "recipient_id"}`, its own `last_indexed_agent_message`
  watermark (BL-R31-1 failure-capping included, same as the other two
  source types).
- `rag_tools.rs::drop_unowned_message_chunks` -- the post-search
  filter, applied right after `drop_unowned_task_chunks` in
  `query_rag_system`'s vector-search stage. A `None` `requesting_agent_
  id` (no agent-bearer identity) or a chunk with no metadata both drop
  closed (deny by default), not open.
- TDD coverage: 6 unit tests for the retrieval filter (own-sent,
  own-received, neither, no-caller, no-metadata, non-message-chunk
  passthrough) plus one indexer end-to-end test asserting the metadata
  actually lands in `rag_chunks` and the watermark/hash meta keys are
  written. Full workspace suite green (2,000+ tests) after the change.
