#!/usr/bin/env bash
# Regression guard: `nix run .#vm-dev`'s router unit must actually be able
# to boot -- both a real ExecStart AND a bootstrap password that passes the
# router's own strength policy.
#
# Background (live incident, 2026-09-18)
# ---------------------------------------
#
# Two independent bugs, found back to back while pentesting against a
# freshly-booted `nix run .#vm-dev --ephemeral`, together left this VM's
# `conexus-router.service` completely non-functional since Python's
# retirement (Phase F):
#
# 1. `flake.nix`'s `vmDev` nixosSystem passed `specialArgs = { src = self; }`
#    -- missing `craneLib`, unlike its sibling `vmMulti`. Without `craneLib`,
#    `nix/vm.nix`'s `conexusPkgsForVm` (and therefore `conexusRouterPackage`/
#    `conexusLauncherPackage`) resolves to `null`, so `nix/module.nix`'s real
#    `conexus-router` unit definition (gated on `conexusRouterPackage != null`)
#    never applies. Only `nix/vm-dev.nix`'s separate `.environment` merge
#    survived, producing a unit with ONLY `Environment=` lines and no
#    `ExecStart=` at all -- systemd refused to even load it
#    ("bad-setting: Service has no ExecStart=...").  Since the Python
#    router/backend pair was fully retired, there was no fallback left: this
#    silently made `nix run .#vm-dev` boot a VM with NO router running at
#    all, for every developer, since that retirement.
#
# 2. Once (1) was fixed and a real unit existed, `conexus-router.service`
#    crash-looped on every start: `nix/vm-dev.nix`'s own first-boot operator
#    seed hardcoded `CONEXUS_BOOTSTRAP_PASSWORD = "dev"` (3 chars), but
#    `rust/conexus-router/src/identity.rs::PASSWORD_MIN_LENGTH` requires 12.
#    The seed exists specifically so a developer can `/login` immediately --
#    a password that fails validation at startup defeats that purpose
#    entirely and instead crash-loops the router.
#
# Both are pinned here via a real evaluation of the SAME nixosSystem
# construction `flake.nix` performs for `packages.<system>.vm-dev`, rather
# than re-deriving an independent expectation.
#
# Update (2026-09-18, pentest class-sweep): CONEXUS_BOOTSTRAP_PASSWORD moved
# off `environment=` onto a 0600 runtime-only file referenced via
# `EnvironmentFile=` (see nix/tests/checks/bootstrap-password-not-inlined.sh
# for that fix's own regression guard). The password value is no longer
# readable at `router.environment.CONEXUS_BOOTSTRAP_PASSWORD`, so the
# strength check below reads it from `system.build.
# conexusVmDevBootstrapEnvFile` instead -- the same writeText derivation
# nix/vm-dev.nix's ExecStartPre installs into the runtime file.
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
  cfg = (lib.nixosSystem {
    inherit system;
    specialArgs = {
      src = flake;
      craneLib = crane.mkLib pkgs;
    };
    modules = [ $REPO_ROOT/nix/vm-dev.nix ];
  }).config;
  router = cfg.systemd.services.conexus-router;
  bootstrapEnvFileText = builtins.readFile cfg.system.build.conexusVmDevBootstrapEnvFile;
  bootstrapPassword = lib.removeSuffix "\n"
    (lib.removePrefix "CONEXUS_BOOTSTRAP_PASSWORD=" bootstrapEnvFileText);
in {
  execStart = router.serviceConfig.ExecStart or "";
  bootstrapPasswordLength = builtins.stringLength bootstrapPassword;
}
EOF
)

stderr_file="$(mktemp)"
trap 'rm -f "$stderr_file"' EXIT

json=$(NIX_CONFIG="experimental-features = nix-command flakes" \
  timeout 1800 nix eval --impure --json --expr "$DRIVER" 2>"$stderr_file") \
  || { echo "FAIL: nix eval failed:" >&2; cat "$stderr_file" >&2; exit 1; }

exec_start=$(jq -r '.execStart' <<<"$json")
pw_len=$(jq -r '.bootstrapPasswordLength' <<<"$json")

fail=0

if [ -z "$exec_start" ]; then
  echo "FAIL: vm-dev's conexus-router.service has no ExecStart -- " \
    "conexusRouterPackage resolved to null, meaning craneLib never " \
    "reached nix/vm.nix (check flake.nix's vmDev specialArgs and " \
    "nix/vm-dev.nix's own craneLib passthrough); see this script's " \
    "header comment for the live incident this pins." >&2
  fail=1
fi

if [ "$pw_len" -lt 12 ]; then
  echo "FAIL: vm-dev's seeded CONEXUS_BOOTSTRAP_PASSWORD is $pw_len chars, " \
    "below identity::PASSWORD_MIN_LENGTH (12) -- the router will crash-loop " \
    "on every boot instead of seeding a usable dev operator; see this " \
    "script's header comment for the live incident this pins." >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "PASS: vm-dev-router-boots"
