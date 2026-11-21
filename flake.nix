{
  description = "Flash: driver updater and PC health tool for Linux (and Windows)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        flash = pkgs.callPackage ./nix/package.nix { };
        default = flash;
      });

      apps = forAllSystems (pkgs: rec {
        flash = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.flash}/bin/flash";
          meta.description = "Run Flash";
        };
        default = flash;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.flash ];
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
          ];
          # winit and glow load these with dlopen.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
            with pkgs;
            [
              libxkbcommon
              libGL
              wayland
              libx11
              libxcursor
              libxi
              libxrandr
              libxcb
            ]
          );
        };
      });

      overlays.default = final: _prev: {
        flash = final.callPackage ./nix/package.nix { };
      };

      nixosModules.default = import ./nix/module.nix self;
      nixosModules.flash = self.nixosModules.default;

      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.flash;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
