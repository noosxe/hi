{
  description = "hi, a minimalist agent harness (Rust TUI): package and dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-darwin" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "rustfmt" "clippy" ];
        };

        # The app itself (nix/app.nix), shared by packages and apps.
        hiApp = import ./nix/app.nix { inherit pkgs; };
      in
      {
        # The shell body lives in nix/devshell.nix; the flake stays the
        # root manifest that `nix develop` and direnv resolve.
        devShells.default = import ./nix/devshell.nix {
          inherit pkgs rustToolchain;
        };

        # The app itself (nix/app.nix): `nix run .` executes it,
        # `nix profile install github:noosxe/hi` exposes the binary.
        packages = rec {
          hi = hiApp;
          default = hi;
        };
        apps = rec {
          hi = flake-utils.lib.mkApp { drv = hiApp; } // {
            meta.description = "hi, a minimalist agent harness (Rust TUI)";
          };
          default = hi;
        };
      })
    // {
      # Not per-system: `pkgs.hi` for consumers that import this flake.
      overlays.default = final: prev: {
        hi = self.packages.${prev.stdenv.hostPlatform.system}.default;
      };
    };
}
