{ config, lib, pkgs, ... }:

let
  sourceRoot = builtins.toString ./.;
  sourceKey = builtins.substring 0 12 (builtins.hashString "sha256" sourceRoot);
  transientDotfile = "/tmp/furumi-tui-devenv-${sourceKey}";
  rustToolchain = builtins.fromTOML (builtins.readFile ./rust-toolchain.toml);
  rustVersion = rustToolchain.toolchain.channel;
in
{
  # Pure flakes only expose their read-only store source. Keep devenv's own
  # task/profile state writable, then restore the real checkout path in-shell.
  devenv.root = sourceRoot;
  devenv.dotfile = transientDotfile;
  devenv.state = "${transientDotfile}/state";

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  packages = [
    pkgs.pkg-config
  ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
    pkgs.alsa-lib
  ];

  env.RUST_BACKTRACE = "1";

  enterShell = lib.mkAfter ''
    export DEVENV_ROOT="$PWD"
    # Keep Nix builds separate from artifacts produced by rustup or another
    # devenv generation. Cargo metadata is not compatible across compilers.
    export CARGO_TARGET_DIR="$PWD/target/devenv-rust-${rustVersion}"
    export PATH="${config.languages.rust.toolchainPackage}/bin:$PATH"
    export RUSTC="${config.languages.rust.toolchainPackage}/bin/rustc"
    export RUSTDOC="${config.languages.rust.toolchainPackage}/bin/rustdoc"
    hash -r
  '';
}
