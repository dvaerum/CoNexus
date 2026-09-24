#!/usr/bin/env bash
# Regression guard: the home-manager module's per-project backend
# template (`conexus@<name>.service`) must generate the
# `forwarding_hmac` key file in an ExecStartPre, the same way the NixOS
# module does (PRs #214, #216, #217).
#
# Background
# ----------
#
# PRs #214/#216/#217 ported the per-project HMAC key generation from the
# router (Python) into the systemd unit's ExecStartPre. The rationale,
# quoted from PR #214:
#
#     Old: router generates key, writes to disk, invokes systemctl start.
#          Self-heal on cache hit covers the router-driven restart, but
#          systemd's `Restart=on-failure` runs without the router.
#
#     New: systemd unit's ExecStartPre owns key generation. EVERY path
#          that starts the unit — manual `systemctl start`, on-failure
#          restart, boot-time activation — guarantees the file is on disk
#          before the backend's `--forwarding-hmac-in` validator runs.
#
# That fix landed in `nix/module.nix` (system-mode NixOS module). The
# home-manager template (`nix/home-manager-module.nix`) was never
# updated to match — the same drift pattern as PR #223 (the router-DB
# env-var port). Deployed home-manager systems hit:
#
#     agent-mcp-launcher: Error: Invalid value for '--forwarding-hmac-in':
#       File '/run/user/1000/agent-mcp/<name>/forwarding_hmac' does not exist.
#     systemd: agent-mcp@<name>.service: Main process exited,
#       code=exited, status=2/INVALIDARGUMENT
#     systemd: Scheduled restart job, restart counter is at 630.
#
# These checks pin the home-manager template's ExecStartPre to the same
# shape as the NixOS module, adapted for user-scope:
#
# - The ExecStartPre block must include a script that creates
#   `forwarding_hmac` from `/dev/urandom` if missing.
# - The script must use `pkgs.runtimeShell` (PR #216 fix: coreutils
#   does NOT ship `sh`, the unit fails 203/EXEC otherwise).
# - The script must write exactly 32 raw bytes (PR #217 fix: don't
#   `.strip()`, the bytes are binary and any whitespace at the
#   boundary is data).
# - The file mode must be 0600.
# - The unit must declare `RuntimeDirectory=conexus/%i` so the
#   parent dir exists with the right owner/mode.
# - The existing socket-removal ExecStartPre must still be present (the
#   pre-existing defensive cleanup).
#
# Retirement note: the per-project backend template was `agent-mcp@`
# (Python) when these fixes landed; it is `conexus@` (Rust) now that
# the Python implementation was retired, but the ExecStartPre shape these
# checks pin is unchanged -- the forwarding-HMAC contract is implementation-
# agnostic (both backends read the same file via `--forwarding-hmac-in`).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
HM_MODULE="$REPO_ROOT/nix/home-manager-module.nix"

TEXT="$(cat "$HM_MODULE")"

# _extract_backend_template_block(): return the raw nix source of the
# `"conexus@" = lib.mkIf ... { ... };` systemd user service block,
# tracked by brace depth (the block contains its own nested
# `{ ... }`, so a naive "next sibling key" string search is not a safe
# anchor).
extract_backend_template_block() {
  awk '
  BEGIN { RS = "\x01"; }
  {
    marker = "\"conexus@\" = lib.mkIf"
    start = index($0, marker)
    if (start == 0) { print "___MARKER_NOT_FOUND___"; exit 1 }
    brace_start = index(substr($0, start), "{") + start - 1
    depth = 1
    i = brace_start + 1
    n = length($0)
    while (depth > 0 && i <= n) {
      c = substr($0, i, 1)
      if (c == "{") depth++
      else if (c == "}") depth--
      i++
    }
    print substr($0, brace_start, i - brace_start)
  }' <<< "$1"
}

BLOCK="$(extract_backend_template_block "$TEXT")"
if [ "$BLOCK" = "___MARKER_NOT_FOUND___" ]; then
  echo "FAIL: could not find '\"conexus@\" = lib.mkIf' in nix/home-manager-module.nix" >&2
  exit 1
fi

fail=0

# test_backend_template_declares_runtime_directory:
# The per-project backend unit must declare
# `RuntimeDirectory=conexus/%i` so systemd creates
# `$XDG_RUNTIME_DIR/conexus/<name>/` with the right owner/mode
# before any ExecStartPre runs.
if ! grep -qP 'RuntimeDirectory\s*=\s*"conexus/%i"' <<<"$BLOCK"; then
  echo 'FAIL: home-manager-module.nix: "conexus@" service must set' \
    'RuntimeDirectory = "conexus/%i" so the parent dir exists' \
    'before ExecStartPre tries to write forwarding_hmac into it.' >&2
  fail=1
fi

# test_backend_template_references_shared_hmac_seed_logic:
# The per-project backend unit's ExecStartPre must reference the
# shared forwarding_hmac generator + stale-socket-cleanup helpers
# from nix/conexus-exec.nix, not a re-inlined or missing copy.
#
# Since that extraction (2026-09-24), the actual generator SHELL
# TEXT (head -c 32 /dev/urandom, chmod 600, runtimeShell, the
# idempotent test -f guard, the backend.sock rm -f) is no longer
# literal text in this file — it lives in nix/conexus-exec.nix and is
# proven correct there (see the real-evaluation tier below), which
# ALSO proves it for nix/module.nix's identical consumption. This
# structural check only pins that home-manager-module.nix still
# WIRES that shared logic in, rather than silently dropping it.
if ! grep -qF 'conexusExec.hmacSeedExecStartPre' <<<"$BLOCK"; then
  echo 'FAIL: home-manager-module.nix: "conexus@" service must wire in' \
    'conexusExec.hmacSeedExecStartPre (nix/conexus-exec.nix). The' \
    'NixOS module gained this key-generation logic in PR #214 (F015' \
    'v4); dropping it here reopens the exact crash-loop it fixed:' \
    "Error: Invalid value for '--forwarding-hmac-in': File '...'" \
    'does not exist.' >&2
  fail=1
fi

if ! grep -qF 'conexusExec.staleSocketExecStartPre' <<<"$BLOCK"; then
  echo 'FAIL: home-manager-module.nix: "conexus@" service must still' \
    'remove a stale backend.sock via' \
    'conexusExec.staleSocketExecStartPre in ExecStartPre.' >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi

# ── Real evaluation: the shared ExecStartPre logic's actual content ──
#
# Proves the properties PRs #214/#216/#217 fixed (32 raw urandom
# bytes, chmod 600, pkgs.runtimeShell not coreutils' missing /bin/sh,
# an idempotent test -f guard, the socket cleanup) against
# nix/conexus-exec.nix's real evaluated output directly — the actual
# source of truth for both modules now, per the same tier structure
# as nix/tests/checks/module-package-set.sh.
if ! command -v nix >/dev/null 2>&1; then
  echo "SKIP: nix is not available on PATH (real-evaluation tier skipped)"
  echo "PASS: home-manager-module-units (structural tier only)"
  exit 0
fi

CONEXUS_EXEC="$REPO_ROOT/nix/conexus-exec.nix"
eval_json=$(NIX_CONFIG="experimental-features = nix-command flakes" \
  timeout 300 nix eval --impure --json --expr \
  "let e = import $CONEXUS_EXEC { pkgs = (builtins.getFlake \"$REPO_ROOT\").inputs.nixpkgs.legacyPackages.\${builtins.currentSystem}; lib = (builtins.getFlake \"$REPO_ROOT\").inputs.nixpkgs.lib; }; in { hmac = e.hmacSeedExecStartPre; socket = e.staleSocketExecStartPre \"/run/conexus\"; }") \
  || { echo "FAIL: nix eval of nix/conexus-exec.nix failed" >&2; exit 1; }

hmac_line=$(jq -r '.hmac' <<<"$eval_json")
socket_line=$(jq -r '.socket' <<<"$eval_json")

if [[ "$hmac_line" != *"forwarding_hmac"* ]]; then
  echo "FAIL: nix/conexus-exec.nix: hmacSeedExecStartPre must reference " \
    "forwarding_hmac. Rendered value: $hmac_line" >&2
  exit 1
fi
if [[ "$hmac_line" != *"head -c 32 /dev/urandom"* ]]; then
  echo "FAIL: nix/conexus-exec.nix: hmacSeedExecStartPre must use " \
    "head -c 32 /dev/urandom (32 raw bytes, PR #217). Rendered value: " \
    "$hmac_line" >&2
  exit 1
fi
if [[ "$hmac_line" != *"chmod 600"* ]]; then
  echo "FAIL: nix/conexus-exec.nix: hmacSeedExecStartPre must chmod 600 " \
    "the key file. Rendered value: $hmac_line" >&2
  exit 1
fi
# `pkgs.runtimeShell` is already EVALUATED by this point (a real
# store path like `/nix/store/.../bash-5.3p15/bin/bash`), so the
# identifier "runtimeShell" itself never appears in rendered output —
# check the actual property PR #216 fixed instead: the interpreter is
# NOT coreutils' own (nonexistent) /bin/sh.
if [[ "$hmac_line" != "/nix/store/"*"-c "* ]] || [[ "$hmac_line" == *"coreutils"*"/bin/sh"* ]]; then
  echo "FAIL: nix/conexus-exec.nix: hmacSeedExecStartPre must run under a " \
    "real shell (pkgs.runtimeShell), not coreutils' missing /bin/sh " \
    "(PR #216 / F015 v6). Rendered value: $hmac_line" >&2
  exit 1
fi
if [[ "$hmac_line" != *'test -f'* ]]; then
  echo "FAIL: nix/conexus-exec.nix: hmacSeedExecStartPre must guard key " \
    "generation with an idempotent test -f check (commit 862e594) — " \
    "without it every restart rotates the key and invalidates the " \
    "router's in-memory cache. Rendered value: $hmac_line" >&2
  exit 1
fi
if [[ "$socket_line" != *"rm -f"* ]] || [[ "$socket_line" != *"backend.sock"* ]]; then
  echo "FAIL: nix/conexus-exec.nix: staleSocketExecStartPre must rm -f " \
    "a stale backend.sock. Rendered value: $socket_line" >&2
  exit 1
fi

echo "PASS: home-manager-module-units"
