# Development

Use the repository's pinned devenv environment. Rust is pinned in
`rust-toolchain.toml`.

```sh
devenv allow
devenv shell
```

Run the CLI inside that shell:

```sh
cargo run --no-default-features --features cli --bin gafctl -- ble scan
cargo run --no-default-features --features cli --bin gafctl -- devices --format json
```

Start the server directly with `cargo run --bin gafctl-server --`, using the
configuration in the deployment guide. To run `gafctl server`,
build both executables first:

```sh
cargo build --bins
./target/debug/gafctl server
```

If `DEVENV_ROOT` points to this repository, run Cargo commands directly. If it
points elsewhere, start a fresh command from this repository's root.

## Checks

```sh
devenv tasks run check:all
```

This runs formatting, Clippy with warnings denied for all features, CLI-only
and HTTP-only configurations, workspace tests with all features, CLI-only and
HTTP-only tests, Rust doctests, Ruff formatting and lint checks, strict typing of the Python
client, models and controls, and the Python
Home Assistant client tests.

On native Linux, `check:all` also runs `check:ha-registry`. That task uses Home
Assistant and MQTT dependencies pinned by `devenv.lock`, generates discovery
fixtures from Rust, and checks HA entity/device registries,
onboarding, reconfiguration, ownership transitions and MQTT templates. Each run
uses a configuration directory under `target/ha-registry` and runs natively.
The suite uses synthetic data and has no connection to the deployed HA or fan.

Run that suite separately on native Linux:

```sh
devenv tasks run check:ha-registry
```

The HA package and registry task require Linux. Other checks run on both platforms.

For a focused Rust test inside the shell:

```sh
cargo nextest run --workspace --all-targets --locked -E 'test(TEST_NAME)'
```

Replace `TEST_NAME` with a test name or substring. For the Python tests:

```sh
python3 -m unittest discover -s tests -p test_gafctl_client.py
```

The HA integration uses Python 3.14, typed config-entry runtime data, and shared
entity descriptions. `models.py` holds immutable device and reading records;
`client.py` validates HTTP payloads; `controls.py` defines number controls once
for entity setup, command validation and readback checks. The coordinator serializes writes, checks current ownership and
capabilities, sends once, and refreshes state after the response. Entities expose
readings and translate API errors for HA.

Run `devenv tasks run check:python-types` for strict typing. Format and lint Python changes with
`ruff format custom_components tests/*.py` and
`ruff check custom_components tests/*.py`.

Tests use fake Bluetooth transports, local HTTP servers, and synthetic cloud
fixtures. They check parsing, request validation, timeouts, state handling,
controls, and Home Assistant mapping. Hardware captures are documented in
[protocol findings](protocol-findings.md). QuickConnect tests use synthetic data;
live account compatibility is untested.

## Repository layout

| Path | Purpose |
| --- | --- |
| `src/` | Executable, HTTP service, CLI, state polling, and MQTT |
| `crates/gafctl-api/` | Shared device, state, capability, and command types |
| `crates/gafctl-client/` | HTTP client library for a running service |
| `crates/gafctl-protocol/` | Original controller's frames, commands, and values |
| `crates/gafctl-bluetooth/` | Bluetooth discovery and communication |
| `crates/gafctl-quickconnect/` | Cloud authentication, requests, and decoding |
| `custom_components/gafctl/` | Home Assistant integration |
| `fixtures/quickconnect/` | Synthetic cloud request and response examples |

The protocol crate has no Bluetooth or application dependency. The HTTP client
uses the shared API types without importing the service or Bluetooth runtime.

## Optional tools

Run these inside the development shell as needed:

```sh
bacon clippy
cargo llvm-cov nextest --workspace --html
cargo deny check advisories sources
cargo machete
cargo tree --duplicates
```
