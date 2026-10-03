{
  description = "GAF attic fan control and Home Assistant integration";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/f45c6f04c2f013f004bf94e284e95d72898d9393";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/49b6548d31019e8bfe9d4415193ac1df3c48f53a";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forSystems = nixpkgs.lib.genAttrs systems;
      packagesFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
    in
    {
      packages = forSystems (
        system:
        let
          pkgs = packagesFor system;
        in
        {
          default = self.packages.${system}.gafctl;
          gafctl = pkgs.callPackage ./nix/package.nix {
            rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          };
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          home-assistant = pkgs.callPackage ./nix/home-assistant.nix { };
        }
      );
      nixosModules.default = self.nixosModules.gafctl;
      nixosModules.gafctl = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.gafctl.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.gafctl;
      };
      checks = forSystems (
        system:
        let
          pkgs = packagesFor system;
          moduleCheck = import ./nix/checks.nix {
            inherit pkgs;
            lib = nixpkgs.lib;
            module = self.nixosModules.gafctl;
            homeAssistant = self.packages.${system}.home-assistant;
          };
        in
        nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          module = moduleCheck;
          service = moduleCheck.service;
        }
      );
    };
}
