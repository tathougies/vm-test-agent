{
  description = "An agent that sits inside a VM and communicates with a host harness to run commands.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, rust-overlay, crane, ... }:
    let systems = [ "x86_64-linux"
                    "aarch64-linux" ];
        forAllSystems = f: nixpkgs.lib.genAttrs systems (system:
          let pkgs = import nixpkgs {
                inherit system;
                overlays = [ rust-overlay.overlays.default ];
              };
              rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          in f system pkgs rust);

        mkPythonPackage = pkgs: pkgs.buildPythonPackage {
          pname = "vm_test_agent";
          version = "0.1.0";

          src = ./.;

          pyproject = true;

          build-system = [ pkgs.setuptools ];
        };
    in {
      packages = forAllSystems (system: pkgs: rust:
        let craneLib = (crane.mkLib pkgs).overrideToolchain rust;

            src = craneLib.cleanCargoSource ./.;

            commonArgs = {
              inherit src;
              strictDeps = true;
              nativeBuildInputs = [ pkgs.pkg-config ];
              buildInputs = [ pkgs.openssl ];
            };

            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            package = craneLib.buildPackage (commonArgs // {
              inherit cargoArtifacts;
            });

        in {
          default = package;
          vm-test-agent = package;

          python = mkPythonPackage pkgs.python3.pkgs;
        });

      overlays.default = final: prev: {
        vm-test-agent = self.packages.${final.system}.vm-test-agent;
        pythonPackagesExtensions = prev.pythonPackagesExtensions ++ [
          (pyFinal: pyPrev: {
            vm_test_agent = mkPythonPackage pyFinal;
          })
        ];
      };

      devShells = forAllSystems (system: pkgs: rust: {
        default = pkgs.mkShell {
          packages = [
            rust
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.python3
          ];
          buildInputs = [
            pkgs.openssl
          ];
          RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
        };
      });
    };
}
