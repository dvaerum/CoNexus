# Standing requirements for agents and contributors

This file is tracked in the repo (unlike `CLAUDE.md`, which is
gitignored/personal-only) — anything here needs to survive across
sessions, clones, and contributors, not just live in one person's local
config.

## NixOS module and home-manager module must stay at feature parity

`nix/module.nix` (NixOS) and `nix/home-manager-module.nix`
(home-manager) must both work correctly, and must be kept at feature
parity with each other. This is a standing requirement, not a
nice-to-have — it has already drifted silently once: as of 2026-09-24,
`module.nix` had fallen to ~10 options against
`home-manager-module.nix`'s ~31, with zero SSO/OIDC support, zero
proxy-header trust config, no `multiTenant`/`singleProject`
distinction, and no daemon-agent packaging at all. `flake.nix`'s own
comment labeling `module.nix` "legacy, used by VM tests only" reads
like it justifies that gap — it doesn't. It's a maintenance-activity
observation, not license to let the module fall further behind.

**Any change that adds an option, systemd unit, or piece of config
logic to one module needs the equivalent considered for the other, in
the same change — not deferred to "later" or a follow-up.**

Full design rationale, and what's genuinely shareable between the two
module shapes vs. what has to stay split (home-manager's `%h`/`%t`
systemd specifiers and `config.xdg.*` vs. NixOS's `cfg.stateDir`/
`users.users`/`security.polkit`), is recorded in
[`docs/adr/0029-nixos-home-manager-module-parity.md`](docs/adr/0029-nixos-home-manager-module-parity.md).
