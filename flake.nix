{
  description = "Dev shell for a Rust + ratatui TUI app";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "rustfmt" "clippy" ];
        };
      in
      {
        # The shell body lives in nix/devshell.nix; the flake stays the
        # root manifest that `nix develop` and direnv resolve.
        devShells.default = import ./nix/devshell.nix {
          inherit pkgs rustToolchain;
        };
      });
}
