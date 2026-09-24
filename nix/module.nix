{ config, lib, pkgs, ... }:

# NixOS module exposing the conexus deployment as system services.
#
#   services.conexus.enable = true
#     → router (conexus-router) on :1337 + per-project conexus@<name>.service
#       template (mirrors the production nixos-developer-system deployment).
#
# The flake passes `src` (the repo root) through `_module.args.src`
# so we can use the same code path from both the VM and any other
# NixOS host that wants to import this module.
#
# History: this module used to also support `mode = "single"` (one
# always-on Python backend on a bare TCP port, no router) and
# `mode = "multi"` selecting between that and the router shape below.
# Both modes ran the Python implementation (`agent-mcp@<name>.service`
# / `agent-mcp-router.service`), retired together with the Python
# source tree. The router shape survives as the module's only shape,
# rebuilt on the CoNexus Rust binaries (`conexus@<name>.service` /
# `conexus-router.service`) — `conexusLauncherPackage`/
# `conexusRouterPackage` below. The bare-TCP-port single-backend shape
# has NO Rust equivalent: `conexus-backend` only serves over `--uds`
# (see rust/conexus-backend/src/main.rs's own module doc — host:port
# serving was explicitly never ported, since no `conexus@<name>.service`
# caller needs it). Reviving it would mean adding TCP-serving support
# to conexus-backend, which is out of scope for this retirement; until
# that happens, single-backend deploys have no supported path through
# this module.

let
  cfg = config.services.conexus;
  pkgs' = import ./packages.nix {
    inherit pkgs lib;
    src = cfg.src;
  };

  # ── Systemd hardening (defense-in-depth) ──────────────────────────
  # The SAFE sandboxing subset is shared verbatim with the home-manager
  # (user-scope) module via nix/hardening.nix — one source of truth for
  # both deployment shapes. See that file for the per-directive
  # rationale and the deliberately-omitted set.
  #
  # system-mode runs as a dedicated system user (not under $HOME), so it
  # goes stricter than the shared subset: ProtectSystem="strict" +
  # ProtectHome + explicit ReadWritePaths lock the filesystem down to
  # exactly the state + runtime dirs the service writes (all units keep
  # their persistent state under cfg.stateDir and their sockets / HMAC
  # keys under cfg.runtimeDir). Merged into every serviceConfig via
  # `// systemHardening`.
  hardening = import ./hardening.nix;
  systemHardening = hardening // {
    ProtectSystem = "strict";
    ProtectHome = true;
    ReadWritePaths = [ cfg.stateDir cfg.runtimeDir ];
  };

  # Shared ExecStart/ExecStartPre construction — see nix/conexus-exec.nix's
  # own module doc for why this is a plain function file (mirroring
  # hardening.nix's role) rather than a shared options module.
  conexusExec = import ./conexus-exec.nix { inherit lib pkgs; };

  daemonAgentInstanceName = a: "${a.project}--${a.agentId}";

  # ── Single-tenant ExecStartPre seed ───────────────────────────────
  # System-mode counterpart of nix/home-manager-module.nix's own
  # `singleProjectSeedScript` — same idempotent-seed contract (leave
  # an already-correct file alone, overwrite otherwise), simpler here
  # since `cfg.stateDir` is already an absolute path at eval time (no
  # `$XDG_CONFIG_HOME`/`$HOME` runtime resolution needed).
  #
  # NOT wrapped in `lib.mkIf` here — see home-manager-module.nix's own
  # doc comment on its identical helper for why that's a real,
  # previously-live bug (`mkIf` is only meaningful inside the module
  # system's own config-merge; on a plain `let`-bound value it
  # crashes on string interpolation unconditionally). The real gating
  # already happens correctly at the `ExecStartPre` site below.
  singleProjectSeedScript =
    pkgs.writeShellScript "conexus-single-tenant-seed" ''
      set -euo pipefail
      file="${cfg.stateDir}/projects.local.json"
      desired='{"${cfg.singleProject.name}":"${cfg.singleProject.workspace}"}'
      if [[ -f "$file" ]] \
        && ${pkgs.jq}/bin/jq -e \
            --arg n "${cfg.singleProject.name}" \
            --arg w "${cfg.singleProject.workspace}" \
            '.[$n] == $w' "$file" >/dev/null 2>&1; then
        exit 0
      fi
      echo "$desired" > "$file.new"
      mv "$file.new" "$file"
    '';
in {
  options.services.conexus = {
    enable = lib.mkEnableOption "conexus (multi-tenant router + per-project backends)";

    src = lib.mkOption {
      type = lib.types.path;
      description = ''
        Path to the conexus source tree. The flake wires this to
        the repo root via `_module.args`.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "conexus";
      description = "System user that owns the deployment.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "conexus";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/conexus";
      description = "Persistent state root (projects.local.json, workspaces).";
    };

    runtimeDir = lib.mkOption {
      type = lib.types.str;
      default = "/run/conexus";
      description = "Volatile UDS root (cleared on reboot).";
    };

    # Nested to match nix/home-manager-module.nix's own `router.*`
    # shape (parity decision, 2026-09-24 — see AGENTS.md) rather than
    # this module's own prior flat routerPort/routerHost/externalUrl
    # options, which this renames.
    router = {
      port = lib.mkOption {
        type = lib.types.port;
        default = 1337;
        description = "TCP port the router listens on.";
      };

      host = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = ''
          Interface the router binds, passed through as
          `--host`/`CONEXUS_ROUTER_HOST`-equivalent config. Defaults to
          loopback — matching the application's deliberately-safe
          default — so a bare import of this module keeps the router
          behind a reverse proxy (nginx on loopback handles
          rate-limiting, XFF sanitization, TLS) instead of exposing it
          on every interface. The VM configs (nix/vm.nix) override
          this to "0.0.0.0" because qemu user-mode hostfwd delivers
          packets to the guest's primary IP, not loopback — see the
          comment there. No home-manager-module.nix counterpart (a
          user-scope router only ever binds loopback), so this option
          is system-mode-only.
        '';
      };

      externalUrl = lib.mkOption {
        type = lib.types.str;
        default = "http://localhost:5454";
        description = ''
          Base URL the host can reach the router at. Rendered into
          copy-pastable .mcp.json snippets, so it has to match
          whatever actually forwards traffic to this host (a reverse
          proxy, or the VM wrapper's qemu hostfwd).
        '';
      };

      idleSec = lib.mkOption {
        type = lib.types.ints.positive;
        default = 14400;
        description = ''
          Seconds of inactivity before the router stops a per-project
          backend. Default is 4h, matching the dashboard's "sleeping"
          status bucket.
        '';
      };

      defaultWorkspaceParent = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.stateDir}/projects";
        description = ''
          Where POST /conexus/api/router/projects puts a project's
          workspace when the user leaves the "Workspace" form field
          blank. Each project then lives at
          `''${defaultWorkspaceParent}/<name>/`. Defaults to
          `''${stateDir}/projects`, matching where the module's own
          tmpfiles rule already creates the workspace parent.
        '';
      };
    };

    dashboard = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Whether to build and serve the Next.js dashboard. Disable
          to save build time on headless hosts; the MCP transport
          still works without the dashboard.
        '';
      };

      package = lib.mkOption {
        type = lib.types.package;
        default = pkgs'.conexusDashboard;
        defaultText = lib.literalMD
          "the dashboard built from `services.conexus.src` via `nix/packages.nix`";
        description = "Dashboard derivation (Next.js static export).";
      };
    };

    conexusLauncherPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        The CoNexus Rust backend's systemd-template launcher (`nix/
        conexus.nix`'s `conexusLauncher`), for the `conexus@<name>.service`
        template -- the ONLY per-project backend implementation now
        that the Python one was retired. `null` (the default) omits
        the template entirely — this module has no direct access to
        the `crane` flake input, so whichever caller DOES (the flake's
        own outputs, `nix/vm.nix`) is responsible for building `nix/
        conexus.nix` and setting this option, the same "build
        elsewhere, pass the package in" pattern `src` already uses one
        option up.
      '';
    };

    conexusRouterPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        The CoNexus Rust router wrapper (`nix/conexus.nix`'s
        `conexusRouterWrapper`), for the singleton `conexus-router`
        system service -- the ONLY router implementation now that the
        Python one (`agent-mcp-router.service`) was retired. `null`
        (the default) omits the service entirely, same "build
        elsewhere, pass the package in" pattern as
        `conexusLauncherPackage` above.
      '';
    };

    conexusDaemonAgentPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        The CoNexus Rust reference daemon-agent binary (`nix/
        conexus.nix`'s `conexusDaemonAgentWrapper`), for
        `conexus-daemon-agent@<instance>.service`. `null` (the
        default) omits every `daemonAgents` entry's unit entirely,
        same "build elsewhere, pass the package in, no package means
        no unit" pattern as `conexusLauncherPackage`/
        `conexusRouterPackage` above.
      '';
    };

    multiTenant = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        When true (default), the router runs in multi-tenant mode:
        projects are registered at runtime via
        POST /conexus/api/router/projects and the dashboard's
        overview lists them all.

        When false, the router runs in single-tenant mode (N=1):
        `services.conexus.singleProject` declares the only project,
        the module seeds projects.local.json before the router starts,
        and the router 410s every project-lifecycle write endpoint
        plus 302-redirects any wrong-project URL to the configured one
        (ADR-0008, plan decisions #1 + #9).
      '';
    };

    sso = {
      # See nix/home-manager-module.nix's own `sso` option doc for the
      # full OIDC-vs-proxy-header rationale — identical here, this is
      # the system-mode counterpart of the exact same feature.
      oidc = lib.mkOption {
        type = lib.types.nullOr (lib.types.submodule {
          options = {
            issuer = lib.mkOption {
              type = lib.types.str;
              example = "https://keycloak.example.com/realms/conexus";
              description = ''
                OIDC issuer URL. The router fetches its discovery
                document at `''${issuer}/.well-known/openid-configuration`
                and binds the resulting authorize / token endpoints.
              '';
            };
            clientId = lib.mkOption {
              type = lib.types.str;
              example = "conexus";
              description = "RP client identifier registered with the IdP.";
            };
            clientSecretFile = lib.mkOption {
              type = lib.types.path;
              example = "/run/secrets/conexus-oidc-client-secret";
              description = ''
                Path to a chmod-0600 file holding the OIDC client
                secret, readable by `services.conexus.user`. The
                router reads it once at startup; secret rotation is
                "edit the file, restart the unit".
              '';
            };
            providerName = lib.mkOption {
              type = lib.types.str;
              default = "SSO";
              example = "Keycloak";
              description = ''
                Display name on the login page's "Sign in with ..."
                button. Purely cosmetic; the router doesn't validate
                this against the issuer.
              '';
            };
            groupMapping = lib.mkOption {
              type = lib.types.attrsOf lib.types.str;
              default = { };
              example = {
                "eng-backend" = "backend-team";
                "*" = "";
              };
              description = ''
                Map IdP-supplied group claims to conexus groups.
                Each entry is `oidc_group_name = conexus_group_name`.
                Unmapped claims are silently ignored.

                Special: an entry with key `"*"` enables the wildcard
                JIT escape — every unmatched group claim auto-creates
                a sanitized conexus group (lowercase, dashes only)
                and the user is added.
              '';
            };
            scopes = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ "openid" "profile" "email" "groups" ];
              description = ''
                OAuth2 scopes requested at the IdP. Defaults to
                openid + profile + email + groups; trim if the IdP
                rejects unknown scopes.
              '';
            };
            redirectUrl = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              example = "https://router.example.com/conexus/sso/callback";
              description = ''
                Override the redirect URL handed to the IdP.
                Defaults to `''${router.externalUrl}/conexus/sso/callback`
                — only set this if the IdP's registered redirect URI
                differs from the natural one.
              '';
            };
          };
        });
        default = null;
        description = ''
          OIDC SSO. When non-null, the dashboard's login page swaps
          the username/password form for a single "Sign in with
          ''${providerName}" button that initiates the OAuth2
          authorization-code + PKCE flow. Mutually exclusive with
          `sso.proxyHeader`.
        '';
      };

      proxyHeader = lib.mkOption {
        type = lib.types.nullOr (lib.types.submodule {
          options = {
            trustHeader = lib.mkOption {
              type = lib.types.str;
              default = "Remote-User";
              description = ''
                HTTP header carrying the trusted username. Common
                values: `Remote-User` (nginx + oauth2-proxy),
                `X-Forwarded-User` (traefik + forward-auth),
                `Tailscale-User-Login` (tailscale-serve).
              '';
            };
            trustedIps = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ "127.0.0.1" "::1" ];
              example = [ "127.0.0.1" "::1" "10.0.0.5" ];
              description = ''
                Source IPs the router will accept the trusted header
                from. CRITICAL SAFETY RULE: any other source is
                treated as a spoof attempt and the header is silently
                ignored. Default is localhost-only — appropriate for
                deploys where the upstream proxy runs on the same
                host (the common pattern).
              '';
            };
            defaultIsSysadmin = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = ''
                When a JIT-created user lands via the proxy-header
                path, give them the sysadmin bit. Default false —
                the operator promotes users to sysadmin manually
                via the dashboard. Set true ONLY when the upstream
                proxy is your sole, well-trusted auth boundary.
              '';
            };
          };
        });
        default = null;
        description = ''
          Proxy-header trust SSO. When non-null, the router accepts
          the configured header as a session-equivalent identity
          provided the request originates from one of `trustedIps`.
          Mutually exclusive with `sso.oidc`.
        '';
      };
    };

    singleProject = lib.mkOption {
      type = lib.types.nullOr (lib.types.submodule {
        options = {
          name = lib.mkOption {
            type = lib.types.strMatching "^[a-z]([a-z0-9-]*[a-z0-9])?$";
            example = "washing-brothers";
            description = ''
              Slug for the single-tenant project. Must match the
              router's slug regex (lowercase letters / digits /
              hyphens; no underscores; no leading/trailing hyphen).
              Single-letter names are permitted.
            '';
          };
          workspace = lib.mkOption {
            type = lib.types.str;
            example = "/srv/conexus/washing-brothers";
            description = ''
              Absolute workspace path the backend will run against
              (--project-dir on conexus@<name>.service). The
              directory is NOT auto-created — the operator either
              provisions it ahead of time or hands the module an
              existing repo checkout, owned by `services.conexus.user`.
            '';
          };
        };
      });
      default = null;
      description = ''
        Required when `multiTenant = false`; must remain `null` when
        `multiTenant = true`. An assertion enforces the pairing so a
        half-toggled config fails at evaluation, not at runtime.
      '';
    };

    daemonAgents = lib.mkOption {
      type = lib.types.listOf (lib.types.submodule {
        options = {
          project = lib.mkOption {
            type = lib.types.str;
            example = "washing-brothers";
            description = ''
              Project slug. Must match an existing project registered
              via POST /conexus/api/router/projects.
            '';
          };
          agentId = lib.mkOption {
            type = lib.types.str;
            example = "backend-dev";
            description = ''
              Agent slug. Must match an existing agent on the project.
            '';
          };
          tokenPath = lib.mkOption {
            type = lib.types.str;
            example = "/run/secrets/conexus-washing-brothers-backend-dev-token";
            description = ''
              Absolute path to the file containing the agent's bearer
              token, readable by `services.conexus.user`. Wire this
              through sops-nix (or an equivalent secret-management
              module) in production.
            '';
          };
        };
      });
      default = [ ];
      description = ''
        Daemon-agent instances to enable. Each entry expands to one
        `conexus-daemon-agent@<project>--<agent_id>.service` unit,
        WantedBy `multi-user.target`.
      '';
    };
  };

  # NOTE: A legacy `autoProject` option used to live here, backed by
  # an `agent-mcp-bootstrap.service` oneshot that POSTed
  # `/agent-mcp/__create` on first boot. The `__create` endpoint was
  # deleted in ADR 0014 (see conexus/router/app.py:1410-1415) and
  # the REST replacement at `POST /api/router/projects` requires a
  # session cookie that a oneshot can't have. The bootstrap unit was
  # silently succeeding via curl -L following the empty-users
  # redirect to /setup (HTTP 200), then touching its marker file
  # without creating anything. Retired entirely — operators create
  # projects via the dashboard UI after first login. The first-boot
  # operator can still be auto-seeded by setting
  # `CONEXUS_BOOTSTRAP_USERNAME` / `_PASSWORD` on the router
  # service environment.

  config = lib.mkIf cfg.enable {
    # Catch the lopsided configurations at evaluation time, before the
    # operator wastes a `nixos-rebuild switch` on a broken system —
    # mirrors nix/home-manager-module.nix's own identical 3 assertions.
    assertions = [
      {
        assertion = cfg.multiTenant -> cfg.singleProject == null;
        message = ''
          services.conexus.singleProject must be null when
          services.conexus.multiTenant = true (multi-tenant mode
          discovers projects at runtime via
          POST /conexus/api/router/projects; the singleProject
          option only applies to single-tenant mode).
        '';
      }
      {
        assertion = (!cfg.multiTenant) -> cfg.singleProject != null;
        message = ''
          services.conexus.multiTenant = false requires
          services.conexus.singleProject = { name = "<slug>";
          workspace = "<abs-path>"; }. The router refuses to start
          without a configured project in single-tenant mode (the
          dashboard would have no project to point at).
        '';
      }
      {
        assertion = !(cfg.sso.oidc != null && cfg.sso.proxyHeader != null);
        message = ''
          services.conexus.sso.oidc and services.conexus.sso.proxyHeader
          are mutually exclusive. Pick one: OIDC (authorization-code
          flow against an external IdP) OR proxy-header trust
          (upstream proxy supplies the username). Setting both would
          create surprising precedence rules and a security footgun.
        '';
      }
    ];

    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.stateDir;
      createHome = true;
    };
    users.groups.${cfg.group} = { };

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0750 ${cfg.user} ${cfg.group} - -"
      "d ${cfg.stateDir}/projects 0750 ${cfg.user} ${cfg.group} - -"
      "d ${cfg.runtimeDir} 0750 ${cfg.user} ${cfg.group} - -"
    ];

    # ── Router (conexus-router) ─────────────────────────────────────
    # `null` by default (see `conexusRouterPackage`'s own option doc)
    # -- the unit is simply omitted until a caller builds nix/conexus.nix
    # and sets the option, exactly like `conexus@` below.

    # Polkit rule so the router's unprivileged user can drive
    # conexus@*.service via systemctl without sudo prompts.
    security.polkit.enable = true;
    security.polkit.extraConfig = ''
      polkit.addRule(function(action, subject) {
        if (action.id == "org.freedesktop.systemd1.manage-units" &&
            subject.user == "${cfg.user}") {
          var unit = action.lookup("unit");
          if (unit && unit.indexOf("conexus@") == 0) {
            return polkit.Result.YES;
          }
        }
      });
    '';

    # `systemd.services` is assigned once, as a single attrset merged
    # with `//` against the dynamic daemon-agent units below — mirrors
    # nix/home-manager-module.nix's own identical reason (a bare
    # dotted-path assignment can't express a KEY computed per
    # `cfg.daemonAgents` list entry).
    systemd.services = {
    conexus-router = lib.mkIf (cfg.conexusRouterPackage != null) {
      description = "CoNexus router (URL-keyed, system-mode systemctl)";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" "ollama.service" ];
      environment = {
        # `--host`/bind-interface has no CLI flag on `conexus-router`
        # (mirrors the retired Python router's own CLI, which didn't
        # have one either) — env-var only, same as the Python router.
        # Defaults to loopback (cfg.router.host = "127.0.0.1"), the
        # safe production posture: the router sits behind an
        # nginx-on-loopback reverse proxy that owns rate-limiting, XFF
        # sanitization, and TLS. The VM configs override
        # cfg.router.host to "0.0.0.0" because qemu user-mode hostfwd
        # delivers packets to the guest's primary IP (not loopback),
        # so the VM must bind the wildcard to be reachable on the
        # host-forwarded port. See nix/vm.nix.
        CONEXUS_ROUTER_HOST = cfg.router.host;
        # `--default-workspace` has no CLI-flag equivalent on
        # `conexus-router` (env-var-only -- see rust/conexus-router/src/
        # main.rs's own `default_workspace_parent()` doc).
        CONEXUS_DEFAULT_WORKSPACE = cfg.router.defaultWorkspaceParent;
        # Crucial: VM has no per-user systemd instance — router
        # has to drive the system bus directly.
        CONEXUS_SYSTEMCTL_MODE = "system";
      }
      // lib.optionalAttrs (cfg.sso.oidc != null) {
        CONEXUS_SSO_OIDC_ISSUER = cfg.sso.oidc.issuer;
        CONEXUS_SSO_OIDC_CLIENT_ID = cfg.sso.oidc.clientId;
        CONEXUS_SSO_OIDC_CLIENT_SECRET_FILE = toString cfg.sso.oidc.clientSecretFile;
        CONEXUS_SSO_OIDC_PROVIDER_NAME = cfg.sso.oidc.providerName;
        CONEXUS_SSO_OIDC_GROUP_MAPPING = builtins.toJSON cfg.sso.oidc.groupMapping;
        CONEXUS_SSO_OIDC_SCOPES = lib.concatStringsSep " " cfg.sso.oidc.scopes;
      }
      // lib.optionalAttrs (cfg.sso.oidc != null && cfg.sso.oidc.redirectUrl != null) {
        CONEXUS_SSO_OIDC_REDIRECT_URL = cfg.sso.oidc.redirectUrl;
      }
      // lib.optionalAttrs (cfg.sso.proxyHeader != null) {
        CONEXUS_SSO_PROXY_HEADER = cfg.sso.proxyHeader.trustHeader;
        CONEXUS_SSO_PROXY_TRUSTED_IPS = lib.concatStringsSep "," cfg.sso.proxyHeader.trustedIps;
        CONEXUS_SSO_PROXY_DEFAULT_SYSADMIN = if cfg.sso.proxyHeader.defaultIsSysadmin then "true" else "false";
      };
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        # Needs to call `systemctl start conexus@…` against the
        # system bus, which requires root or polkit grants. Root
        # is the path of least friction inside a single-purpose VM.
        AmbientCapabilities = [ ];
        ExecStartPre = lib.mkIf (!cfg.multiTenant) [
          "${singleProjectSeedScript}"
        ];
        ExecStart = conexusExec.routerExecStart {
          routerPackage = cfg.conexusRouterPackage;
          port = cfg.router.port;
          projectsFile = "${cfg.stateDir}/projects.local.json";
          sockDir = cfg.runtimeDir;
          dashboardPackage = cfg.dashboard.package;
          externalUrl = cfg.router.externalUrl;
          idleSec = cfg.router.idleSec;
          singleProject = if cfg.multiTenant then null else cfg.singleProject;
        };
        Restart = "on-failure";
        RestartSec = 10;
      } // systemHardening;
    };

    # `conexus@<name>.service` — the CoNexus Rust backend template,
    # and the ONLY per-project backend implementation now that the
    # Python one (`agent-mcp@<name>.service`) was retired together
    # with the rest of the Python source tree.
    #
    # `null` by default (see `conexusLauncherPackage`'s option doc) —
    # the template is omitted entirely until a caller (the flake, or
    # `nix/vm.nix` for VM tests) actually builds `nix/conexus.nix` and
    # sets the option, since this module has no direct access to the
    # `crane` flake input.
    "conexus@" = lib.mkIf (cfg.conexusLauncherPackage != null) {
      description = "CoNexus backend — project %i (UDS)";
      after = [ "network.target" ];
      # F015 v4: lift systemd's default start-rate-limit (5 starts /
      # 10 s) so the unit can keep restarting through a transient
      # failure without the unit getting wedged by
      # ``Failed to schedule restart job: Start request repeated
      # too quickly``. StartLimit* live in [Unit], not [Service]
      # (per ``man systemd.unit``); NixOS exposes them as top-level
      # options on ``systemd.services.<name>``.
      startLimitBurst = 100;
      startLimitIntervalSec = 300;
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        RuntimeDirectory = "conexus/%i";
        RuntimeDirectoryMode = "0750";
        # F015 v3 (defence-in-depth): without this, systemd's default
        # ``RuntimeDirectoryPreserve=no`` wipes ``/run/conexus/%i/``
        # on every ``systemctl stop`` — including the
        # ``forwarding_hmac`` key file the backend's
        # ``--forwarding-hmac-in`` flag points at. Preserving the
        # runtime dir across restarts keeps the file alive between
        # stop/start cycles.
        #
        # SC-3: the flip side of preservation is that systemd will NOT
        # wipe ``/run/conexus/%i/`` when a project is deleted (only on
        # reboot, since it's tmpfs). The router's own project-deletion
        # handler owns that cleanup — it rmtree's
        # ``$CONEXUS_SOCK_DIR/<name>/`` after stopping the unit so the
        # ``forwarding_hmac`` key doesn't linger for a now-gone project.
        RuntimeDirectoryPreserve = "yes";
        Environment = [
          "CONEXUS_PROJECTS_FILE=${cfg.stateDir}/projects.local.json"
          "CONEXUS_SOCK_DIR=${cfg.runtimeDir}"
        ];
        # F015 v4/v6 (HMAC key generation, `runtimeShell` over
        # coreutils' missing `sh`): see nix/conexus-exec.nix's own doc
        # comment on `hmacSeedExecStartPre` — same logic, shared with
        # nix/home-manager-module.nix's identical unit.
        ExecStartPre = [
          conexusExec.hmacSeedExecStartPre
          (conexusExec.staleSocketExecStartPre cfg.runtimeDir)
        ];
        ExecStart = "${cfg.conexusLauncherPackage}/bin/conexus-launcher %i";
        Restart = "on-failure";
        RestartSec = 5;
        TimeoutStopSec = 10;
      } // systemHardening;
    };
    } // lib.listToAttrs (map (a: {
      name = "conexus-daemon-agent@${daemonAgentInstanceName a}";
      # Omitted when `conexusDaemonAgentPackage` is null — see that
      # option's own doc for why this mirrors `conexus@`/
      # `conexus-router`'s "no package, no unit" idiom.
      value = lib.mkIf (cfg.conexusDaemonAgentPackage != null) {
        description = "CoNexus daemon agent — ${daemonAgentInstanceName a} (event-driven wait_for_events loop)";
        after = [ "conexus-router.service" ];
        wants = [ "conexus-router.service" ];
        # StartLimit* live in [Unit], not [Service] (see the F015 v4
        # comment on `conexus@`'s own startLimitBurst/
        # startLimitIntervalSec above for the same NixOS attribute
        # placement). A 5-in-60s cap forces operator attention when
        # the token's missing rather than restart-storming forever.
        startLimitBurst = 5;
        startLimitIntervalSec = 60;
        serviceConfig = {
          Type = "simple";
          User = cfg.user;
          Group = cfg.group;
          # The Rust binary resolves its own token/URL/cursor paths
          # from the instance argument PLUS an explicit `--router-port`
          # flag (no per-build @router_port@ substitution needed).
          ExecStart = "${cfg.conexusDaemonAgentPackage}/bin/conexus-daemon-agent --router-port ${toString cfg.router.port} ${daemonAgentInstanceName a}";
          Restart = "on-failure";
          RestartSec = 10;
        } // systemHardening;
        wantedBy = [ "multi-user.target" ];
      };
    }) cfg.daemonAgents);

    # First-boot project bootstrap retired — see the autoProject
    # NOTE above. Operators create projects via the dashboard
    # `POST /api/router/projects` (which requires session-cookie
    # auth) after logging in.
  };
}
