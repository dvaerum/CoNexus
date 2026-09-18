#!/usr/bin/env bash
# Regression guard: `CONEXUS_BOOTSTRAP_PASSWORD` must never be rendered as a
# literal into a `conexus-router.service` unit file.
#
# Confirmed pentest finding (MEDIUM, class: "secret inlined via nix
# `environment =` instead of a file reference"): nix/vm-dev.nix and four
# nix/tests/*.nix VM-test fixtures wired CONEXUS_BOOTSTRAP_PASSWORD via
# `systemd.services.conexus-router.environment = { ... }`, which bakes the
# plaintext password directly into the rendered systemd unit file under
# /nix/store/... -- world-readable (444) by design, since Nix store paths
# are never access-controlled. Confirmed live: an unprivileged local user
# (`nobody`) could read the operator bootstrap password straight out of the
# store path backing /etc/systemd/system/conexus-router.service.
#
# Fix: a dedicated, EARLIER prerequisite oneshot unit
# (conexus-router-bootstrap-seed) copies the password into a 0600
# runtime-only file BEFORE conexus-router.service's own activation
# begins; that unit is referenced via `EnvironmentFile=`, which systemd
# renders into the unit file as a *path*, never the value itself. A
# same-unit `ExecStartPre=` doesn't work here -- `EnvironmentFile=` is
# loaded once per unit activation, before ANY of that unit's own exec
# steps (including its own ExecStartPre) run, so the file must already
# exist by the time this unit's activation starts. CONEXUS_BOOTSTRAP_USERNAME
# stays inline; it isn't a secret.
#
# This check evaluates each affected module's real `conexus-router.service`
# unit text -- the exact string systemd would load -- via
# `config.systemd.units."conexus-router.service".text`, and asserts:
#   1. no `Environment=` line carries a literal CONEXUS_BOOTSTRAP_PASSWORD
#      value;
#   2. an `EnvironmentFile=` line is present instead.
#
# Confirmed RED against the pre-fix code (each of the 5 files failed check
# 1: the rendered text contained
# `Environment="CONEXUS_BOOTSTRAP_PASSWORD=<plaintext>"` verbatim).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

if ! command -v nix >/dev/null 2>&1; then
  echo "SKIP: nix is not available on PATH"
  exit 0
fi

DRIVER=$(cat <<EOF
let
  flake = builtins.getFlake "$REPO_ROOT";
  system = builtins.currentSystem;
  lib = flake.inputs.nixpkgs.lib;
  pkgs = flake.inputs.nixpkgs.legacyPackages.\${system};
  crane = flake.inputs.crane;
  craneLib = crane.mkLib pkgs;

  vmDevConfig = (lib.nixosSystem {
    inherit system;
    specialArgs = { src = flake; craneLib = craneLib; };
    modules = [ $REPO_ROOT/nix/vm-dev.nix ];
  }).config;

  singleTenant = import $REPO_ROOT/nix/tests/single-tenant.nix {
    inherit pkgs lib craneLib;
    self = flake;
  };
  eventDrivenCoord = import $REPO_ROOT/nix/tests/event-driven-coord.nix {
    inherit pkgs lib craneLib;
    self = flake;
  };
  noAutoCleanup = import $REPO_ROOT/nix/tests/no-auto-cleanup.nix {
    inherit pkgs lib craneLib;
    self = flake;
  };
  multiTenant = import $REPO_ROOT/nix/tests/multi-tenant.nix {
    inherit pkgs lib craneLib;
    self = flake;
  };

  unitText = cfg: cfg.systemd.units."conexus-router.service".text;
in {
  "vm-dev" = unitText vmDevConfig;
  "single-tenant" = unitText singleTenant.nodes.machine;
  "event-driven-coord" = unitText eventDrivenCoord.containers.machine;
  "no-auto-cleanup" = unitText noAutoCleanup.containers.machine;
  "multi-tenant" = unitText multiTenant.containers.machine;
}
EOF
)

stderr_file="$(mktemp)"
trap 'rm -f "$stderr_file"' EXIT

json=$(NIX_CONFIG="experimental-features = nix-command flakes" \
  timeout 1800 nix eval --impure --json --expr "$DRIVER" 2>"$stderr_file") \
  || { echo "FAIL: nix eval failed:" >&2; cat "$stderr_file" >&2; exit 1; }

fail=0

for name in vm-dev single-tenant event-driven-coord no-auto-cleanup multi-tenant; do
  unit_text=$(jq -r --arg n "$name" '.[$n]' <<<"$json")

  if grep -qE '^Environment="?CONEXUS_BOOTSTRAP_PASSWORD=' <<<"$unit_text"; then
    echo "FAIL: $name's rendered conexus-router.service still bakes" \
      "CONEXUS_BOOTSTRAP_PASSWORD's plaintext value directly into the" \
      "unit file via Environment= -- world-readable under /nix/store/..." \
      "See this script's header comment for the confirmed pentest finding." >&2
    fail=1
  fi

  if ! grep -q '^EnvironmentFile=' <<<"$unit_text"; then
    echo "FAIL: $name's rendered conexus-router.service has no" \
      "EnvironmentFile= line -- the bootstrap password fix expects the" \
      "value to be sourced from a private runtime file, not inlined." >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "PASS: bootstrap-password-not-inlined"
