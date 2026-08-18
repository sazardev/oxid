{
  description = "Oxid — ephemeral, branch-based preview environments with real scale-to-zero.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "oxid";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

          src = pkgs.lib.cleanSource ./.;

          cargoLock.lockFile = ./Cargo.lock;

          # Cargo.toml pins `default-members = ["crates/oxid-cli"]` for a
          # plain `cargo build`; buildRustPackage inherits that default and
          # would otherwise only produce the `oxid` CLI binary, silently
          # dropping the `oxidd` daemon.
          buildAndTestSubdir = ".";
          cargoBuildFlags = [ "--workspace" ];

          # git2's vendored-libgit2/vendored-openssl features (Cargo.toml)
          # build those from source at compile time — cmake drives the
          # libgit2 build, perl the OpenSSL one. sqlx's bundled sqlite
          # feature needs a plain C compiler, already provided by stdenv.
          nativeBuildInputs = with pkgs; [
            cmake
            perl
            pkg-config
          ];

          buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
          ];

          # Both binaries (`oxid` the CLI, `oxidd` the daemon) build from one
          # workspace compilation; no per-package split needed here.
          doCheck = false; # the test suite spins up real Docker containers/SQLite, not sandboxable

          meta = {
            description = "Self-hosted control plane for ephemeral, branch-based preview environments";
            homepage = "https://github.com/sazardev/oxid";
            license = pkgs.lib.licenses.bsd0;
            mainProgram = "oxid";
          };
        };

        apps = {
          oxid = flake-utils.lib.mkApp {
            drv = self.packages.${system}.default;
            name = "oxid";
          };
          oxidd = flake-utils.lib.mkApp {
            drv = self.packages.${system}.default;
            name = "oxidd";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            cargo-audit
            cargo-deny
            gitleaks
          ];
        };
      }
    );
}
