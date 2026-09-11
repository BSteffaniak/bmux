{
  description = "bmux development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    cargoMacheteSrc = {
      url = "github:BSteffaniak/cargo-machete/ignored-dirs";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      cargoMacheteSrc,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
          config = {
            allowUnfreePredicate =
              pkg:
              builtins.elem (nixpkgs.lib.getName pkg) [
                "codeql"
              ];
          };
        };
        cargoMachete = pkgs.rustPlatform.buildRustPackage {
          pname = "cargo-machete";
          version = "ignored-dirs";
          src = cargoMacheteSrc;
          cargoLock = {
            lockFile = "${cargoMacheteSrc}/Cargo.lock";
          };
          doCheck = false;
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rustfmt"
            "clippy"
            "rust-src"
          ];
          targets = [
            "aarch64-linux-android"
            "x86_64-linux-android"
            "armv7-linux-androideabi"
          ];
        };
      in
      {
        packages.rust-toolchain = rustToolchain;
        # Native non-Nix CI can consume this value instead of floating rustup stable.
        rustVersion = rustToolchain.version;
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            cargo-deny
            cargo-nextest
            rust-analyzer
            cargo-ndk
            cargoMachete
            markdownlint-cli
            pkg-config
            openssl
            jdk21_headless
            fish
            codeql
          ];

          shellHook = ''
            for override in RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER; do
              if [ -n "''${!override:-}" ]; then
                echo "BMUX: $override conflicts with the Nix-owned compiler; unset it before entering." >&2
                exit 1
              fi
            done
            export PATH="${rustToolchain}/bin:$PATH"
            export RUSTC="${rustToolchain}/bin/rustc"
            export RUSTDOC="${rustToolchain}/bin/rustdoc"
            export BMUX_NIX_TOOLCHAIN="${rustToolchain}"
            checkout="$(git rev-parse --show-toplevel 2>/dev/null || pwd -P)"
            export CARGO_TARGET_DIR="$checkout/target/nix/${builtins.baseNameOf rustToolchain}"
            for tool in cargo rustc cargo-clippy clippy-driver; do
              if [ "$(command -v "$tool")" != "${rustToolchain}/bin/$tool" ]; then
                echo "BMUX: $tool does not resolve to the locked Nix toolchain" >&2
                exit 1
              fi
            done
            echo "bmux development environment loaded"
            echo "Available tools:"
            echo "  - cargo ($(cargo --version))"
            echo "  - rustc ($(rustc --version))"
            echo "  - clippy ($(cargo clippy --version))"
            echo "  - cargo-deny ($(cargo deny --version))"
            echo "  - cargo-machete ($(cargo machete --version))"
            echo "  - markdownlint ($(markdownlint --version))"
            echo "  - java ($(java -version 2>&1 | head -1))"
            echo "  - codeql ($(codeql --version | head -1))"
            echo ""
            echo "Run 'markdownlint *.md **/*.md' to lint all markdown files"
            echo "Run 'markdownlint --fix *.md **/*.md' to auto-fix markdown issues"
            echo "Run 'cargo clippy --all-targets -- -D warnings' for strict linting"

            # Only exec fish if we're in an interactive shell (not running a command)
            if [ -z "$IN_NIX_SHELL_FISH" ] && [ -z "$BASH_EXECUTION_STRING" ]; then
              case "$-" in
                *i*) export IN_NIX_SHELL_FISH=1; exec fish ;;
              esac
            fi
          '';
        };
      }
    );
}
