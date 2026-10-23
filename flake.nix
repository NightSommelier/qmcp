{
  description = "MCP server for a running QueryMT ACP WebSocket server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = inputs @ {self, ...}:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      systems = inputs.nixpkgs.lib.systems.flakeExposed;

      perSystem = {system, ...}: let
        overlays = [inputs.rust-overlay.overlays.default];
        pkgs = import inputs.nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        qmcp = rustPlatform.buildRustPackage {
          pname = "qmcp";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };
          env = {
            QMCP_GIT_SHA = self.shortRev or self.dirtyShortRev or "unknown";
          };
          auditable = false;
          doCheck = false;
        };
      in {
        packages = {
          qmcp = qmcp;
          default = qmcp;
        };

        apps = {
          qmcp = {
            type = "app";
            program = "${self.packages.${system}.qmcp}/bin/qmcp";
          };
          default = {
            type = "app";
            program = "${self.packages.${system}.qmcp}/bin/qmcp";
          };
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.pkg-config
            pkgs.openssl
            pkgs.alejandra
          ];

          shellHook = ''
            export PS1="(dev:qmcp) $PS1"
          '';
        };
      };
    };
}
