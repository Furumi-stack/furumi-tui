{ pkgs, ... }:

{
  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.97.0";
    components = [
      "cargo"
      "clippy"
      "rust-src"
      "rustfmt"
    ];
  };

  packages = [
    pkgs.pkg-config
  ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
    pkgs.alsa-lib
  ];

  env.RUST_BACKTRACE = "1";
}
