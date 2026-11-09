{ config, lib, pkgs, ... }:

let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.voxelatlas;
  format = pkgs.formats.toml { };
  output = "/var/lib/voxelatlas/output";
  configFile = format.generate "voxelatlas.toml" (cfg.settings // { inherit output; });
  worldType = types.submodule {
    options = {
      path = mkOption {
        type = types.str;
        description = "Absolute world root containing region/, DIM-1/, or DIM1/.";
      };
      dimension = mkOption {
        type = types.enum [ "overworld" "nether" "end" ];
        default = "overworld";
        description = "Dimension to read beneath the world root.";
      };
      exclude = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "87" "43:6" ];
        description = "Legacy block IDs or ID:metadata pairs omitted from rendering.";
      };
    };
  };
  mapType = types.submodule {
    options = {
      id = mkOption { type = types.str; description = "Unique map identifier."; };
      world = mkOption { type = types.str; description = "Key in settings.worlds."; };
      view = mkOption { type = types.enum [ "isometric" "topdown" ]; default = "isometric"; description = "Projection."; };
      rotations = mkOption { type = types.listOf (types.ints.between 0 3); default = [ 0 ]; description = "Unique rotations; topdown requires [ 0 ]."; };
      mode = mkOption { type = types.enum [ "daylight" "plain" ]; default = "daylight"; description = "Lighting mode."; };
      water_opacity = mkOption { type = types.numbers.between 0.0 1.0; default = 0.8; description = "Water opacity."; };
      lighting_water_intensity = mkOption { type = types.numbers.between 0.0 1.0; default = 0.5; description = "Underwater lighting intensity."; };
      default_zoom = mkOption { type = types.ints.unsigned; default = 7; description = "Initial viewer zoom."; };
    };
  };
  serviceConfig = lib.optionalAttrs (cfg.cpuQuota != "") { CPUQuota = cfg.cpuQuota; } // lib.optionalAttrs (cfg.allowedCPUs != "") { AllowedCPUs = cfg.allowedCPUs; } // {
    Type = "oneshot";
    TimeoutStartSec = "infinity";
    Slice = "voxelatlas.slice";
    User = "voxelatlas";
    Group = "voxelatlas";
    SupplementaryGroups = cfg.supplementaryGroups;
    StateDirectory = "voxelatlas";
    StateDirectoryMode = "0755";
    UMask = "0022";
    WorkingDirectory = "/var/lib/voxelatlas";
    MemoryMax = cfg.memoryMax;
    MemoryHigh = cfg.memoryHigh;
    MemorySwapMax = cfg.memorySwapMax;
    CPUWeight = 1;
    IOWeight = 1;
    Nice = 19;
    OOMPolicy = "stop";
    OOMScoreAdjust = 500;
    IOSchedulingClass = "idle";
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    PrivateDevices = true;
    PrivateNetwork = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    RestrictAddressFamilies = [ "AF_UNIX" ];
    CapabilityBoundingSet = "";
    ReadWritePaths = [ "/var/lib/voxelatlas" ];
  };
  # No user-controlled strings are interpolated into nginx directives other
  # than this validated URI prefix; filesystem paths are fixed by the module.
  prefix = cfg.nginx.location;
in {
  options.services.voxelatlas = {
    enable = mkEnableOption "VoxelAtlas scheduled offline rendering";
    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ../default.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ../default.nix { }";
      description = "VoxelAtlas executable package.";
    };
    settings = mkOption {
      type = types.submodule {
        freeformType = format.type;
        options = {
          output = mkOption { type = types.str; readOnly = true; default = output; description = "Module-managed output directory."; };
          textures = mkOption { type = types.str; description = "External Minecraft texture directory; never copied into the Nix store."; };
          workers = mkOption { type = types.ints.between 1 256; default = 4; description = "Rendering worker count."; };
          cache_mib = mkOption { type = types.ints.between 16 65536; default = 128; description = "Combined decoded-chunk cache budget in MiB (not total RSS). Increase after measuring if speed matters more than RAM."; };
          tile_size = mkOption { type = types.enum [ 256 512 ]; default = 512; description = "Tile width and height in pixels."; };
          encoding = mkOption { type = types.enum [ "fast" "compact" "indexed" ]; default = "fast"; description = "fast and compact keep every rendered pixel: fast prioritizes generation time, compact spends more encoding CPU on isometric base tiles to reduce storage. indexed additionally reduces each tile to a 256-colour adapted palette, roughly halving isometric storage for a small visible colour change, and is the same kind of reduction Mapcrafter's png_indexed applies."; };
          skip_corrupt_chunks = mkOption { type = types.bool; default = false; description = "Warn and omit corrupt chunks without modifying input files. Source changes, I/O failures and unsupported Minecraft versions remain fatal."; };
          worlds = mkOption { type = types.attrsOf worldType; default = { }; description = "Read-only input worlds."; };
          maps = mkOption { type = types.listOf mapType; default = [ ]; description = "Maps and rotations to generate."; };
        };
      };
      default = { };
      description = "Renderer configuration serialized to TOML. Do not put secrets here: it is public in the Nix store.";
    };
    render.schedule = mkOption {
      type = types.str;
      default = "*-*-* 0/3:00:00";
      description = "systemd OnCalendar expression for incremental rendering.";
    };
    supplementaryGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "minecraft" ];
      description = "Groups granting read access to worlds, textures, and player data. The service never changes their ownership.";
    };
    memoryMax = mkOption {
      type = types.str;
      default = "3G";
      description = "Hard aggregate systemd memory limit for all VoxelAtlas jobs, also applied per job; size workers and cache_mib accordingly.";
    };
    memoryHigh = mkOption {
      type = types.str;
      default = "2560M";
      description = "Aggregate systemd memory pressure threshold, also applied per job; keep below memoryMax.";
    };
    memorySwapMax = mkOption {
      type = types.str;
      default = "0";
      description = "Aggregate systemd swap limit, also applied per job; zero prevents renderer swap thrashing.";
    };
    cpuQuota = mkOption {
      type = types.str;
      default = "";
      example = "400%";
      description = ''
        Optional hard aggregate systemd CPU quota, also applied per job; 100% is one CPU.
        Empty by default: rendering runs at Nice 19, CPUWeight 1 and idle I/O
        priority to favor other services. This is not complete resource isolation.
        Set a quota to cap CPU time. Independent of settings.workers.
      '';
    };
    allowedCPUs = mkOption {
      type = types.str;
      default = "";
      example = "0-3";
      description = "Optional CPU set for affinity (systemd AllowedCPUs syntax). Empty permits all CPUs within cpuQuota; settings.workers limits task parallelism.";
    };
    dynamic = {
      enable = mkEnableOption "scheduled updates from local player-data files (no database or network)";
      playerDataDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "External directory passed to update-data --players; must be readable by voxelatlas.";
      };
      identitiesFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional runtime UUID-to-name/head JSON passed with --identities; never put database credentials in Nix expressions.";
      };
      schedule = mkOption { type = types.str; default = "*-*-* *:*:00"; description = "systemd OnCalendar expression for player updates."; };
    };
    nginx = {
      enable = mkEnableOption "nginx static map hosting";
      virtualHost = mkOption { type = types.str; default = "localhost"; description = "nginx virtual host to extend; TLS can be configured on that host separately."; };
      location = mkOption { type = types.strMatching "/([A-Za-z0-9_-]+/)*"; default = "/map/"; description = "Safe URI prefix, with leading and trailing slash."; };
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    assertions = [
      { assertion = cfg.settings.maps != [ ]; message = "services.voxelatlas.settings.maps must contain at least one map."; }
      { assertion = lib.hasPrefix "/" cfg.settings.textures && lib.all (world: lib.hasPrefix "/" world.path) (lib.attrValues cfg.settings.worlds); message = "VoxelAtlas textures and world paths must be absolute."; }
      { assertion = !cfg.dynamic.enable || (cfg.dynamic.playerDataDirectory != null && lib.hasPrefix "/" cfg.dynamic.playerDataDirectory); message = "VoxelAtlas dynamic updates require an absolute dynamic.playerDataDirectory."; }
    ];

    users.groups.voxelatlas = { };
    users.users.voxelatlas = {
      isSystemUser = true;
      group = "voxelatlas";
      home = "/var/lib/voxelatlas";
    };

    systemd.slices.voxelatlas = {
      description = "Shared VoxelAtlas CPU and memory budget";
      sliceConfig = {
        MemoryMax = cfg.memoryMax;
        MemoryHigh = cfg.memoryHigh;
        MemorySwapMax = cfg.memorySwapMax;
        CPUWeight = 1;
        IOWeight = 1;
      } // lib.optionalAttrs (cfg.cpuQuota != "") { CPUQuota = cfg.cpuQuota; }
        // lib.optionalAttrs (cfg.allowedCPUs != "") { AllowedCPUs = cfg.allowedCPUs; };
    };

    systemd.services.voxelatlas-render = {
      description = "Render VoxelAtlas static maps";
      serviceConfig = serviceConfig // {
        ExecStartPre = "${cfg.package}/bin/voxelatlas validate --config ${configFile}";
        ExecStart = "${cfg.package}/bin/voxelatlas render --config ${configFile}";
      };
    };
    systemd.timers.voxelatlas-render = {
      description = "Schedule incremental VoxelAtlas renders";
      wantedBy = [ "timers.target" ];
      timerConfig = { OnCalendar = cfg.render.schedule; Persistent = true; };
    };

    systemd.services.voxelatlas-update-data = mkIf cfg.dynamic.enable {
      description = "Update VoxelAtlas local player data";
      serviceConfig = serviceConfig // {
        ExecStart = lib.escapeShellArgs ([ "${cfg.package}/bin/voxelatlas" "update-data" "--config" (toString configFile) "--players" cfg.dynamic.playerDataDirectory ] ++ lib.optionals (cfg.dynamic.identitiesFile != null) [ "--identities" cfg.dynamic.identitiesFile ]);
      };
    };
    systemd.timers.voxelatlas-update-data = mkIf cfg.dynamic.enable {
      description = "Schedule VoxelAtlas local player updates";
      wantedBy = [ "timers.target" ];
      timerConfig = { OnCalendar = cfg.dynamic.schedule; Persistent = true; };
    };

    services.nginx = mkIf cfg.nginx.enable {
      enable = true;
      virtualHosts.${cfg.nginx.virtualHost}.locations = {
        "^~ ${prefix}.state/".extraConfig = "return 404;";
        "^~ ${prefix}generations/".extraConfig = "return 404;";
        "${prefix}" = {
          alias = "${output}/";
          index = "index.html";
          extraConfig = ''
            autoindex off;
            add_header Cache-Control "no-cache" always;
          '';
        };
        "^~ ${prefix}tiles/" = {
          alias = "${output}/tiles/";
          extraConfig = ''
            autoindex off;
            add_header Cache-Control "public, max-age=31536000, immutable";
          '';
        };
        "^~ ${prefix}indexes/" = {
          alias = "${output}/indexes/";
          extraConfig = ''
            autoindex off;
            add_header Cache-Control "public, max-age=31536000, immutable";
          '';
        };
        "^~ ${prefix}live/" = {
          alias = "${output}/live/";
          extraConfig = ''
            autoindex off;
            add_header Cache-Control "public, max-age=5, must-revalidate";
          '';
        };
        "= ${prefix}current.json" = {
          alias = "${output}/current.json";
          extraConfig = ''
            add_header Cache-Control "no-cache, must-revalidate" always;
          '';
        };
      };
    };
  };
}
