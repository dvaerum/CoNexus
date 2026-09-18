# pkgs.nixosTest: end-to-end smoke that the server NEVER auto-terminates
# workers — the regression guard for the dashboard-cleanup-loop bug fixed
# in this PR.
#
# Runs as a systemd-nspawn container (`containers.machine`), not a QEMU
# VM (`nodes.machine`) — nixpkgs' native nspawn test-driver support
# (nixos/lib/testing/nodes.nix). Migrated using the same 2-change
# pattern as multi-tenant.nix (see
# docs/learnings/nspawn-test-migration.md): no systemd-hardening-
# directive assertions and no multi-node topology here either.
#
# Background
# ----------
# The dashboard previously ran a 2-minute setInterval that called
# `terminate_agent` on every "idle" worker (no current_task, > 10 min old)
# while any browser tab had the agents view open. This silently killed
# valid long-lived workers (`backend-dev`, `ios-app-dev`).
#
# The deeper invariant the fix needs to preserve: WITHOUT a browser
# connected, the backend itself MUST NOT auto-terminate any agent. The
# agent-deletion model is "explicit user action only." This VM test
# proves that property end-to-end:
#
#   1. Boot the multi-tenant router + a per-project backend.
#   2. Register a project.
#   3. Insert a "long-idle" worker directly in the sqlite DB
#      (created_at = "2024-01-01T00:00:00", status='created', no task).
#   4. Hit /api/all-data several times across a 3-minute window — the
#      sole client traffic is curl, no dashboard tab. (The deleted
#      cleanup loop was a browser-side setInterval; if it ever sneaks
#      back to the server, this test catches it.)
#   5. Re-read the worker row from sqlite. Status MUST still be
#      'created', terminated_at MUST still be NULL.
#
# This guarantees the server has no auto-terminate code path now and
# also pins the property as a CI-checked invariant against future
# "soft replacement" regressions (a background task, a periodic
# sweeper, anything that auto-flips status=terminated).
#
# Time budget
# -----------
# The 3-minute wait is intentional. The old dashboard loop fired every
# 120s; a 180s window covers more than one tick. Adding 30-60s for
# router cold-start + project creation puts the total runtime around
# 5-7 minutes — heavier than `multi-tenant.nix`'s ~3 minutes but well
# under the per-check budget for the existing VM tests.
{ pkgs, lib, self, craneLib, ... }:

let
  ports = import ./_ports.nix;
  packagedPkgs = import ../packages.nix {
    inherit pkgs lib;
    src = self;
  };
  # The Python router/backend this test used to boot
  # (`agentMcpRouterWrapper`/`agentMcpLauncher`) was retired together
  # with the rest of the Python source tree; `conexus-router`/
  # `conexus-backend` (via `nix/conexus.nix`) are the sole replacement.
  conexusPkgs = import ../conexus.nix {
    inherit pkgs lib craneLib;
    src = self;
  };
  # Bootstrap password sentinel for the router's env-var bootstrap (see
  # the CONEXUS_BOOTSTRAP_* comment on the router unit below). Class-
  # swept pentest fix: passing this via `environment=` bakes the
  # plaintext into the rendered, world-readable systemd unit file under
  # /nix/store/... (444 perms by design). Written to a 0600 runtime-
  # only file instead, referenced via `EnvironmentFile=` — mirrors
  # nix/module.nix's `forwarding_hmac` key-file pattern (search that
  # file for "F015 v4"), except this value is a fixed, publicly-known
  # CI sentinel rather than `/dev/urandom` output.
  bootstrapPasswordEnvFile = pkgs.writeText "conexus-router-bootstrap.env"
    "CONEXUS_BOOTSTRAP_PASSWORD=ci-sentinel-pw\n";
in
pkgs.testers.nixosTest {
  name = "conexus-no-auto-cleanup";

  containers.machine = { config, pkgs, ... }: {
    imports = [ ./fake-openai.nix ];

    users.users.testuser = {
      isNormalUser = true;
      group = "testuser";
      uid = 1500;
      createHome = true;
    };
    users.groups.testuser = {};

    systemd.services."conexus@" = {
      description = "CoNexus backend — project %i";
      after = [ "fake-openai.service" ];
      serviceConfig = {
        Type = "simple";
        User = "testuser";
        Group = "testuser";
        Environment = [
          "HOME=/home/testuser"
          "XDG_RUNTIME_DIR=/run/user/1500"
          # The launcher and router must agree on the backend socket
          # path. Without CONEXUS_SOCK_DIR the launcher falls back to
          # ``$XDG_RUNTIME_DIR/conexus`` (= /run/user/1500/...), which
          # this session-less system user can't create (mkdir EACCES) —
          # and even if it could, the backend would bind a socket the
          # router (CONEXUS_SOCK_DIR=/run/conexus) never looks at.
          # Pin both to the router's dir. Mirrors production
          # `nix/module.nix` (multi mode), which sets these on the
          # template too.
          "CONEXUS_SOCK_DIR=/run/conexus"
          "CONEXUS_PROJECTS_FILE=/home/testuser/.config/conexus/projects.local.json"
          "OPENAI_BASE_URL=http://127.0.0.1:11434/v1"
          "OPENAI_API_KEY=fake"
          "CONEXUS_EMBEDDING_MODEL=fake-zero-vector"
          "CONEXUS_EMBEDDING_DIMENSION=1024"
        ];
        RuntimeDirectory = "conexus/%i";
        RuntimeDirectoryMode = "0700";
        # See conexus-router's own RuntimeDirectoryPreserve comment
        # below -- same bare-parent-vs-%i-child sharing, same fix.
        RuntimeDirectoryPreserve = "yes";
        # F015 v4 (see nix/module.nix): the per-project forwarding-HMAC
        # key is generated by the unit's ExecStartPre, and the backend's
        # ``--forwarding-hmac-in`` refuses to start if the file is
        # absent. Generate it here (idempotently) before clearing the
        # stale socket, exactly as the production template does — the
        # test template previously only did the socket cleanup, so the
        # backend exited 2/INVALIDARGUMENT on every start.
        ExecStartPre = [
          "${pkgs.runtimeShell} -c 'test -f \"$RUNTIME_DIRECTORY/forwarding_hmac\" || { ${pkgs.coreutils}/bin/head -c 32 /dev/urandom > \"$RUNTIME_DIRECTORY/forwarding_hmac\" && ${pkgs.coreutils}/bin/chmod 600 \"$RUNTIME_DIRECTORY/forwarding_hmac\"; }'"
          "${pkgs.coreutils}/bin/rm -f /run/conexus/%i/backend.sock"
        ];
        ExecStart = ''
          ${conexusPkgs.conexusLauncher}/bin/conexus-launcher %i
        '';
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    # `conexus-router` (Rust) — the sole router implementation now
    # that the Python one (`agent-mcp-router`) was retired. See
    # multi-tenant.nix's own comment on its `conexus-router` unit for
    # the flag-vs-env-var split rationale.
    # EnvironmentFile= is loaded by systemd ONCE per unit activation,
    # before ANY of that unit's own exec steps run (including its own
    # ExecStartPre=) — so a same-unit ExecStartPre can never create the
    # file in time. The file must be written by an EARLIER, SEPARATE
    # unit that fully completes before conexus-router.service's own
    # activation begins.
    systemd.services.conexus-router-bootstrap-seed = {
      description = "Write CoNexus test sentinel operator password to a private runtime file";
      before = [ "conexus-router.service" ];
      requiredBy = [ "conexus-router.service" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        RuntimeDirectory = "conexus-bootstrap-seed";
        RuntimeDirectoryMode = "0700";
        ExecStart = "${pkgs.coreutils}/bin/install -m 0600 ${bootstrapPasswordEnvFile} /run/conexus-bootstrap-seed/bootstrap.env";
      };
    };

    systemd.services.conexus-router = {
      description = "CoNexus router (no-auto-cleanup test)";
      wantedBy = [ "multi-user.target" ];
      after = [ "fake-openai.service" "network.target" "conexus-router-bootstrap-seed.service" ];
      environment = {
        # Phase 1 PR B (prancy-napping-pie): see multi-tenant.nix.
        CONEXUS_ROUTER_DB = "/home/testuser/.config/conexus/router.db";
        # Phase 1 PR C: see single-tenant.nix.
        CONEXUS_BOOTSTRAP_USERNAME = "ci-sentinel";
        # CONEXUS_BOOTSTRAP_PASSWORD is intentionally NOT set here — see
        # the EnvironmentFile= / ExecStartPre wiring in serviceConfig
        # below.
        # The template unit is a SYSTEM service and the router runs as
        # an unprivileged user, so lazy-spawn must drive the system bus
        # (`systemctl start`, authorised by the polkit rule below) —
        # NOT `systemctl --user`, which would target testuser's own
        # systemd instance where no conexus@ unit exists. Mirrors
        # production `nix/module.nix`.
        CONEXUS_SYSTEMCTL_MODE = "system";
        CONEXUS_ROUTER_HOST = "0.0.0.0";
        # `--default-workspace` has no CLI-flag equivalent on
        # `conexus-router` (env-var-only); without it, projects created
        # via __create/api/router/projects would land under the wrong
        # fallback path (see home-manager-module.nix's own comment).
        CONEXUS_DEFAULT_WORKSPACE = "/home/testuser/projects";
      };
      serviceConfig = {
        Type = "simple";
        User = "testuser";
        Group = "testuser";
        # RuntimeDirectoryPreserve=yes (live incident 2026-09-07, see
        # nix/home-manager-module.nix's conexus-router unit for the
        # full writeup): this bare, single-component RuntimeDirectory
        # is a strict parent of conexus@'s own "conexus/%i" above --
        # per systemd.exec(5), that makes it THIS unit's own innermost
        # subdirectory, so without `=yes` every stop of this router
        # (crash-loop, redeploy) recursively deletes the whole
        # /run/conexus tree, including any live per-project backend's
        # own subdirectory and socket.
        RuntimeDirectory = "conexus";
        RuntimeDirectoryMode = "0700";
        RuntimeDirectoryPreserve = "yes";
        ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /home/testuser/.config/conexus /home/testuser/projects";
        # Written by the conexus-router-bootstrap-seed unit above,
        # BEFORE this unit's own activation begins.
        EnvironmentFile = "/run/conexus-bootstrap-seed/bootstrap.env";
        ExecStart =
          "${conexusPkgs.conexusRouterWrapper}/bin/conexus-router "
          + "--port ${toString ports.routerPort} "
          + "--projects-file /home/testuser/.config/conexus/projects.local.json "
          + "--sock-dir /run/conexus "
          + "--dashboard-dir ${packagedPkgs.conexusDashboard}/share/conexus-dashboard "
          + "--external-url ${lib.escapeShellArg "http://localhost:${toString ports.routerPort}"} "
          + "--idle-sec 14400";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.systemd1.manage-units" &&
            subject.user == "testuser") {
          var unit = action.lookup("unit");
          if (unit && (unit.indexOf("conexus@") == 0)) {
            return polkit.Result.YES;
          }
        }
      });
    '';

    environment.systemPackages = [ pkgs.curl pkgs.jq pkgs.sqlite ];
    networking.firewall.enable = false;
  };

  testScript = ''
    start_all()
    machine.wait_for_unit("fake-openai.service")
    machine.wait_for_unit("conexus-router.service")
    machine.wait_for_open_port(${toString ports.routerPort})

    # Log in the sentinel operator (ADR 0014: admin REST surface is
    # session-gated).
    machine.succeed(
        "curl -fsS -c /tmp/conexus-cookies.txt "
        "--data 'username=ci-sentinel&password=ci-sentinel-pw' "
        "http://127.0.0.1:${toString ports.routerPort}/conexus/login"
    )

    # 1. Register a project so a backend gets lazy-spawned (ADR 0014:
    # POST /api/router/projects with a JSON body).
    machine.succeed(
        "curl -fsSL -b /tmp/conexus-cookies.txt -o /dev/null "
        "-H 'Accept: application/vnd.conexus.v1+json' "
        "-H 'Content-Type: application/json' "
        "-X POST --data '{\"name\": \"idle-test\"}' "
        "http://127.0.0.1:${toString ports.routerPort}/conexus/api/router/projects"
    )

    # 2. Force backend startup by hitting the dashboard endpoint —
    # this routes to /conexus/app/idle-test/ which spawns the
    # per-project backend on first contact.
    #
    # SC-R6-1 (round 6): the /app/ warm-start side-effect is now
    # authorization-gated. The auth middleware serves the SPA shell to
    # authenticated NON-MEMBERS too (round-4 project-existence-oracle
    # fix), but only sets the internal ``_warm_authorized`` flag — and
    # thus fires the lazy-spawn — for an AUTHORIZED caller (sysadmin,
    # sufficient membership, or single-tenant). The ``-b`` cookie below
    # is therefore load-bearing: ci-sentinel is the first operator
    # (bootstrap sysadmin) AND the creator/member of idle-test, so the
    # spawn fires. Dropping the cookie here would still 200 (uniform
    # shell, no oracle) but would NO LONGER start the backend, breaking
    # this test's premise — do not remove it.
    machine.succeed(
        "curl -fsS -b /tmp/conexus-cookies.txt -o /dev/null "
        "http://127.0.0.1:${toString ports.routerPort}/conexus/app/idle-test/"
    )

    # Wait for the per-project backend to come up AND finish schema
    # init before touching its DB directly. Poll the UDS socket, not
    # the sqlite file's mere existence: `rusqlite::Connection::open()`
    # creates the file lazily at connection-open time, strictly BEFORE
    # any `CREATE TABLE` statement runs, so `test -f mcp_state.db` can
    # observe a real, on-disk, but SCHEMA-LESS file (confirmed live --
    # this exact race produced a genuine "no such table: agents"
    # failure in event-driven-coord.nix's sibling check). The socket
    # binds as `conexus-backend::main()`'s literal last step, strictly
    # AFTER schema init completes -- the same readiness idiom
    # `orchestrator::ensure::socket_ready()` already uses router-side.
    machine.wait_for_unit("conexus@idle-test.service")
    machine.wait_until_succeeds(
        "test -S /run/conexus/idle-test/backend.sock",
        timeout=60,
    )

    db = "/home/testuser/projects/idle-test/.agent/mcp_state.db"

    # 3. Insert a "long-idle" worker directly. created_at is two
    # years in the past so the deleted dashboard heuristic
    # (>10 min idle) would have flagged it on every poll. status
    # is the same shape used elsewhere in tests.
    machine.succeed(
        # -cmd '.timeout 5000': the backend holds the WAL write-lock in
        # short bursts; without a busy-timeout this direct INSERT races it
        # and fails intermittently with "database is locked (5)" (a
        # recurring CI flake). 5s busy-timeout makes sqlite wait for the
        # lock instead of erroring instantly.
        f"sqlite3 -cmd '.timeout 5000' {db} \"INSERT INTO agents "
        "(token, agent_id, created_at, status, "
        "working_directory, color, updated_at) VALUES "
        "('__test_token_old', 'old-worker', "
        "'2024-01-01T00:00:00', 'created', '/tmp', '#888', "
        "'2024-01-01T00:00:00');\""
    )

    # Sanity: the row exists and is not terminated.
    initial = machine.succeed(
        f"sqlite3 -cmd '.timeout 5000' {db} \"SELECT status FROM agents WHERE "
        "agent_id='old-worker';\""
    ).strip()
    assert initial == "created", (
        f"setup: expected status='created' after insert; got {initial!r}"
    )

    # 4. Drive client traffic for ~3 minutes. The deleted bug was a
    # browser-side setInterval; this loop is a stand-in for "the
    # dashboard would be open." If anything server-side ever
    # auto-terminates idle agents, hitting /api/all-data repeatedly
    # is the most likely trigger surface.
    #
    # 3-min sleep > the old 2-min auto-cleanup tick + slack.
    machine.succeed("sleep 30")
    for _ in range(9):
        machine.succeed(
            "curl -fsS -o /dev/null "
            "http://127.0.0.1:${toString ports.routerPort}/conexus/api/idle-test/all-data"
            " || curl -fsS -o /dev/null "
            "http://127.0.0.1:${toString ports.routerPort}/conexus/__api/idle-test/all-data"
            " || true"
        )
        machine.succeed("sleep 20")

    # 5. Re-read the row. status MUST still be 'created' and
    # terminated_at MUST still be NULL — proves the server has no
    # auto-terminate code path.
    final_status = machine.succeed(
        f"sqlite3 -cmd '.timeout 5000' {db} \"SELECT status FROM agents WHERE "
        "agent_id='old-worker';\""
    ).strip()
    assert final_status == "created", (
        f"REGRESSION: old-worker was auto-terminated by the server "
        f"(status={final_status!r}). The agent-deletion model must "
        f"remain 'explicit user action only' — no background task, "
        f"no periodic sweeper, no API-side cleanup may exist."
    )

    final_terminated_at = machine.succeed(
        f"sqlite3 -cmd '.timeout 5000' {db} \"SELECT IFNULL(terminated_at, '<null>') "
        "FROM agents WHERE agent_id='old-worker';\""
    ).strip()
    assert final_terminated_at == "<null>", (
        f"REGRESSION: old-worker has a terminated_at timestamp "
        f"({final_terminated_at!r}) despite never being terminated "
        f"by user action. Server-side auto-cleanup snuck back in."
    )
  '';
}
