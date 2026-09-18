#!/usr/bin/env bash
# Regression guard: every conexus-owned systemd unit must merge the
# shared hardening baseline (nix/hardening.nix) into its serviceConfig.
#
# Background (pentest class-sweep, LOW finding)
# ----------------------------------------------
# nix/vm-dev.nix's own dev-support units (conexus-vm-dev-bootstrap-seed,
# dev-mode-banner, conexus-vm-dev-preload) and nix/tests/single-tenant.nix's
# conexus-router-bootstrap-seed predated the `// hardening` convention
# nix/module.nix and nix/home-manager-module.nix already follow, and were
# never wired up to it. Confirmed live via `systemd-analyze security` on a
# real booted vm-dev VM: conexus-vm-dev-bootstrap-seed scored 9.6 UNSAFE
# (root, NoNewPrivileges=no, ProtectSystem=no, full stock
# CapabilityBoundingSet) vs. conexus-router's 2.8 OK in the SAME unit file.
#
# Rather than hardcode the individual directive values here (which would
# happily keep passing even if hardening.nix's own contents drifted away
# from what these units actually end up with), this reads
# nix/hardening.nix directly and asserts every key it defines is present
# with the SAME value on each target unit -- except the two keys
# nix/vm-dev.nix's own `preloadHardening` deliberately overrides on
# conexus-vm-dev-preload: CapabilityBoundingSet (needs CAP_CHOWN /
# CAP_DAC_OVERRIDE / CAP_FOWNER for `tar --same-owner` to restore the
# archived conexus uid into a directory it doesn't own) and ProcSubset
# (hardening.nix's "pid" hides /proc/cmdline, which the preload script
# reads to pick a fixture bundle -- confirmed live: with a plain
# `// hardening` merge the unit silently no-op'd instead of restoring).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

if ! command -v nix >/dev/null 2>&1; then
  echo "SKIP: nix is not available on PATH"
  exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "SKIP: jq is not available on PATH"
  exit 0
fi

DRIVER=$(cat <<EOF
let
  flake = builtins.getFlake "$REPO_ROOT";
  system = builtins.currentSystem;
  lib = flake.inputs.nixpkgs.lib;
  pkgs = flake.inputs.nixpkgs.legacyPackages.\${system};
  crane = flake.inputs.crane;
  hardening = import $REPO_ROOT/nix/hardening.nix;

  vmDevCfg = (lib.nixosSystem {
    inherit system;
    specialArgs = {
      src = flake;
      craneLib = crane.mkLib pkgs;
    };
    modules = [ $REPO_ROOT/nix/vm-dev.nix ];
  }).config;

  singleTenantTest = import $REPO_ROOT/nix/tests/single-tenant.nix {
    inherit pkgs lib;
    self = flake;
    craneLib = crane.mkLib pkgs;
  };
  singleTenantConfig = singleTenantTest.nodes.machine.config;

  units = {
    "vm-dev:conexus-vm-dev-bootstrap-seed" = {
      serviceConfig = vmDevCfg.systemd.services.conexus-vm-dev-bootstrap-seed.serviceConfig;
      overrides = [ ];
    };
    "vm-dev:dev-mode-banner" = {
      serviceConfig = vmDevCfg.systemd.services.dev-mode-banner.serviceConfig;
      overrides = [ ];
    };
    "vm-dev:conexus-vm-dev-preload" = {
      serviceConfig = vmDevCfg.systemd.services.conexus-vm-dev-preload.serviceConfig;
      overrides = [ "CapabilityBoundingSet" "ProcSubset" ];
    };
    "single-tenant:conexus-router-bootstrap-seed" = {
      serviceConfig = singleTenantConfig.systemd.services.conexus-router-bootstrap-seed.serviceConfig;
      overrides = [ ];
    };
  };

  hardeningKeys = builtins.attrNames hardening;

  checkUnit = name:
    let
      u = units.\${name};
      mismatched = builtins.filter
        (k: !(builtins.elem k u.overrides) && (u.serviceConfig.\${k} or null) != hardening.\${k})
        hardeningKeys;
    in { inherit name mismatched; };

in map checkUnit (builtins.attrNames units)
EOF
)

stderr_file="$(mktemp)"
trap 'rm -f "$stderr_file"' EXIT

json=$(NIX_CONFIG="experimental-features = nix-command flakes" \
  timeout 1800 nix eval --impure --json --expr "$DRIVER" 2>"$stderr_file") \
  || { echo "FAIL: nix eval failed:" >&2; cat "$stderr_file" >&2; exit 1; }

fail=0
count=$(jq 'length' <<<"$json")
for i in $(seq 0 $((count - 1))); do
  name=$(jq -r ".[$i].name" <<<"$json")
  mismatched=$(jq -c ".[$i].mismatched" <<<"$json")
  if [ "$mismatched" != "[]" ]; then
    echo "FAIL: $name does not fully merge nix/hardening.nix -- " \
      "mismatched/missing keys: $mismatched. Every conexus-owned systemd " \
      "unit is expected to merge the shared hardening baseline via " \
      "\`// hardening\` (or a documented per-unit override, see this " \
      "script's header comment); see nix/hardening.nix for what each " \
      "key defends against." >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "PASS: hardening-baseline-merged"
