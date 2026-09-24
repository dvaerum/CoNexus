# pkgs.nixosTest: proves nix/module.nix's parity-work options
# (multiTenant/singleProject, sso.proxyHeader, daemonAgents) actually
# work at runtime — not just that they evaluate cleanly.
#
# Unlike the other VM tests in this directory (single-tenant.nix,
# multi-tenant.nix, etc.), which hand-roll their own inline systemd
# unit config mirroring nix/module.nix rather than importing it (a
# real, separate, deliberately out-of-scope drift this test doesn't
# fix — see AGENTS.md/PR2's own plan notes), THIS test `imports =
# [ ../module.nix ];` directly. It's the one place in this repo's own
# test suite proving the real, shipped NixOS module — not a copy of
# it — boots and behaves correctly with its newest option surface.
#
# TWO nodes, not one: `single_tenant.rs`'s own module doc confirms
# `multiTenant = false` bypasses the ENTIRE operator/session gate for
# every caller (ADR-0008 — "no second tenant to gate against"), so SSO
# proxy-header TRUST is architecturally unobservable under single-
# tenant mode (the gate that would evaluate it never runs at all — an
# unauthenticated caller is admitted exactly as readily as a trusted
# one). Combining singleProject + sso.proxyHeader in one router config
# would either silently pass regardless of whether SSO wiring works,
# or (worse, and what actually happened while building this test)
# crash every session-gated REST read with a 500, since the gate's
# `PassThrough` outcome never populates the `GateIdentity` extension
# several handlers unconditionally required — a real bug, fixed
# alongside this test (see `lifecycle_rest.rs`'s `list_projects_handler`/
# `overview_handler` doc comments). Splitting into two real node
# configs tests each option combination in the mode it's actually
# meaningful in, rather than forcing an artificial single scenario.
#
# Four real-behavior assertions across the two nodes:
#
#   1. [single_tenant] multiTenant = false + singleProject: the
#      module's own singleProjectSeedScript actually seeds
#      projects.local.json with the correct content, and the router
#      410s a project-lifecycle write (ADR-0008) — proving the
#      2026-09-24 lib.mkIf fix (see nix/module.nix's own comment on
#      singleProjectSeedScript) works end-to-end through the real
#      module, not just via a direct eval.
#   2. [single_tenant] daemonAgents: a real
#      `conexus-daemon-agent@<instance>.service` unit, seeded with a
#      real agent + bearer token via direct SQL (the same pattern
#      nix/tests/event-driven-coord.nix already uses), starts and
#      STAYS active — proving the unit template this PR added
#      actually reaches the router, not just that it evaluates.
#   3. [multi_tenant] SSO proxy-header trust: a request carrying the
#      configured trust header from a trusted IP is admitted without
#      a session cookie or bearer token.
#   4. [multi_tenant] the identical request with NO header still
#      requires real auth (401) — proving trust isn't accidentally
#      wide open.
{ pkgs, lib, self, craneLib, ... }:

let
  ports = import ./_ports.nix;
  conexusPkgs = import ../conexus.nix {
    inherit pkgs lib craneLib;
    src = self;
  };
  singleName = "solo-project";
  singleWorkspace = "/var/lib/conexus/projects/${singleName}";
  daemonAgentId = "test-worker";
  # conexus-daemon-agent resolves its own token path from
  # $XDG_CONFIG_HOME (falling back to $HOME/.config, and systemd sets
  # $HOME for a User=-scoped system service from the user database
  # entry — module.nix sets `home = cfg.stateDir` on cfg.user, so
  # this resolves to `${cfg.stateDir}/.config/...` with no extra
  # Environment= needed). daemonAgents[].tokenPath is documentation-
  # only (confirmed by reading rust/conexus-daemon-agent/src/main.rs
  # directly — it's never actually read by any Nix-generated config);
  # what matters for this test is placing the real token file where
  # the binary will actually look.
  daemonAgentTokenPath =
    "/var/lib/conexus/.config/conexus/tokens/${singleName}--${daemonAgentId}.token";

  commonModuleConfig = {
    enable = true;
    src = self;
    conexusRouterPackage = conexusPkgs.conexusRouterWrapper;
    conexusLauncherPackage = conexusPkgs.conexusLauncher;
    conexusDaemonAgentPackage = conexusPkgs.conexusDaemonAgentWrapper;
  };
in
pkgs.testers.nixosTest {
  name = "conexus-module-parity";

  nodes.single_tenant = { config, pkgs, ... }: {
    imports = [ ./fake-openai.nix ../module.nix ];

    virtualisation = {
      memorySize = 1536;
      cores = 2;
      diskSize = 2048;
    };

    services.conexus = commonModuleConfig // {
      router = {
        port = ports.routerPort;
        # Every curl in this test runs from `single_tenant` itself
        # against 127.0.0.1 — loopback bind is both sufficient and
        # required: the real router refuses to boot single-tenant (no
        # operator auth) bound to a non-loopback host (ADR-0008 safety
        # guard).
        host = "127.0.0.1";
        externalUrl = "http://localhost:5454";
      };
      multiTenant = false;
      singleProject = {
        name = singleName;
        workspace = singleWorkspace;
      };
      daemonAgents = [
        {
          project = singleName;
          agentId = daemonAgentId;
          tokenPath = daemonAgentTokenPath;
        }
      ];
    };

    # singleProject.workspace is deliberately NOT auto-created by the
    # module (matches home-manager-module.nix's identical contract —
    # the operator provisions it). Owned by conexus's own system user
    # so conexus@solo-project.service (running as that user) can use
    # it as --project-dir.
    systemd.tmpfiles.rules = [
      "d ${singleWorkspace} 0750 conexus conexus - -"
    ];

    environment.systemPackages = [ pkgs.curl pkgs.sqlite ];
  };

  nodes.multi_tenant = { config, pkgs, ... }: {
    imports = [ ../module.nix ];

    virtualisation = {
      memorySize = 1536;
      cores = 2;
      diskSize = 2048;
    };

    services.conexus = commonModuleConfig // {
      router = {
        port = ports.routerPort;
        host = "127.0.0.1";
        externalUrl = "http://localhost:5454";
      };
      # multiTenant defaults to true (its own module default) — the
      # operator/session gate must actually run for SSO proxy-header
      # trust to mean anything.
      sso.proxyHeader = {
        trustHeader = "X-Test-User";
        trustedIps = [ "127.0.0.1" "::1" ];
        defaultIsSysadmin = true;
      };
    };
  };

  testScript = ''
    start_all()

    # ── single_tenant: assertions 1-2 ──────────────────────────────
    single_tenant.wait_for_unit("conexus-router.service")
    single_tenant.wait_for_open_port(${toString ports.routerPort})

    # Assertion 1: single-tenant seed + ADR-0008 lockout. The seed
    # script's ExecStartPre runs before the router binds its port, so
    # by the time wait_for_open_port succeeds the file must already
    # be correct.
    seeded = single_tenant.succeed("cat /var/lib/conexus/projects.local.json")
    assert '"${singleName}":"${singleWorkspace}"' in seeded, (
        "projects.local.json was not seeded correctly by the real "
        f"module's singleProjectSeedScript: {seeded!r}"
    )

    # No -f here (deliberately): curl -f treats a 410 as a hard
    # failure and discards the status before -w can report it — this
    # assertion needs to SEE the 410, not have curl swallow it.
    lockout_status = single_tenant.succeed(
        "curl -sS -o /dev/null -w '%{http_code}' -X POST "
        "http://127.0.0.1:${toString ports.routerPort}"
        "/conexus/api/router/projects "
        "-H 'Content-Type: application/json' -d '{}'"
    ).strip()
    assert lockout_status == "410", (
        "POST /api/router/projects in single-tenant mode did not "
        f"410 as ADR-0008 requires (got {lockout_status!r}) — the "
        "real module's multiTenant=false wiring is not locking out "
        "project-lifecycle writes"
    )

    # Assertion 2: daemonAgents unit reaches the router. Start the
    # per-project backend explicitly rather than trying to trigger the
    # router's lazy spawn via /conexus/app/<name>/ (dashboard SHELL
    # serving doesn't proxy into the backend at all — confirmed the
    # hard way: that curl returned 200 in 0.13s with no backend unit
    # ever starting). event-driven-coord.nix's own comment explains
    # why: an explicit start "gives us a deterministic readiness
    # probe (wait_for_unit)" rather than racing an HTTP-triggered
    # lazy spawn.
    single_tenant.succeed("systemctl start conexus@${singleName}.service")
    single_tenant.wait_for_unit("conexus@${singleName}.service")
    sock_path = "/run/conexus/${singleName}/backend.sock"
    single_tenant.wait_until_succeeds(f"test -S {sock_path}", timeout=60)

    db_path = "/var/lib/conexus/projects/${singleName}/.agent/mcp_state.db"
    single_tenant.succeed("systemctl stop conexus@${singleName}.service")
    single_tenant.succeed(
        f"sqlite3 {db_path} "
        "\"INSERT INTO agents (token, agent_id, created_at, status, "
        "working_directory, color, updated_at, auto_event_loop) VALUES "
        "('dt-real-token', '${daemonAgentId}', '2026-01-01T00:00:00', "
        "'active', '${singleWorkspace}', '#888', "
        "'2026-01-01T00:00:00', 1)\""
    )
    single_tenant.succeed("systemctl start conexus@${singleName}.service")
    single_tenant.wait_until_succeeds(f"test -S {sock_path}", timeout=60)

    single_tenant.succeed(
        "install -D -m 0600 -o conexus -g conexus /dev/stdin "
        "${daemonAgentTokenPath} <<< 'dt-real-token'"
    )

    # WantedBy = multi-user.target means this unit already auto-started
    # at boot, before the token file existed — it has almost certainly
    # already failed at least once (Restart=on-failure/RestartSec=10
    # would keep retrying it regardless, but an explicit restart here
    # makes the "does it succeed with a REAL token" assertion below
    # deterministic rather than racing the retry backoff).
    single_tenant.succeed(
        "systemctl restart "
        "conexus-daemon-agent@${singleName}--${daemonAgentId}.service"
    )
    single_tenant.wait_for_unit(
        "conexus-daemon-agent@${singleName}--${daemonAgentId}.service"
    )
    # The unit reaching the router (rather than crash-looping on a
    # rejected/missing token) is the real proof this PR's template is
    # correct: give it a real window and confirm it's STILL the
    # ORIGINAL invocation — a restart bumps systemd's own internal
    # NRestarts counter, which stays 0 only if the process never
    # exited.
    single_tenant.sleep(15)
    single_tenant.succeed(
        "test \"$(systemctl show -p NRestarts --value "
        "conexus-daemon-agent@${singleName}--${daemonAgentId}.service)\" "
        "= 0"
    )
    active = single_tenant.succeed(
        "systemctl show -p ActiveState --value "
        "conexus-daemon-agent@${singleName}--${daemonAgentId}.service"
    ).strip()
    assert active == "active", (
        "conexus-daemon-agent@${singleName}--${daemonAgentId}.service "
        "is not active after a real token + real router (state: "
        f"{active!r}) — the daemonAgents systemd unit template this PR "
        "added is not correctly reaching the router"
    )

    # ── multi_tenant: assertions 3-4 ───────────────────────────────
    multi_tenant.wait_for_unit("conexus-router.service")
    multi_tenant.wait_for_open_port(${toString ports.routerPort})

    # Assertion 3: SSO proxy-header trust. No cookie, no bearer — only
    # the trusted header from a trusted source (this curl runs ON
    # `multi_tenant` itself, i.e. from 127.0.0.1). An admitted request
    # reaches the real backing route rather than being rejected.
    status = multi_tenant.succeed(
        "curl -sS -o /dev/null -w '%{http_code}' "
        "-H 'X-Test-User: proxy-header-test-user' "
        "http://127.0.0.1:${toString ports.routerPort}"
        "/conexus/api/router/overview"
    ).strip()
    assert status == "200", (
        "proxy-header-trusted request to /api/router/overview did not "
        f"get admitted (expected 200, got {status}) — sso.proxyHeader "
        "wiring through the real module is not working"
    )

    # Assertion 4: the identical request from an UNTRUSTED-looking
    # caller (no header at all) must still require real auth — proving
    # multi-tenant mode's operator gate is genuinely enforced, not
    # accidentally bypassed the way single-tenant deliberately is.
    no_header_status = multi_tenant.succeed(
        "curl -sS -o /dev/null -w '%{http_code}' "
        "http://127.0.0.1:${toString ports.routerPort}"
        "/conexus/api/router/overview"
    ).strip()
    assert no_header_status == "401", (
        "a request with NO auth and no trusted header reached "
        f"/api/router/overview (got {no_header_status}, expected 401) "
        "— the proxy-header trust must not admit unauthenticated "
        "callers by accident"
    )
  '';
}
