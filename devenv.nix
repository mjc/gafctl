{ pkgs, lib, ... }:
{
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  packages = with pkgs; [
    cargo-nextest
    cargo-llvm-cov
    cargo-deny
    cargo-machete
    bacon
    pkg-config
    mosquitto
  ] ++ lib.optionals pkgs.stdenv.isLinux [
    dbus
  ];

  tasks."check:fmt".exec = "cargo fmt --all -- --check";
  tasks."check:clippy".exec = "cargo clippy --workspace --all-targets --locked -- -D warnings";
  tasks."check:test".exec = "cargo nextest run --workspace --all-targets --locked";
  tasks."check:doc".exec = "cargo test --workspace --doc --locked";
  tasks."check:all".after = [ "check:fmt" "check:clippy" "check:test" "check:doc" ];
}
