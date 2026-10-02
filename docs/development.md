# Development

Use the repository's pinned devenv environment. Rust is pinned in
`rust-toolchain.toml`.

```sh
devenv allow
devenv shell
```

Inside that shell, select the control CLI explicitly:

```sh
cargo run --no-default-features --features cli --bin updraftctl -- ble scan
cargo run --no-default-features --features cli --bin updraftctl -- devices --format json
```

The service binary is `updraft`; start it with `cargo run -- serve` and the
configuration described in the deployment guide.

If `DEVENV_ROOT` points to this repository, run Cargo commands directly. If it
points elsewhere, start a fresh command from this repository's root.

## Checks

```sh
devenv tasks run check:all
```

This runs formatting, Clippy with warnings denied for all features, CLI-only
and HTTP-only configurations, workspace tests with all features, CLI-only and
HTTP-only tests, Rust doctests, and the Python Home Assistant client tests.

On native Linux, `check:all` also runs `check:ha-registry`. That task uses Home
Assistant and MQTT dependencies pinned by `devenv.lock`, generates discovery
fixtures from the Rust implementation, and exercises HA entity/device registries,
onboarding, reconfiguration, ownership transitions and MQTT templates. Each run
uses an isolated configuration directory under `target/ha-registry`; it does not
connect to a deployed Home Assistant instance or the fan. It runs without a VM.

Run that suite separately on native Linux:

```sh
devenv tasks run check:ha-registry
```

The HA package supports Linux, so this task is absent from the macOS environment.
The other checks run on both platforms.

For a focused Rust test inside the shell:

```sh
cargo nextest run --workspace --all-targets --locked -E 'test(TEST_NAME)'
```

Replace `TEST_NAME` with a test name or substring. For the Python tests:

```sh
python3 -m unittest discover -s tests -p test_updraft_client.py
```

Tests use fake Bluetooth transports, local HTTP servers, and synthetic cloud
fixtures. They check parsing, request validation, timeouts, state handling,
controls, and Home Assistant mapping. Hardware captures are documented in
[protocol findings](protocol-findings.md); QuickConnect tests do not establish
live cloud compatibility.

## Repository layout

| Path | Purpose |
| --- | --- |
| `src/` | Executable, HTTP service, CLI, state polling, and MQTT |
| `crates/updraft-api/` | Shared device, state, capability, and command types |
| `crates/updraft-client/` | HTTP client library for a running service |
| `crates/updraft-protocol/` | Original controller's frames, commands, and values |
| `crates/updraft-bluetooth/` | Bluetooth discovery and communication |
| `crates/updraft-quickconnect/` | Cloud authentication, requests, and decoding |
| `custom_components/updraft/` | Home Assistant integration |
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
