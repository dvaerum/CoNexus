# ADR 0029: NixOS/home-manager module parity — requirement, and why options aren't unified

**Status**: Accepted, 2026-09-24.
**Scope**: `nix/module.nix` (NixOS system module) and
`nix/home-manager-module.nix` (home-manager module).

## Context

Another agent, working in a sibling deploy repo, read `nix/module.nix` and
concluded it had "no existing pattern for an always-on service independent
of a login session," and proposed hand-rolling a new systemd unit instead
of reusing this repo's existing home-manager module. That conclusion was
partly right and partly wrong: a real, persistent, systemd `--user`-scoped
`conexus-router.service` has run continuously on this operator's own
machine for days across many separate login sessions (lingering not even
explicitly enabled) — the pattern exists and works. But investigating the
claim surfaced the real, larger problem underneath it: `nix/module.nix`
had drifted badly behind `nix/home-manager-module.nix` — 10 options vs
~31, zero SSO/OIDC, zero proxy-header trust config, no
`multiTenant`/`singleProject` distinction, and no daemon-agent support at
all. `flake.nix`'s own comment calling `module.nix` "legacy, used by VM
tests only" read like license for the gap rather than a problem to fix.

The operator confirmed this parity requirement had been stated before,
and had silently drifted anyway — the requirement itself was never
written down anywhere durable, only held in conversation memory across
sessions. That's the direct cause this ADR + `AGENTS.md` fix.

## Decision

1. **`nix/module.nix` and `nix/home-manager-module.nix` must be kept at
   feature parity.** Any option, systemd unit, or piece of config logic
   added to one gets the equivalent considered for the other in the same
   change — not deferred. This is now recorded durably in `AGENTS.md`
   (repo-tracked, unlike the gitignored `CLAUDE.md`) so it survives
   across sessions and contributors, not just one operator's memory.
2. **Option *declarations* stay split between the two files — they are
   not unified into one shared options file.** Only *logic* (plain
   functions/data, no module-system machinery) is shared, via
   `nix/conexus-exec.nix` (new, mirroring the existing
   `nix/hardening.nix` pattern).
3. **Verification for new capability must be real, not just eval-clean.**
   `nix/tests/module-parity.nix` (new) is a `pkgs.testers.nixosTest`
   importing the *real* `nix/module.nix` (not a hand-copy, unlike this
   repo's 4 pre-existing VM tests) and exercises `multiTenant`/
   `singleProject`, `sso.proxyHeader`, and `daemonAgents` at runtime —
   a real HTTP round-trip and a real daemon-agent unit reaching the
   router, not just a closure that builds.

## Why options aren't unified into one shared file

Checked directly, not assumed: two real, independent precedents in the
Nix ecosystem were read in full before deciding.

- **sops-nix** declares its options separately for NixOS, nix-darwin,
  and home-manager (3 platforms) — no shared `options.nix`.
- **This operator's own `dvaerum/nixpkgs-lib-extensions`** has a parallel
  `systemAutoUpgradeModule`/`homeManagerAutoUpgradeModule` pair, options
  declared separately in each.

Both frameworks run the identical `lib.evalModules` under the hood, so
option *types* are mechanically shareable — but the config each option
wires into lives at structurally incompatible paths:

| | NixOS (`module.nix`) | home-manager (`home-manager-module.nix`) |
|---|---|---|
| systemd scope | `systemd.services` | `systemd.user.services` |
| user creation | `users.users.<name>` (a real system user) | none — runs as the invoking user |
| path specifiers | absolute (`cfg.stateDir`, `cfg.runtimeDir`) | systemd `%h`/`%t` + `config.xdg.dataHome` |
| privilege escalation | `security.polkit` rules | none — user-scope has no privilege boundary to cross |

A shared `options.nix` would still need every option's *description* and
*default* to differ per-framework (an absolute `stateDir` default makes
no sense against `%h`-relative home-manager paths, and vice versa) — the
options layer earns nothing from unification that the logic layer
doesn't already capture more precisely. Real dedup in this ecosystem
happens at the **logic** layer, which this repo already did once via
`nix/hardening.nix` (shared systemd-hardening attrset, consumed by both
modules) before this ADR's own work extracted the router `ExecStart`
flag-string builder and the `forwarding_hmac`-key `ExecStartPre` seeding
script into `nix/conexus-exec.nix` the same way.

## What's genuinely shared vs. deliberately split

**Shared** (`nix/conexus-exec.nix`, `nix/hardening.nix`, `nix/conexus.nix`):
package builds (all 3 conexus binaries + wrapper scripts — already
shared, unaffected by this work), systemd hardening attrset, the
router's `ExecStart` flag-string construction, the `forwarding_hmac`
key-seeding `ExecStartPre` shell one-liner.

**Deliberately split** (real, structural, not maintenance debt): systemd
scope (`systemd.services` vs `systemd.user.services`), user/group
creation and `security.polkit` rules (NixOS-only — home-manager has no
system-user or privilege-escalation concept), path specifier style
(`%h`/`%t` + `config.xdg.dataHome` vs `cfg.stateDir`/`cfg.runtimeDir`),
and the option declarations themselves (per the ecosystem precedent
above).

## Consequences

- Future option/unit additions to either module must consider the other
  in the same PR — enforced by `AGENTS.md`, not just convention.
- `nix/tests/module-parity.nix` catches a regression in `module.nix`'s
  newest option surface that a pure-eval check would miss (it already
  did, during this ADR's own implementation — see PR history for
  `conexus-module-parity`).
- The 4 pre-existing VM tests (`single-tenant.nix`, `multi-tenant.nix`,
  `event-driven-coord.nix`, `no-auto-cleanup.nix`) still hand-roll a
  parallel config rather than importing the real module — a real, known
  gap, deliberately left out of this ADR's scope (see `AGENTS.md`'s own
  PR notes) as its own follow-up.
