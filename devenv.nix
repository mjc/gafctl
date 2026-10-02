{
  pkgs,
  lib,
  ...
}: let
  linux = pkgs.stdenv.hostPlatform.isLinux;
  homeAssistant = pkgs.home-assistant.override {extraComponents = ["mqtt"];};
  registryPython =
    homeAssistant.python3Packages.python.withPackages (ps:
      [(ps.toPythonModule homeAssistant)] ++ homeAssistant.getPackages "mqtt" ps);
in {
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  packages = with pkgs;
    [
      cargo-nextest
      cargo-llvm-cov
      cargo-deny
      cargo-machete
      bacon
      python3
      ruff
      pkg-config
      mosquitto
    ]
    ++ lib.optionals linux [dbus];

  tasks."check:fmt".exec = "cargo fmt --all -- --check";
  tasks."check:clippy".exec = "cargo clippy --workspace --all-targets --all-features --locked -- -D warnings";
  tasks."check:clippy-cli".exec = "cargo clippy --package gafctl --all-targets --no-default-features --features cli --locked -- -D warnings";
  tasks."check:clippy-http".exec = "cargo clippy --package gafctl --all-targets --no-default-features --features http --locked -- -D warnings";
  tasks."check:test".exec = "cargo nextest run --workspace --all-targets --all-features --locked --status-level fail --final-status-level fail";
  tasks."check:cli-bin".exec = "cargo nextest run --package gafctl --no-default-features --features cli --bin gafctl --locked --status-level fail --final-status-level fail";
  tasks."check:http".exec = "cargo nextest run --package gafctl --all-targets --no-default-features --features http --locked --status-level fail --final-status-level fail";
  tasks."check:doc".exec = "cargo test --workspace --doc --locked";
  tasks."check:python-format".exec = "ruff format --check custom_components tests/*.py";
  tasks."check:python-lint".exec = "ruff check custom_components tests/*.py";
  tasks."check:ha".exec = "python3 -m unittest discover -s tests -p test_gafctl_client.py";
  tasks."check:ha-registry" = lib.mkIf linux {
    exec = ''
      set -eu
      mkdir -p "$DEVENV_ROOT/target/ha-registry"
      registry_dir=$(mktemp -d "$DEVENV_ROOT/target/ha-registry/run.XXXXXX")
      trap 'rm -rf "$registry_dir"' EXIT
      export TMPDIR="$registry_dir"
      export GAFCTL_DISCOVERY_FIXTURE="$registry_dir/discovery.json"
      cargo nextest run --package gafctl --all-features --locked \
        -E 'test(namespaced_templates_preserve_unknown_readings_and_freshness)' --status-level fail --final-status-level fail
      ${registryPython}/bin/python tests/test_homeassistant_registry.py
    '';
  };
  tasks."build:service".exec = "cargo build --release --locked --no-default-features --features http,mqtt --bin gafctl-server";
  tasks."build:cli".exec = "cargo build --release --locked --no-default-features --features cli --bin gafctl";
  tasks."check:all".after =
    [
      "check:fmt"
      "check:clippy"
      "check:clippy-cli"
      "check:clippy-http"
      "check:test"
      "check:cli-bin"
      "check:http"
      "check:doc"
      "check:ha"
      "check:python-format"
      "check:python-lint"
    ]
    ++ lib.optionals linux ["check:ha-registry"];
}
