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
    python3
    pkg-config
    mosquitto
  ] ++ lib.optionals pkgs.stdenv.isLinux [
    dbus
  ];

  tasks."check:fmt".exec = "cargo fmt --all -- --check";
  tasks."check:clippy".exec = "cargo clippy --workspace --all-targets --locked -- -D warnings";
  tasks."check:test".exec = "cargo nextest run --workspace --all-targets --locked";
  tasks."check:cli-bin".exec = "cargo nextest run --package updraft --no-default-features --features cli --bin updraftctl --locked";
  tasks."check:cli".exec = "cargo nextest run --package updraft --no-default-features --features http,mqtt,cli --test cli --locked";
  tasks."check:doc".exec = "cargo test --workspace --doc --locked";
  tasks."check:ha".exec = "python3 -m unittest discover -s tests -p test_updraft_client.py";
  tasks."build:service".exec = "cargo build --release --locked --no-default-features --features http,mqtt --bin updraft";
  tasks."build:cli".exec = "cargo build --release --locked --no-default-features --features cli --bin updraftctl";
  tasks."check:all".after = [ "check:fmt" "check:clippy" "check:test" "check:cli-bin" "check:cli" "check:doc" "check:ha" ];
}
