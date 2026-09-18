# CoNexus — NixOS VM for end-to-end testing

This directory ships a Nix flake that boots a self-contained NixOS
VM running the full conexus deployment (router, per-project
backends, dashboard, local Ollama embeddings). The VM is built to be
the smallest reproducible mirror of the production deployment that
still lets you point a real client at `http://localhost:5454` and
get a working `.mcp.json` back.

## Prerequisites

- Linux host (x86_64). macOS qemu is out of scope.
- Nix with flakes enabled (`experimental-features = nix-command flakes`
  in `~/.config/nix/nix.conf`).
- KVM-capable kernel (the VM falls back to TCG, ~5x slower boot,
  but works).
- ~3 GB free disk for the closure + first-boot Ollama download.
- ~4 GB free RAM for the guest — or ~2 GB with `llm = "external"`,
  which moves the LLM/embedding endpoints onto the host (see
  [LLM endpoints](#llm-endpoints-in-guest-internal-vs-on-host-external)).

QEMU itself is brought in by the flake.

## Quick start

```sh
# Multi-tenant (default): router only — no projects auto-created.
nix run github:dvaerum/CoNexus

# Once the boot output stops scrolling, open the dashboard:
xdg-open http://localhost:5454/conexus/
# First boot lands on /setup — create the first operator. Subsequent
# boots land on /login. Create projects from the dashboard UI after
# signing in (the legacy `POST /conexus/__create` form-encoded
# endpoint was retired in ADR 0014).
```

State persists to `./vm-persistent-data/` in the directory you ran
the command from; nothing leaks into `~`. Ctrl-C cleanly shuts the
VM down.

There is also an interactive dashboard-E2E sandbox:

```sh
nix run .#vm-dev        # dashboard on http://localhost:18080/conexus/
                        # operator dev/dev, root SSH on :18222 — DEV ONLY
```

It differs from the above in three ways documented in
`nix/vm-dev.nix`, one of which is that it runs with
`llm = "external"` and therefore needs LLM endpoints on the host —
see [LLM endpoints](#llm-endpoints-in-guest-internal-vs-on-host-external).

## Flags

```
nix run github:dvaerum/CoNexus -- [flags]
```

| Flag                  | Meaning                                                                                                       |
|-----------------------|---------------------------------------------------------------------------------------------------------------|
| (default)             | Multi-tenant router on guest:1337, no auto-created project, persistent storage at `./vm-persistent-data/`.    |
| `--ephemeral`         | Use a tmpdir for state; everything dies with the VM. Mutually exclusive with `--persist`.                     |
| `--persist DIR`       | Persistent state directory on the host (default `./vm-persistent-data/`).                                     |
| `--help`, `-h`        | Print usage and exit.                                                                                         |

The host always reaches the VM on `http://localhost:5454`, translated
to guest port 1337 via qemu user-mode hostfwd, bound to 127.0.0.1.

History: this VM used to also support `--minimal`, booting a
single-tenant conexus backend directly on guest TCP `:8080` with no
router in front of it. That shape ran the Python implementation and
was retired together with it — the Rust `conexus-backend` binary only
serves over a Unix domain socket, with no standalone TCP-port mode to
replace it. `--minimal` is gone; the router + per-project template
shape below is the only one this VM boots.

## Modes

### Multi-tenant (default, and only, shape)

Mirrors the production `nixos-developer-system` deployment, built on
the CoNexus Rust implementation (`rust/conexus-router`,
`rust/conexus-backend`):

- `conexus-router.service` — always-on proxy on `:1337` that fronts
  per-project backends and serves the static Next.js dashboard under
  `/conexus/`.
- `conexus@<name>.service` — systemd template; one instance per
  registered project, listening on a UDS at
  `/run/conexus/<name>/backend.sock`. Lazy-started by the router
  on first request, idle-reaped after 4 h.

Project creation goes through the dashboard's authenticated REST API
(`POST /api/router/projects`); see the post-Phase-1+2 router for the
URL convention. The legacy `conexus-bootstrap.service` that POSTed
to `/conexus/__create` on first boot was retired with ADR 0014 —
the `__create` endpoint no longer exists.

## State layout

Two independent persistence substrates side-by-side in the persist
directory (the `ollama/` half exists in `llm = "internal"` mode only —
`external` mode has no in-guest ollama and no 9p share):

```
./vm-persistent-data/
├── disk.qcow2                       # 8 GB sparse — conexus state
└── ollama/                          # 9p host share — Ollama models
    └── models/blobs/sha256-…        # ~610 MB qwen3-embedding blob

# Inside the VM:
/var/lib/conexus/                  # on disk.qcow2
├── projects.local.json              # {<name>: <path>} registry
├── router.db                        # operator identity store (sqlite)
└── projects/<name>/                 # workspace (SQLite DB in .agent/)
/var/lib/ollama/                     # 9p bind to ./vm-persistent-data/ollama/
└── models/                          # blobs/, manifests/
```

conexus state goes on the qcow2 because SQLite's WAL mode needs
real `fcntl` locks that 9p can't fake. Ollama state goes on 9p
because its blobs are plain files — that way, deleting
`disk.qcow2` to wipe conexus state doesn't force a ~610 MB
redownload of the embedding model. The two substrates are
independent: nuke either without disturbing the other.

With `--ephemeral` the wrapper mktemp's a fresh dir containing both
substrates and `rm -rf`s it on exit.

## LLM endpoints: in-guest (`internal`) vs on-host (`external`)

`nix/vm.nix` takes an `llm` parameter that decides where the chat and
embedding endpoints live. Pass it (plus any of the overrides below)
where `vm.nix` is imported; `nix/vm-dev.nix` is the worked example.

| | `llm = "internal"` (default) | `llm = "external"` |
|---|---|---|
| Ollama | `services.ollama` inside the guest, `loadModels` preloads `qwen3-embedding:0.6b` + `qwen3:1.7b` | not installed at all |
| Guest RAM (`memorySize`) | **4096 MB** | **2048 MB** |
| Guest disk (`diskSize`) | **8192 MB** | **4096 MB** |
| 9p `ollama-models` share | yes (`$CONEXUS_OLLAMA_DIR`) | no |
| Needs anything on the host | no — self-contained | yes — a live chat + embedding endpoint |

`internal` is what `nix run .#` (and every VM the CI workflow builds)
uses; it is unchanged. `external` exists because ~1.6 GB of preloaded
model weights is pure overhead for dashboard/E2E work that never
exercises RAG — and on a busy workstation the difference between a
4 GB and a 2 GB guest is often the difference between the VM starting
and not. 2048 MB is 512 MB above the 1536 MB the `nix/tests/`
nixosTests boot the same router+backend shape at in CI, leaving room
for real 1024-dimension embedding batches and a second concurrent
project backend.

### Reaching the host from the guest

`external` mode points the backend at **`10.0.2.2`**. QEMU's
user-mode ("slirp") networking puts the guest on a synthetic
`10.0.2.0/24` in which `10.0.2.2` is an alias for the host's own
loopback — so a host service bound to `127.0.0.1` is reachable from
inside the guest at `10.0.2.2`, with no bridge, no firewall hole and
no host port published to the network.

Defaults (all overridable at the import site):

| Parameter | Default | Purpose |
|---|---|---|
| `llm` | `"internal"` | mode switch |
| `llmHost` | `"10.0.2.2"` | qemu user-mode host alias |
| `llmChatPort` | `11435` | host llama-cpp |
| `llmChatModel` | `"qwen2.5:3b-instruct"` | |
| `llmEmbeddingPort` | `11434` | host ollama |
| `llmEmbeddingModel` | `"qwen3-embedding:0.6b"` | |
| `llmEmbeddingDimension` | `1024` | must match the model |

Chat and embeddings resolve from *different* env vars, so they can
live on different ports (a fast iGPU llama-cpp for completion, a CPU
ollama for embeddings). `external` mode sets, on the backend units
only:

```
CONEXUS_LLM_BASE_URL=http://10.0.2.2:11435/v1   # chat/completion
OPENAI_BASE_URL=http://10.0.2.2:11434/v1          # embeddings
OPENAI_API_KEY=external                           # non-empty sentinel
OPENAI_MODEL=qwen2.5:3b-instruct
CONEXUS_EMBEDDING_MODEL=qwen3-embedding:0.6b
CONEXUS_EMBEDDING_DIMENSION=1024
```

No Python change is involved — see `rust/conexus-tools/src/completion_client.rs`
and `embedding_client.rs` for the resolution rules those vars feed.

### Fail-loud endpoint probe

`10.0.2.2` answers whether or not anything is listening behind it, so
a host that forgot to start llama-cpp/ollama would otherwise give you
a VM that boots green and fails deep inside RAG indexing — a passing
E2E run against a backend with no embeddings. `external` mode
therefore adds `conexus-llm-endpoint-check.service`: a boot-time
oneshot that curls both `/v1/models` endpoints (10 attempts, 2 s
apart) and hard-fails, naming the exact URLs on the serial console, if
either is unreachable. Every unit that embeds or completes
(`conexus@`) `Requires` it, so a dead endpoint stops the backend
instead of degrading it. `conexus-router.service` is deliberately
*not* gated — it never talks to an LLM, and a dashboard that loads
plus an explicit "backend failed to start" is more diagnostic than a
refused connection.

## Exercising the VM manually

There is no standalone automated e2e harness that points at a live
VM URL today — the repo's real test coverage (Rust `#[test]`s,
dashboard `vitest` suites, `nix/tests/checks/`) runs against
in-process fixtures or `nix eval`/`nix build`, not a booted VM. To
exercise this VM by hand, drive it the same way a real client would:
log in at `http://localhost:5454/conexus/`, provision a per-agent
token from the dashboard, and curl
`http://localhost:5454/conexus/mcp/<project>` with that bearer (see
[`docs/integrations/external-mcp-client.md`](../integrations/external-mcp-client.md)
for the full request shape).

## Build artefacts

The flake exposes:

```
nix build github:dvaerum/CoNexus#conexus-backend      # Rust backend
nix build github:dvaerum/CoNexus#conexus-router       # Rust router
nix build github:dvaerum/CoNexus#conexus-dashboard  # static export
nix build github:dvaerum/CoNexus#vm-multi             # multi-tenant VM
nix build github:dvaerum/CoNexus#default              # wrapper script
```

`nix flake check` builds the dashboard + CoNexus Rust binaries
(CI-cheap) but skips the VM (CI-expensive). The repo's GitHub Actions
workflow gates the Rust crates + dashboard directly; the flake is
opt-in.

## Reusing the NixOS module standalone

```nix
{
  inputs.conexus.url = "github:dvaerum/CoNexus";
  outputs = { self, nixpkgs, conexus, ... }: {
    nixosConfigurations.example = nixpkgs.lib.nixosSystem {
      modules = [
        conexus.nixosModules.default
        ({ ... }: {
          services.conexus = {
            enable = true;
            src = conexus;
            externalUrl = "https://agent.example.com";
          };
        })
      ];
    };
  };
}
```

The module covers the systemd shape only — TLS termination
(nginx, tailscale serve, …) is the operator's job.

## Architecture notes

- The router is `conexus-router` (`rust/conexus-router`), the sole
  implementation now that the Python one and the `router.impl` A/B
  flip between them were retired. Two env knobs matter in the VM:
  `CONEXUS_SYSTEMCTL_MODE` switches between `systemctl --user`
  (production, home-manager) and plain `systemctl` (VM, where there's
  no per-user systemd instance), and `CONEXUS_ROUTER_HOST` lets the
  VM bind `0.0.0.0` for qemu hostfwd.
- A polkit rule (in `nix/module.nix`) grants the unprivileged
  `conexus` user permission to start/stop `conexus@*.service`
  units via systemd, so the router doesn't need root.
- In `llm = "internal"` mode (the default — see
  [LLM endpoints](#llm-endpoints-in-guest-internal-vs-on-host-external)),
  Ollama runs in-VM with `qwen3-embedding:0.6b` (1024-dim). The
  model (~610 MB) is downloaded on first boot to
  `./vm-persistent-data/ollama/` on the host (9p bind-mounted
  into the VM at `/var/lib/ollama`). Subsequent runs — including
  ones where you've deleted `disk.qcow2` — reuse the downloaded
  blob; no re-download unless you also wipe the `ollama/`
  subdir or use `--ephemeral`.
- `services.ollama` is run with `User=root` + selectively
  re-granted capabilities. The hardening NixOS normally applies
  (DynamicUser, bind-mounted private state dirs, `ProtectSystem`,
  empty `CapabilityBoundingSet`) collides with our 9p mountpoint;
  see `nix/vm.nix` for the override. Fine for an e2e-sandbox VM;
  not what you'd ship as a public-facing module.
