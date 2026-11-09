# Import alongside ../nix/module.nix from a commit-pinned source.
# Replace the placeholder paths and use stable snapshots for rendering.
# This example uses a separate preview URL.
{ ... }:

let
  netherExclude = [
    "7" "10" "11" "13" "39" "40" "43:6" "44:6" "51"
    "87" "88" "89" "112" "113" "114" "153" "213"
  ];
  # Four isometric maps with four rotations, plus four top-down maps:
  # eight maps and twenty rendered views in total.
  names = {
    survival_ow = "Survival-Overworld";
    survival_nether = "Survival-Nether";
    survival_end = "Survival-End";
    creative_ow = "Creative-Overworld";
  };
  mapsFor = world: [
    {
      id = names.${world};
      inherit world;
      view = "isometric";
      rotations = [ 0 1 2 3 ];
      mode = if builtins.elem world [ "survival_nether" "survival_end" ] then "plain" else "daylight";
      water_opacity = 0.8;
      lighting_water_intensity = 0.5;
      default_zoom = 7;
    }
    {
      id = "${names.${world}}-Top";
      inherit world;
      view = "topdown";
      rotations = [ 0 ];
      mode = if builtins.elem world [ "survival_nether" "survival_end" ] then "plain" else "daylight";
      water_opacity = 0.8;
      lighting_water_intensity = 0.5;
      default_zoom = 7;
    }
  ];
in {
  services.voxelatlas = {
    enable = true;
    # Grant read/traverse access via the Minecraft group; do not chown worlds.
    supplementaryGroups = [ "mc" ];
    memoryMax = "3G";
    memoryHigh = "2560M";
    memorySwapMax = "0";
    cpuQuota = "";
    render.schedule = "*-*-* 0/3:00:00";
    settings = {
      textures = "/srv/minecraft/textures";
      workers = 8;
      cache_mib = 128;
      tile_size = 512;
      encoding = "indexed";
      worlds = {
        survival_ow = { path = "/srv/snapshots/Survival"; dimension = "overworld"; };
        creative_ow = { path = "/srv/snapshots/Creative"; dimension = "overworld"; };
        survival_nether = { path = "/srv/snapshots/Survival"; dimension = "nether"; exclude = netherExclude; };
        survival_end = { path = "/srv/snapshots/Survival"; dimension = "end"; };
      };
      maps = builtins.concatMap mapsFor [ "survival_ow" "creative_ow" "survival_nether" "survival_end" ];
    };
    dynamic = {
      enable = false;
      # Must belong to a configured world root; disable for snapshots without playerdata.
      playerDataDirectory = "/srv/snapshots/Survival/playerdata";
      schedule = "*-*-* *:*:00";
    };
    nginx = {
      enable = true;
      virtualHost = "maps.example.org";
      location = "/map-preview/";
    };
  };

  # Configure TLS on the ordinary NixOS nginx virtual host if desired:
  # services.nginx.virtualHosts."maps.example.org" = {
  #   enableACME = true;
  #   forceSSL = true;
  # };
  # security.acme.defaults.email = "admin@example.org";
  networking.firewall.allowedTCPPorts = [ 80 443 ];
}
