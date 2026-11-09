{ pkgs ? import <nixpkgs> { } }:

pkgs.rustPlatform.buildRustPackage {
  pname = "voxelatlas";
  version = "0.1.0";

  src = pkgs.lib.cleanSourceWith {
    src = ./.;
    filter = path: type:
      pkgs.lib.cleanSourceFilter path type
      && !(type == "directory" && builtins.elem (baseNameOf path) [ "target" ".direnv" ]);
  };
  cargoLock.lockFile = ./Cargo.lock;

  meta = with pkgs.lib; {
    description = "Offline Minecraft Java 1.12.2 map renderer for static hosting";
    license = licenses.mit;
    mainProgram = "voxelatlas";
    platforms = platforms.linux;
  };
}
