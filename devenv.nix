{
  pkgs,
  lib,
  ...
}: let
  linux = pkgs.stdenv.hostPlatform.isLinux;
  registryComponents = [
    "mqtt"
    "recorder"
  ];
  homeAssistant = pkgs.home-assistant.override {
    extraComponents = registryComponents;
  };
  registryPython = homeAssistant.python3Packages.python.withPackages (
    ps:
    [ (ps.toPythonModule homeAssistant) ]
    ++ lib.concatMap (component: homeAssistant.getPackages component ps) registryComponents
  );
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
      cargo-about
      cargo-machete
      bacon
      (python3.withPackages (ps: [ps.aiohttp]))
      ruff
      mypy
      pkg-config
      mosquitto
      podman
      docker-compose
      docker-client
      actionlint
      shellcheck
      curl
      jq
      nixfmt
    ]
    ++ lib.optionals linux [dbus];

  tasks."check:fmt".exec = "cargo fmt --all -- --check";
  tasks."check:clippy".exec = "cargo clippy --all-targets --all-features --locked -- -D warnings";
  tasks."check:clippy-cli".exec = "cargo clippy --all-targets --no-default-features --features cli --locked -- -D warnings";
  tasks."check:clippy-http".exec = "cargo clippy --all-targets --no-default-features --features http --locked -- -D warnings";
  tasks."check:no-features".exec = "cargo check --all-targets --no-default-features --locked";
  tasks."check:test".exec = "cargo nextest run --all-targets --all-features --locked --status-level fail --final-status-level fail";
  tasks."check:cli-bin".exec = "cargo nextest run --no-default-features --features cli --lib --bin gafctl --locked --status-level fail --final-status-level fail";
  tasks."check:http".exec = "cargo nextest run --all-targets --no-default-features --features http --locked --status-level fail --final-status-level fail";
  tasks."check:doc".exec = "cargo test --doc --locked";
  tasks."check:package".exec = "cargo package --locked";
  tasks."check:python-format".exec = "ruff format --check custom_components tests/*.py packaging";
  tasks."check:python-types".exec = "mypy custom_components/gafctl/client.py custom_components/gafctl/models.py custom_components/gafctl/controls.py";
  tasks."check:python-lint".exec = "ruff check custom_components tests/*.py packaging";
  tasks."check:ha".exec = "python3 -m unittest discover -s tests -p test_gafctl_client.py";
  tasks."check:release".exec = "python3 -m unittest discover -s packaging -p 'test_*.py'";
  tasks."check:licenses".exec = "packaging/licenses.sh --check";
  tasks."licenses:update".exec = "packaging/licenses.sh --update";
  tasks."check:ha-registry" = lib.mkIf linux {
    exec = ''
      set -eu
      mkdir -p "$DEVENV_ROOT/target/ha-registry"
      registry_dir=$(mktemp -d "$DEVENV_ROOT/target/ha-registry/run.XXXXXX")
      trap 'rm -rf "$registry_dir"' EXIT
      export TMPDIR="$registry_dir"
      export GAFCTL_DISCOVERY_FIXTURE="$registry_dir/discovery.json"
      cargo nextest run --all-features --locked \
        -E 'test(namespaced_templates_preserve_unknown_readings_and_freshness)' --status-level fail --final-status-level fail
      GAFCTL_HA_SKIP_PIP=1 ${registryPython}/bin/python tests/test_homeassistant_registry.py
    '';
  };
  tasks."check:install".exec = "packaging/check.sh";
  tasks."check:compose".exec = "packaging/check-compose.sh";
  tasks."check:install-native".exec = "python3 packaging/check-native.py";
  tasks."check:nix".exec = "nixfmt --check flake.nix nix/*.nix && nix flake check --all-systems --no-build";
  tasks."build:service".exec = "cargo build --release --locked --no-default-features --features http,mqtt --bin gafctl-server";
  tasks."build:cli".exec = "cargo build --release --locked --no-default-features --features cli --bin gafctl";
  tasks."check:all".after =
    [
      "check:nix"
      "check:fmt"
      "check:clippy"
      "check:clippy-cli"
      "check:clippy-http"
      "check:no-features"
      "check:test"
      "check:cli-bin"
      "check:http"
      "check:doc"
      "check:package"
      "check:ha"
      "check:release"
      "check:licenses"
      "check:python-format"
      "check:python-lint"
      "check:python-types"
    ]
    ++ lib.optionals linux ["check:ha-registry"];
}
