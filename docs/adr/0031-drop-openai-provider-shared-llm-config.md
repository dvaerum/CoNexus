# ADR 0031: Drop the OpenAI cloud provider; one shared LLM endpoint; subject-gen on by default

**Status**: Accepted, implemented 2026-09-28.
**Date**: 2026-09-28.
**Builds on**: `completion_client.rs`/`embedding_client.rs`'s existing
"explicit `get_env` lookup" convention; ADR-0030's precedent of
resolving a design question directly with dvv before implementation,
given the operator-facing/config-shape stakes.
**Requested by**: dvv (operator), while investigating why the
`ask_project_rag` auto-subject feature "looked like it should work but
didn't" (root cause: the AI subject-gen feature was silently a no-op
because `CONEXUS_SUBJECT_MODEL` had no default and nothing set it).

## Context

Three separate LLM-config surfaces had accreted independently over
time, each with its own shape:

1. **Chat completion** (`completion_client.rs`): branched on
   `OPENAI_API_KEY` — set (+ `OPENAI_MODEL`) selected a real OpenAI
   cloud client; unset defaulted to local Ollama
   (`OLLAMA_MODEL`, default `qwen3:1.7b`).
2. **Embeddings** (`embedding_client.rs`): the identical branch shape,
   its own `OPENAI_BASE_URL`/cloud defaults
   (`text-embedding-3-large`/1536-dim) on the OpenAI side.
3. **Subject-gen** (`message_suggestions.rs`): deliberately SEPARATE
   from both of the above (own doc comment: "always wants a LOCAL
   Ollama endpoint regardless of whatever provider RAG ... is
   configured with") — gated entirely on `CONEXUS_SUBJECT_MODEL`
   being set at all, with NO compiled-in default. Unset meant
   "feature off," conflating "which model" with "should this run."

This produced two real problems, found by dvv testing a live
deployment:

- **Inconsistency invited exactly the kind of confusion that caused
  the original complaint**: three related settings, three different
  resolution shapes, none of them documented together. An operator
  had no way to reason about "what's my chat model" without first
  understanding OpenAI-vs-Ollama branching that only applied to TWO
  of the three seams.
- **Subject-gen's "unset = off, no default" design meant the feature
  could never turn on with zero extra config**, unlike chat/embedding,
  which always resolve to something usable. This is why it "looked
  like it should work" (get_system_prompt and the codebase's own docs
  describe it as a real feature) but silently never fired.

Separately: this deployment's real completion/embedding backend is
`big-test-server-01` (added same session, a plain Ollama instance) —
there was no real OpenAI cloud usage anywhere in this project, and
none planned; dvv's own stated preference is to potentially add a
generic external-provider mechanism LATER (e.g. OpenRouter) rather
than keep a single-cloud-vendor branch that only ever pointed at
`api.openai.com`.

## Decision

1. **Remove the OpenAI cloud-provider branch entirely** from
   `completion_client.rs` and `embedding_client.rs` — no
   `OPENAI_API_KEY`/`OPENAI_MODEL`/`OPENAI_BASE_URL`, no
   `api.openai.com`/`text-embedding-3-large`/1536-dim cloud fallback,
   no `CompletionConfigError` (its only trigger, an API key set
   without a model, no longer has a code path to trigger from).
   `completion_client::resolve` becomes infallible. The wire format
   (OpenAI-shaped `/v1/chat/completions` + `/v1/embeddings` JSON)
   stays — that's the de-facto standard shape Ollama, llama.cpp, and
   a future OpenRouter integration all speak; only the CLOUD-PROVIDER
   branch and its vars are gone.

2. **One shared base-url var for every LLM seam.** `CONEXUS_LLM_BASE_URL`
   now resolves chat, embedding, AND subject-gen (previously
   embedding used its own `OPENAI_BASE_URL`/cloud default, and
   subject-gen deliberately kept itself separate from both). Chosen
   over keeping per-seam base URLs (which `nix/vm.nix`'s "external"
   dev-VM mode used, to point chat at a fast iGPU llama.cpp and
   embeddings at a separate Ollama simultaneously) -- dvv confirmed
   losing that split-endpoint capability is acceptable, since every
   real deployment (including `big-test-server-01`) already runs
   chat+embedding off the same Ollama instance.

3. **`OLLAMA_MODEL` renamed to `CONEXUS_CHAT_MODEL`** (default
   `qwen3:1.7b`, unchanged) -- matches `CONEXUS_EMBEDDING_MODEL`/
   `CONEXUS_SUBJECT_MODEL`'s naming, closing the "three vars, three
   different name shapes" inconsistency.

4. **Every model-name var always has a real default now — "unset"
   is no longer a magic "feature off" sentinel for any of the
   three.** `CONEXUS_SUBJECT_MODEL` defaults to whatever
   `CONEXUS_CHAT_MODEL` resolves to (reuse, not a new default) rather
   than requiring its own separately-typed model name.

5. **Subject-gen gets its own, SEPARATE on/off switch:
   `CONEXUS_ENABLE_SUBJECT_GEN`, default `true`.** This is the actual
   fix for the root complaint: "should this feature run at all" and
   "which model would it use if it ran" are two different questions
   that were previously conflated into one env var's presence/absence.
   Chat and embedding have no equivalent toggle because they are
   REQUIRED subsystems (RAG doesn't function without them) — there is
   no meaningful "off" state to switch. Subject-gen is the one
   genuinely optional feature of the three (the truncated-body-preview
   fallback is a legitimate steady state, not a degraded error), so
   it's the one that gets a dedicated flag — same pattern this
   codebase already uses for `CONEXUS_DISABLE_AUTO_INDEXING`
   (`background_tasks::rag_indexing`), just inverted polarity (an
   ENABLE flag defaulting on, not a DISABLE flag defaulting off).

   Consequence, confirmed accepted by dvv: subject-gen being on by
   default means every `send_agent_message`/dashboard-compose call
   without an explicit subject now costs a real LLM round-trip before
   the message is stored (same latency profile as a RAG completion
   call — this deployment's own `qwen3:1.7b` has been measured at
   26-67s on CPU). Accepted as-is (the model is already warm/loaded
   for RAG anyway) rather than made fire-and-forget/best-effort.

6. **Zero content-based inference, zero cross-table lookups anywhere
   in this resolution** — every var above is read straight from
   `get_env`, nothing is inferred from database state or another
   config value's content. (Not a new principle for this ADR
   specifically, but worth stating: this is the same "no exposure/
   behavior without a hard, explicit signal" discipline ADR-0030
   applied to message-visibility scoping, applied here to config
   resolution instead.)

## Consequences

### Positive

- One config shape across all three LLM seams: `CONEXUS_LLM_BASE_URL`
  (shared endpoint) + `CONEXUS_{CHAT,EMBEDDING,SUBJECT}_MODEL` (one
  naming family, every one with a real default). An operator can
  reason about "what model does X use" identically for all three.
- Subject-gen actually works out of the box now — the root complaint
  ("this looks like it should work but doesn't") is fixed at the
  design level, not patched around.
- Smaller `completion_client`/`embedding_client` — one resolution path
  each instead of two, `CompletionConfigError` and its whole "hard
  config error" failure mode deleted along with the branch that could
  trigger it.
- A future generic external-provider mechanism (OpenRouter, or
  anything else OpenAI-wire-compatible) has a clean, single place to
  land — `CONEXUS_LLM_BASE_URL` already IS "any compatible endpoint,"
  it just currently always resolves to Ollama's own defaults absent an
  override. No half-finished cloud-provider abstraction to either
  extend awkwardly or rip out first.

### Negative / trade-offs

- **Breaking config-var rename**: any existing deployment setting
  `OLLAMA_MODEL` or relying on the `OPENAI_API_KEY` cloud branch stops
  working after this change, silently reverting to the Ollama
  defaults rather than erroring (no compatibility shim). Acceptable
  here because this project has exactly one real deployment
  (IT-03743), updated in the same change.
- `nix/vm.nix`'s "external" dev-VM mode loses the ability to split
  chat and embeddings onto different host:port pairs (previously used
  to point a fast iGPU llama.cpp at chat while a separate Ollama
  served embeddings). Confirmed acceptable by dvv — no real deployment
  exercises that split today.
- Subject-gen being on-by-default adds real per-message latency to
  message-sending that wasn't there before for any deployment that
  never bothered to configure `CONEXUS_SUBJECT_MODEL` (which, given
  the prior "no default" design, was every deployment until now).

## Alternatives considered

- **Keep the OpenAI branch, just fix subject-gen's default separately**
  — rejected; the OpenAI branch had zero real usage in this project
  and was the direct SOURCE of the naming/shape inconsistency dvv
  flagged (`OLLAMA_MODEL` vs `CONEXUS_EMBEDDING_MODEL` vs
  `CONEXUS_SUBJECT_MODEL` only exists because two of the three had a
  second, differently-named branch to accommodate). Removing it was
  the actual fix, not scope creep.
- **Subject-gen model presence stays the on/off switch, just give it a
  default value** — rejected during design (see the session's own
  back-and-forth): this is structurally impossible to do safely,
  since "model defaults to X" and "feature is on by default" become
  the same statement under that mechanism — there's no way to have a
  sensible default model name WITHOUT also flipping the feature on.
  Splitting the concerns into two variables was the only design that
  gives both a real default AND a genuine off switch.
- **Keep independent base-url vars per seam** (`CONEXUS_LLM_BASE_URL`
  for chat, a new `CONEXUS_EMBEDDING_BASE_URL` for embeddings) —
  considered, then explicitly rejected by dvv in favor of one shared
  var, trading away the split-endpoint capability for a simpler
  single-var story that matches every real deployment's actual
  topology.

## Links

- `rust/conexus-tools/src/completion_client.rs` — the removed OpenAI
  branch, renamed `CONEXUS_CHAT_MODEL`, infallible `resolve`.
- `rust/conexus-tools/src/embedding_client.rs` — the removed OpenAI
  branch, shared `CONEXUS_LLM_BASE_URL`.
- `rust/conexus-tools/src/message_suggestions.rs` —
  `subject_gen_enabled`/`resolve_subject_model`, the on/off-vs-model
  split.
- `nix/vm.nix` — `externalLlmEnvironment`'s collapse to one
  `llmPort`/`llmBaseUrl`, and `conexus-llm-endpoint-check.service`'s
  single-endpoint probe.
- `nix/tests/fake-llm.nix` (renamed from `fake-openai.nix`) — the
  VM-test embeddings stub, now just an Ollama-shaped fixture with no
  OpenAI-specific framing.
- `docs/operator/getting-started.md`,
  `docs/operator/local-embeddings-guide.md`, `docs/operator/vm.md` —
  updated operator-facing config docs.
- [ADR-0030](0030-agent-message-rag-indexing.md) — the immediately
  prior ADR in this same investigative thread (dvv testing
  `ask_project_rag` after the Ollama/RAG infra fix), and the precedent
  for resolving a config/design question directly with dvv before
  implementation.

## Status of implementation

Implemented 2026-09-28:
- `completion_client.rs`/`embedding_client.rs`: OpenAI branches
  removed, infallible `resolve`, shared `CONEXUS_LLM_BASE_URL`,
  `CONEXUS_CHAT_MODEL` rename. Full test-suite rewrite for both
  modules' removed/changed branches.
- `message_suggestions.rs`: `subject_gen_enabled` (new,
  `CONEXUS_ENABLE_SUBJECT_GEN`, default on) + `resolve_subject_model`
  (reuses the chat model by default) replace the old
  `subject_model_configured` conflation. Callers in
  `background_tasks.rs`/`rest_handlers.rs` updated.
- `rag_tools.rs`: `RagQueryError` collapsed to a single unit struct
  (its "config error" variant's only trigger no longer exists).
- `nix/vm.nix`/`nix/vm-dev.nix`: `externalLlmEnvironment` rewritten
  for the shared base URL; `llmChatPort`/`llmEmbeddingPort` collapsed
  to one `llmPort` (default 11434); the fail-loud boot probe now
  checks one endpoint instead of two.
- `nix/tests/fake-openai.nix` renamed to `fake-llm.nix`; its 4 VM-test
  callers (`event-driven-coord.nix`, `multi-tenant.nix`,
  `no-auto-cleanup.nix`, `single-tenant.nix`) plus `module-parity.nix`
  and the hardening-baseline check script updated to match.
- Docs updated: `getting-started.md` (env-var table rewritten),
  `local-embeddings-guide.md` (OpenAI-comparison framing removed
  throughout), `vm.md` (external-mode parameter table + env block),
  `mcd-example.md` (env-var bullets), `docs/README.md` (one-line
  description).
- Verification: full Rust workspace test suite green (2,000+ tests).
  2 of 5 relevant NixOS VM checks (`vm-single-tenant`,
  `vm-module-parity`) run and confirmed green end-to-end (both import
  the renamed `fake-llm.nix` fixture and boot successfully). The
  remaining 3 (`vm-multi-tenant`, `vm-no-auto-cleanup`,
  `vm-event-driven-coord`) are blocked by this host's nix-daemon
  missing the `uid-range` system-feature required for their
  container-based (`systemd-nspawn`) test variant — a pre-existing
  host environment gap, unrelated to this change (would fail
  identically against unmodified `main`), not fixed here per the
  policy of not making unrelated host/infra changes as a side effect
  of an unrelated code change.

## Addendum: subject-gen's on-by-default flip surfaced a real, separate bug

Deploying the above to IT-03743 and testing subject-gen end-to-end
(now that it actually runs, for the first time ever in this project)
surfaced a genuine defect this ADR's config unification did not
create, but did expose: `qwen3:1.7b` (the model subject-gen defaults
to reusing) is a REASONING model. Called through the shared
OpenAI-compatible `/v1/chat/completions` path with
`MAX_COMPLETION_TOKENS = 32`, it spends its entire token budget on
hidden `reasoning` output and never emits real `content` -- silently
empty every time, degrading to the old truncated-body-preview
fallback with no visible error. Verified directly against the real
`big-test-server-01` server:

- The OpenAI-compat `/v1/chat/completions` endpoint does NOT forward
  Ollama's `"think"` field at all -- setting it has zero effect there.
- Appending the documented Qwen3 `/no_think` directive to the prompt
  also has no effect over that endpoint.
- Raising the token budget to 200 doesn't reliably fix it either --
  this model can reason at length even for a trivial 6-word task and
  still never reach a real answer within a generous budget.
- Ollama's OWN native endpoint, `/api/chat`, DOES honor `"think":
  false` correctly (confirmed via direct `curl`) -- the compat layer
  is the gap, not the model or the request field.

**Fix**: `message_suggestions::suggest_subject` now bypasses
`completion_client::CompletionClient::chat`'s portable `/v1` path
entirely for this one seam, calling Ollama's native `/api/chat`
directly (`ollama_native_chat_no_think`, `ollama_native_chat_url`)
with `"think": false`. This is intentionally NOT a portable,
provider-agnostic client -- Ollama's native API is Ollama-specific,
which is fine here because subject-gen has always been documented as
"always wants a LOCAL Ollama endpoint" (this module's own original
doc, predating this ADR). RAG's own chat call is unaffected and stays
on the portable `/v1` path -- it passes no `max_tokens` cap, so the
reasoning model has room for both the thinking phase and the real
answer either way; this problem is specific to small, fixed-budget
completions, of which subject-gen is the only one in this codebase.

Verified against the real deployed server: the exact request shape
`ollama_native_chat_no_think` builds now returns real subject text
(`"Deploy to staging failed with timeout"`) instead of an empty
string, confirmed via direct `curl` before deploying the fix.
