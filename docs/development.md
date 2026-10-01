# Development

Use the repository's pinned devenv environment. Rust is pinned in
`rust-toolchain.toml`.

```sh
devenv allow
devenv shell
```

Inside that shell, run the executable with `cargo run --`, for example:

```sh
cargo run -- ble scan
cargo run -- devices --format json
```

If `DEVENV_ROOT` points to this repository, run Cargo commands directly. If it
points elsewhere, start a fresh command from this repository's root.

## Checks

```sh
devenv tasks run check:all
```

This runs formatting, Clippy with warnings denied, Rust tests through nextest,
Rust doctests, and the Python Home Assistant client tests. The individual tasks
are `check:fmt`, `check:clippy`, `check:test`, `check:doc`, and `check:ha`.

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
