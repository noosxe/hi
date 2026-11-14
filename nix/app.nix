# The `hi` application, imported by the root flake (flake.nix) and
# exposed as `packages.<system>.default` / `apps.<system>.default` —
# `nix run github:noosxe/hi` and `nix profile install github:noosxe/hi`
# both land here. Where nix/devshell.nix serves contributors, this
# build serves users: release mode, no cargo tooling in the closure.
{
  pkgs,
}:
let
  # One version source: the crate manifest, not a copy in nix land.
  version = (builtins.fromTOML (builtins.readFile ../crates/cli/Cargo.toml)).package.version;
in
pkgs.rustPlatform.buildRustPackage {
  pname = "hi";
  inherit version;

  src = ../.;
  # The committed workspace lock: all registry deps, no git deps, so
  # no cargoHash needed — the lockfile itself pins the fetches.
  cargoLock.lockFile = ../Cargo.lock;

  # The full suite runs in CI (and via `cargo test` in the devshell);
  # `nix run` should not pay a second release-mode test compile.
  doCheck = false;

  # TLS is rustls (Cargo.lock has no openssl-sys): nothing native to
  # wire in, the closure stays lean.
  nativeBuildInputs = [ pkgs.installShellFiles ];

  # Completion scripts come from the binary itself (`hi --completions`),
  # so they can never drift from the flags. Skipped when cross-compiling:
  # the freshly built binary cannot run on the build machine.
  postInstall = pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
    installShellCompletion --cmd hi \
      --bash <($out/bin/hi --completions bash) \
      --zsh <($out/bin/hi --completions zsh) \
      --fish <($out/bin/hi --completions fish)
  '';

  meta = with pkgs.lib; {
    description = "A minimalist agent harness";
    homepage = "https://github.com/noosxe/hi";
    license = with licenses; [
      mit
      asl20
    ];
    mainProgram = "hi";
  };
}
