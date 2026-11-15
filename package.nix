{ lib, rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "voxelatlas";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./web ./tests ./examples ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Offline Minecraft Java 1.12.2 map renderer for static hosting";
    license = lib.licenses.mit;
    mainProgram = "voxelatlas";
    platforms = lib.platforms.linux;
  };
}
