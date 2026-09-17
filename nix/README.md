# conexus home-manager module

[Agent-MCP](https://github.com/rinadelph/Agent-MCP), packaged via
the [`dvaerum/CoNexus`](https://github.com/dvaerum/CoNexus)
fork, exposed as a home-manager module. Provides multi-agent
coordination tools (spawn sub-agents, assign tasks, shared messages,
per-project RAG over markdown) to claude-code sessions on the same
host or anywhere on a tailnet.

## What this module ships

Three groups of user-scope systemd units:

- **`conexus-router.service`** (the CoNexus Rust router; the retired
  Python one was `agent-mcp-router.service`) — always-on URL-keyed
  HTTP router on loopback (default `127.0.0.1:1337`). Serves the
  Next.js dashboard at `/conexus/app/`, proxies MCP traffic
  to per-project backends, lazy-starts/stops them by activity, and
  exposes the project-lifecycle REST API under `/conexus/api/router/`
  (ADR-0014).
- **`conexus@<name>.service`** (systemd template; the retired Python
  one was `agent-mcp@<name>.service`) — one instance per registered
  project, started lazily by the router on first MCP request, stopped
  after `services.conexus.router.idleSec` seconds of inactivity.
  Listens on a Unix domain socket under
  `$XDG_RUNTIME_DIR/conexus/<name>/backend.sock`.
- **`conexus-daemon-agent@<project>--<agent_id>.service`** (systemd
  template) — one instance per entry in
  `services.conexus.daemonAgents`. Runs an event-driven
  `wait_for_events` long-poll loop so the agent reacts to messages /
  task assignments without anyone keeping a Claude session open.

Project membership is **not** declared in nix. Every project is
registered at runtime via `POST /conexus/api/router/projects`
(dashboard form or `curl`, JSON body), recorded in
`~/.config/conexus/projects.local.json`.
The module materialises the router + systemd templates + the
daemon-agent wiring; the project list lives outside source control.

## Quick start

In your home-manager flake:

```nix
{
  inputs.agent-mcp.url = "github:dvaerum/CoNexus";

  outputs = { self, nixpkgs, home-manager, agent-mcp, ... }: {
    homeConfigurations."alice" = home-manager.lib.homeManagerConfiguration {
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      modules = [
        agent-mcp.homeModules.default
        {
          home.username = "alice";
          home.homeDirectory = "/home/alice";
          home.stateVersion = "25.11";

          services.conexus = {
            enable = true;
            router = {
              # Default; uncomment to change.
              # port = 1337;
              # idleSec = 14400;  # 4 hours

              # Used in dashboard wiring snippets so .mcp.json files
              # work from other devices on the tailnet, not only the
              # host's loopback.
              externalUrl = "https://my-host.tailfdae0.ts.net";

              # Where POST /conexus/api/router/projects puts a
              # project's workspace when the form's Workspace field
              # is empty.
              defaultWorkspaceParent =
                "/home/alice/.local/share/conexus/projects";
            };
            dashboard.enable = true;
            daemonAgents = [
              # Reference instance — keeps the wiring exercised on
              # every redeploy. Remove or replace with your own.
              {
                project = "washing-brothers";
                agentId = "backend-dev";
                tokenPath =
                  "/home/alice/.config/conexus/tokens/washing-brothers--backend-dev.token";
              }
            ];
          };
        }
      ];
    };
  };
}
```

After `home-manager switch`, the router boots on
`http://127.0.0.1:1337/conexus/`. Log in as the operator (see
[`docs/operator/getting-started.md`](../docs/operator/getting-started.md#first-boot-setup-operator-login)
for first-boot setup), then the first project you create through the
dashboard (or via `POST /conexus/api/router/projects` with a JSON
body `{"name": "foo"}` and the session cookie) appears in
`~/.config/conexus/projects.local.json` and shows up in the
dashboard's overview.

## Options reference

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `services.conexus.enable` | bool | `false` | Enable the router, template, and daemon-agent units. |
| `services.conexus.source` | path | `self` (the flake) | Source tree the module builds from. Override to pin a local checkout. |
| `services.conexus.pkgs` | package set | the consumer's `pkgs` | Package set every conexus derivation is built from. See [Building from a different nixpkgs](#building-from-a-different-nixpkgs). |
| `services.conexus.router.port` | port | `1337` | Router's loopback port. |
| `services.conexus.router.idleSec` | int (positive) | `14400` | Seconds of inactivity before the router stops a per-project backend. |
| `services.conexus.router.externalUrl` | str | (required) | Base URL the host can be reached at; used in `.mcp.json` snippets. |
| `services.conexus.router.defaultWorkspaceParent` | str | (required) | Where new project workspaces are created when the form's Workspace field is empty. |
| `services.conexus.dashboard.enable` | bool | `true` | Build and serve the Next.js dashboard. |
| `services.conexus.dashboard.package` | package | computed | Override the dashboard derivation. |
| `services.conexus.daemonAgents` | list of submodules | `[]` | Each entry expands to one `conexus-daemon-agent@<project>--<agent_id>.service` unit. |
| `services.conexus.multiTenant` | bool | `true` | `true`: the router runs multi-tenant (projects registered at runtime). `false`: single-tenant (N=1) — requires `singleProject` to be set; an assertion enforces the pairing. |
| `services.conexus.singleProject` | nullable submodule (`name`, `workspace`) | `null` | Required when `multiTenant = false`, must stay `null` when `true`. Declares the single project a single-tenant deploy serves. |
| `services.conexus.sso.oidc` | nullable submodule | `null` | OIDC authorization-code+PKCE config (`issuer`, `clientId`, `clientSecretFile`, `providerName`, `groupMapping`, `scopes`, `redirectUrl`). Mutually exclusive with `sso.proxyHeader` — the router refuses to start if both are set. |
| `services.conexus.sso.proxyHeader` | nullable submodule | `null` | Trust-an-upstream-proxy SSO config (`trustHeader`, `trustedIps`, `defaultIsSysadmin`). Mutually exclusive with `sso.oidc`. |
| `services.conexus.conexusLauncherPackage` | nullable package | `null` (auto-wired by the flake's `homeModules.default`) | The `conexus@<name>.service` backend launcher. `null` with no override (i.e. bypassing the flake wrapper) ships no per-project backend at all. |
| `services.conexus.conexusRouterPackage` | nullable package | `null` (auto-wired by the flake's `homeModules.default`) | The `conexus-router` singleton service package. `null` with no override ships no router at all. |
| `services.conexus.conexusDaemonAgentPackage` | nullable package | `null` (auto-wired by the flake's `homeModules.default`) | The daemon-agent binary for every `daemonAgents` unit. `null` with no override omits every daemon-agent unit entirely. |

The three `conexus*Package` options are auto-wired to a `crane`-built
binary when the module is consumed via this flake's own
`homeModules.default` (the normal path, shown under
[Quick start](#quick-start) above) — they only matter directly if
you're importing `home-manager-module.nix` standalone without the
flake wrapper.

Each `daemonAgents` entry has:

| Sub-option | Type | Description |
|------------|------|-------------|
| `project` | str | Project slug (must exist — create it via `POST /conexus/api/router/projects` first). |
| `agentId` | str | Agent slug (must exist on the project). |
| `tokenPath` | str | Absolute path to the file holding the agent's bearer token. |

## Building from a different nixpkgs

The module builds agent-mcp from the **consumer's** package set — the
`pkgs` your home-manager configuration was evaluated with. agent-mcp's
own flake pin has no say in it, so adding or dropping
`inputs.agent-mcp.inputs.nixpkgs.follows` in your flake changes only
what `nix build` inside *this* repo produces, never your deployed
closure.

That matters when the host tracks a NixOS **stable** branch. Stable
branches don't take routine Python security backports, so the router's
aiohttp stays on whatever that branch froze on for the life of the
release, advisories included, while unstable already ships the fix.
`services.conexus.pkgs` rebuilds conexus — and only conexus —
from a set you choose, leaving the rest of the profile on stable:

```nix
{ agent-mcp, pkgs, ... }:

{
  imports = [ agent-mcp.homeModules.default ];

  services.conexus.pkgs = import agent-mcp.inputs.nixpkgs {
    inherit (pkgs.stdenv.hostPlatform) system;
  };
}
```

The Python tree, the interpreter the units exec, the wrappers and the
dashboard all come from that one set — mixing them is not offered,
because a wrapper that execs one channel's interpreter against another
channel's site-packages does not run. That is also why there is no
single-derivation override; `services.conexus.package` was removed
(setting it now fails evaluation with a pointer here). The full
rationale is in the `pkgs` option's description in
[`home-manager-module.nix`](./home-manager-module.nix).

## Daemon-agent tokens

Each declared daemon-agent reads its bearer from `tokenPath`. The
file is operator-provisioned — for first-day setup, a chmod-0600
plaintext file is fine:

```sh
install -m 600 /dev/stdin \
  ~/.config/conexus/tokens/washing-brothers--backend-dev.token \
  <<<'<bearer-token-from-dashboard>'
```

For production hosts, wire `tokenPath` through
[`sops-nix`](https://github.com/Mic92/sops-nix) so the token lands
in the right place at activation time. The module does not enforce
any particular provisioning mechanism; the runtime cares only that
the path resolves to a readable file containing the token.

## URL surface

The router exposes (see `rust/conexus-router/src/main.rs`'s real
route table for the authoritative list — this is a summary, not
exhaustive):

| URL | Purpose |
|-----|---------|
| `GET /conexus/` | JSON service descriptor, or a redirect to the dashboard for a browser (`Accept: text/html`). |
| `GET /conexus/app/` | Next.js dashboard. |
| `GET/POST /conexus/api/router/projects` | List / register a project (JSON body: `{"name": ..., "workspace": ...}`). |
| `PATCH/DELETE /conexus/api/router/projects/{name}` | Rename (creates a grace-period alias; ADR-0010) / unregister a project. |
| `POST /conexus/api/router/projects/{name}/stop` | Stop a project's backend (refuses if busy). |
| `GET /conexus/api/router/projects/{name}/aliases` | Retired-alias usage for a project. |
| `DELETE /conexus/api/router/projects/{name}/aliases/{alias}` | Drop a grace-period alias early. |
| `GET /conexus/api/router/overview` | Cross-project status summary. |
| `GET/POST /conexus/api/router/users`, `.../groups` (+ `PATCH`/`DELETE .../{id}` on each), `GET/POST .../groups/{id}/members` (+ `DELETE .../members/{member_id}`), `GET/PUT .../groups/{id}/capabilities` | Operator/group/capability admin surface. |
| `GET/POST /conexus/api/router/projects/{name}/memberships`, `PATCH`/`DELETE .../memberships/{id}` | Per-project membership admin. |
| `GET /conexus/api/router/sso/config` | SSO configuration readback. |
| `ANY /conexus/mcp/{name}` | Streamable HTTP MCP endpoint for `{name}`. |

Every `/conexus/api/router/*` route above is also mounted with an
`/conexus/api/router/.../` trailing-slash twin and a root-mounted
alias without the `/conexus` prefix (ADR-0020, mount-agnostic). No
`client-config`/`installer` curl-installable-snippet route exists
today — the router's own CLI carries an `--installer-template` flag
wired up alongside those routes, but they're deferred indefinitely
(confirmed inert token plumbing in production).

Tailnet exposure (`externalUrl`) is configured outside this module
— e.g. via `services.tailscale.serve` at NixOS scope. See the
[deployment example](https://github.com/dvaerum/nixos-developer-system).

## Asset prefix

The dashboard's static export embeds a literal sentinel string
(`__CONEXUS_ASSET_PREFIX__`) wherever Next.js would normally bake
in `assetPrefix`. The router substitutes the configured runtime
prefix into served HTML / JS / CSS bodies on the fly so a single
build artifact serves any deployment URL.

* **Default**: assets serve at `/conexus/assets` (the router's own
  route table). Operators who deploy the router straight onto loopback
  (the documented path) need no configuration.
* **Custom mount**: the prefix is resolved per-request
  (`mount::external_prefix`), not from a static flag — a request that
  arrived under `/conexus` gets that prefix; a request a TRUSTED
  reverse proxy forwarded with an `X-Forwarded-Prefix` header gets
  that header's value instead (only honoured when the peer is on the
  trusted-proxy list). Deploying behind a reverse proxy mounted at a
  different prefix (e.g. `/tools/`) means configuring that proxy to
  send `X-Forwarded-Prefix: /tools` and trusting its source address —
  no router rebuild or restart-time flag needed. (The `--asset-prefix`
  CLI flag is accepted and stored on `RouterState` but never read
  anywhere after that — don't rely on it. There is no
  `CONEXUS_ASSET_PREFIX` env var wired to it at all; the only place
  that string appears in the source is the unrelated build-time
  sentinel `__CONEXUS_ASSET_PREFIX__` Next.js bakes in, a different
  mechanism entirely.)

Substitution is Content-Type-gated (`rust/conexus-router/src/asset_prefix.rs`'s
`SUBSTITUTABLE_CTYPE_PREFIXES`): `text/html`, `text/css`,
`application/javascript`, `text/javascript`, `text/plain`, and
`text/x-component` (the last two cover Next.js's RSC flight payloads,
which also carry the sentinel as a plain string). JSON API responses,
fonts, images, and other binary assets pass through verbatim, so
substitution can never corrupt their bytes even if a chance sequence
happens to match the sentinel.

**Important — single-tenant requires a router-fronted serve.** Per
[ADR-0008](../docs/adr/0008-single-tenant-url-parity.md), the router
runs in both single-tenant and multi-tenant modes. Substitution
happens at the router, so deploying the dashboard without the router
(e.g. serving the static export directly from nginx) would leak the
sentinel into served bytes and render the dashboard blank. Don't.

## Multi-tenant vs. single-tenant

`services.conexus.multiTenant` (default `true`, [ADR-0008](../docs/adr/0008-single-tenant-url-parity.md))
picks the deployment shape. `true` (default): the router runs
multi-tenant, projects are registered at runtime, `singleProject` must
stay `null`. `false`: single-tenant (N=1) — `services.conexus.singleProject`
(`name`/`workspace`) declares the sole project, the module seeds
`projects.local.json` before the router starts, and the router 410s
every project-lifecycle write endpoint plus 302-redirects any
wrong-project URL to the configured one. An assertion enforces the
`multiTenant`/`singleProject` pairing at evaluation time, not at
runtime.

## Wiring claude

Once the router is up, point claude at any registered project's MCP
endpoint. The dashboard's "Wiring help" panel shows three ready-to-paste
recipes per project:

1. **One-line installer** (`curl … | bash`) — the UI still offers
   this, but the router-side route it curls
   (`.../projects/{name}/installer`) is currently NOT registered
   (deferred indefinitely — confirmed inert token plumbing in
   production; see `rust/conexus-router/src/main.rs`'s
   `installer_template` doc comment). Running this recipe today 404s.
   Use recipe 2 or 3 instead until that route lands.
2. **Raw `.mcp.json` snippet** — paste into an existing
   `.mcp.json`'s `mcpServers` block.
3. **`claude mcp add`** invocation — writes to `~/.claude.json`
   (user scope, not project scope).

All three use the Streamable HTTP transport (`type: http`). The
legacy SSE pair (`type: sse`) was retired in `dvaerum/CoNexus`
3.0.0.

## Architectural background

| Decision | Doc |
|----------|-----|
| Single-tenant runs the router too (URL parity) | [ADR-0008](../docs/adr/0008-single-tenant-url-parity.md) |
| Dashboard owns the ops surface (no `/__admin/`) | [ADR-0009](../docs/adr/0009-dashboard-owns-ops-surface.md) |
| Project rename uses alias-with-grace, agent warning via `serverInfo.instructions` | [ADR-0010](../docs/adr/0010-rename-alias-with-grace.md) |

For the operator's day-to-day workflow (creating projects, wiring
clients, running daemon agents), see the in-dashboard help panel
once the router is running.
