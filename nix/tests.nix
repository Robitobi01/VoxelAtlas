{ pkgs, package }:

let
  system = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      ./module.nix
      {
        boot.isContainer = true;
        fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
        system.stateVersion = "25.11";
        services.voxelatlas = {
          enable = true;
          inherit package;
          settings = {
            textures = "/srv/minecraft/textures";
            worlds.test.path = "/srv/minecraft/world";
            maps = [ { id = "test"; world = "test"; } ];
          };
          dynamic.enable = true;
          dynamic.playerDataDirectory = "/srv/minecraft/world/playerdata";
          nginx.enable = true;
        };
      }
    ];
  };
  cfg = system.config;
  render = cfg.systemd.services.voxelatlas-render;
  configFile = builtins.elemAt (pkgs.lib.splitString " --config " render.serviceConfig.ExecStart) 1;
in {
  vm = import ./vm-test.nix { inherit pkgs package; };

  module =
    assert pkgs.lib.all (item: item.assertion) cfg.assertions;
    assert render.serviceConfig.ProtectSystem == "strict";
    assert render.serviceConfig.ReadWritePaths == [ "/var/lib/voxelatlas" ];
    assert render.serviceConfig.MemoryMax == "2G";
    assert render.serviceConfig.MemoryHigh == "1536M";
    assert render.serviceConfig.MemorySwapMax == "0";
    # No CPU quota by default: renders use leftover capacity at Nice 19,
    # CPUWeight 1 and idle I/O priority rather than being capped.
    assert !(render.serviceConfig ? CPUQuota);
    assert !(render.serviceConfig ? AllowedCPUs);
    assert render.serviceConfig.CPUWeight == 1;
    assert render.serviceConfig.IOWeight == 1;
    assert render.serviceConfig.Nice == 19;
    assert render.serviceConfig.IOSchedulingClass == "idle";
    assert render.serviceConfig.Slice == "voxelatlas.slice";
    assert cfg.systemd.slices.voxelatlas.sliceConfig.MemoryMax == "2G";
    assert cfg.systemd.slices.voxelatlas.sliceConfig.CPUWeight == 1;
    assert cfg.systemd.timers.voxelatlas-render.timerConfig.OnCalendar == "*-*-* 0/3:00:00";
    assert cfg.systemd.timers.voxelatlas-update-data.timerConfig.OnCalendar == "*-*-* *:*:00";
    assert cfg.services.nginx.virtualHosts.localhost.locations."/map/".alias == "/var/lib/voxelatlas/output/";
    assert cfg.services.nginx.virtualHosts.localhost.locations."^~ /map/.state/".extraConfig == "return 404;";
    pkgs.runCommand "voxelatlas-module-smoke" { nativeBuildInputs = [ package ]; } ''
      voxelatlas validate --config ${configFile}
      touch "$out"
    '';
}
