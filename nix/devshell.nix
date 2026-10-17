# The dev shell body, imported by the root flake (flake.nix).
# The flake stays the root manifest — `nix develop` and direnv's
# `use flake` both resolve it there — but the shell definition lives
# here so the root file stays a thin declaration of inputs/outputs.
{
  pkgs,
  # The pinned toolchain from the root flake (rust-overlay), shared
  # with RUST_SRC_PATH below.
  rustToolchain,
}:
pkgs.mkShell {
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
}
