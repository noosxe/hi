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
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain

            # Cargo tooling
            cargo-watch # watch & rebuild on change
            bacon # TUI test/check runner
            cargo-nextest # better test runner
            cargo-expand # macro expansion (needs nightly, see below)
            cargo-audit # security advisories
            cargo-outdated # dep updates

            # Native deps commonly needed by Rust crates
            pkg-config
            openssl

            # Misc
            rust-analyzer # fallback if the toolchain one is missing
          ];

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src-rust/library";

          shellHook = ''
            echo "rustc  : $(rustc --version)"
            echo "cargo  : $(cargo --version)"
            echo "editor : run 'bacon' or 'cargo watch -x run' to develop"
          '';
        };
      });
}
