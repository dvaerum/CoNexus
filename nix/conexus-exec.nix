# Shared systemd ExecStart/ExecStartPre construction for the conexus
# binaries (defense against silent drift, not just less typing).
#
# Single source of truth for the exec-line logic duplicated verbatim
# (or near-verbatim, differing only in which path form each framework
# supplies) between BOTH modules:
#
#   - nix/home-manager-module.nix  (user-scope units, %h/%t specifiers)
#   - nix/module.nix               (system-scope units, cfg.stateDir/
#                                    cfg.runtimeDir absolute paths)
#
# Every function here is pure: no `cfg`/`config` reference, no
# `systemd.*` attrset construction — only `pkgs`/`lib` plus the exact
# strings/packages the caller already resolved from its own options.
# This mirrors nix/hardening.nix's own role (shared plain data/logic,
# imported by both modules) rather than inventing a new "shared
# options module" abstraction — see docs/adr/0029-* for why options
# themselves stay split per-module (no ecosystem precedent unifies
# those; see also AGENTS.md's parity requirement).
{ lib, pkgs }:

{
  # The `forwarding_hmac` key-generation ExecStartPre line for a
  # `conexus@<project>` backend instance. Byte-identical between both
  # modules today — generates the key in the unit's own
  # ExecStartPre (not the router) so EVERY path that starts the unit
  # guarantees the file exists, regardless of who triggered the start
  # (manual, on-failure restart, the router's lazy-spawn). Needs no
  # caller-supplied path: `$RUNTIME_DIRECTORY` is systemd's own env
  # var, already correct under both `systemd.services` (system units)
  # and `systemd.user.services` (user units).
  #
  # `runtimeShell`, not `${pkgs.coreutils}/bin/sh`: coreutils does NOT
  # ship `sh` (F015 v6 — the original interpolation failed with
  # `status=203/EXEC` on every backend start). `head`/`chmod` ARE in
  # coreutils.
  hmacSeedExecStartPre =
    "${pkgs.runtimeShell} -c 'test -f \"$RUNTIME_DIRECTORY/forwarding_hmac\" "
    + "|| { ${pkgs.coreutils}/bin/head -c 32 /dev/urandom > \"$RUNTIME_DIRECTORY/forwarding_hmac\" "
    + "&& ${pkgs.coreutils}/bin/chmod 600 \"$RUNTIME_DIRECTORY/forwarding_hmac\"; }'";

  # Removes a stale `backend.sock` left behind from a prior run, given
  # the per-project socket directory's PARENT as an absolute path or a
  # systemd specifier string (`cfg.runtimeDir` on the NixOS side,
  # `%t/conexus` on the home-manager side) — the two frameworks
  # resolve "the runtime dir" through structurally different
  # mechanisms, so this stays a caller-supplied argument rather than a
  # single shared constant.
  staleSocketExecStartPre =
    socketDirParent: "${pkgs.coreutils}/bin/rm -f ${socketDirParent}/%i/backend.sock";

  # The `conexus-router` ExecStart flag string, common to both
  # deployment shapes. `singleProject` is `null` for multi-tenant, or
  # `{ name, workspace }` for single-tenant (ADR-0008) — ported
  # verbatim from home-manager-module.nix's own `commonFlags`
  # let-binding, the only one of the two modules that already needed
  # the single-tenant branch before this refactor.
  routerExecStart =
    {
      routerPackage,
      port,
      projectsFile,
      sockDir,
      dashboardPackage,
      externalUrl,
      idleSec,
      singleProject ? null,
    }:
    let
      commonFlags =
        "--port ${toString port} "
        + "--projects-file ${projectsFile} "
        + "--sock-dir ${sockDir} "
        + "--dashboard-dir ${dashboardPackage}/share/conexus-dashboard "
        + "--external-url ${lib.escapeShellArg externalUrl} "
        + "--idle-sec ${toString idleSec}";
    in
    if singleProject == null then
      "${routerPackage}/bin/conexus-router " + commonFlags
    else
      "${routerPackage}/bin/conexus-router "
      + commonFlags
      + " --single-tenant ${lib.escapeShellArg singleProject.name} "
      + "--single-workspace ${lib.escapeShellArg singleProject.workspace}";
}
