{
  description = "VoxelAtlas Minecraft renderer, development environment, and NixOS service";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      packages = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = self.packages.${system}.voxelatlas;
          voxelatlas = pkgs.callPackage ./default.nix { };
        });

      devShells = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer ];
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        });

      nixosModules.default = import ./nix/module.nix;
      nixosModules.voxelatlas = self.nixosModules.default;

      checks = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in import ./nix/tests.nix {
          inherit pkgs;
          package = self.packages.${system}.default;
        });
    };
}
