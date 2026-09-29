# Development

## Environment

Use `devenv shell` from the repository root. `devenv.lock` pins Nix inputs, `rust-toolchain.toml` pins Rust, and `Cargo.lock` pins Rust dependencies. Commit all three when changing the toolchain or dependencies.

If `DEVENV_ROOT` already matches this checkout, run Cargo directly. If it names a different checkout, start a fresh shell in this repository.

The environment includes Clippy, rustfmt, rust-analyzer, Rust sources, LLVM coverage tools, cargo-nextest, cargo-llvm-cov, cargo-deny, cargo-machete, and Bacon.

## Dependencies

Versions are declared in `[workspace.dependencies]`. Each crate opts into the dependencies and features it uses.

| Area | Crates | Intended use |
| --- | --- | --- |
| Runtime | `tokio` | Async device sessions, TCP/UDP, timers, channels, and shutdown |
| Local API | `axum`, `serde`, `serde_json`, `tower-http` | HTTP/JSON routes, request limits, timeouts, request IDs, and tracing |
| Service configuration | `clap`, `toml` | CLI/environment options and a typed TOML configuration file |
| Errors | `thiserror`, `anyhow` | Typed library errors and service-level error context |
| Logging | `tracing`, `tracing-subscriber` | Structured events, filters, and optional JSON logs |
| Bluetooth candidate | `btleplug`, `futures-util`, `uuid` | BLE discovery, GATT access, event streams, and service identifiers |
| Protocol tests | `proptest` | Generated valid/invalid inputs and codec round trips once the format is known |
| API tests | `tower`, `http-body-util` | Exercise the Axum router and inspect response bodies without opening a port |

[Axum](https://docs.rs/axum/0.8.9/axum/) runs on Tokio and supports Tower middleware. The planned service API uses HTTP/JSON. Endpoint paths and the device schema are not defined.

[btleplug](https://github.com/deviceplug/btleplug) provides BLE Central on macOS and Linux. Updraft uses it with the GAF Wi-Fi Vent service `00FF` and characteristic `FF01`. Linux builds need D-Bus development files and BlueZ at runtime. macOS requires Bluetooth permission.

Tokio provides TCP/UDP support. The GAF Wi-Fi Vent accepts connections on its access point. Home-LAN access is unverified.

## Optional dependencies

- `reqwest` with rustls for a verified HTTP device API or a later QuickConnect cloud backend.
- `tokio-rustls` for a verified TLS socket protocol.
- `bytes` and `tokio-util` codecs if captured framing requires buffered streaming.
- `mdns-sd` if the fan advertises services through mDNS.
- An MQTT client if MQTT becomes an explicit integration requirement.
- OpenAPI generation once the local API schema exists.
- Fuzzing once there is a parser, and benchmarks once there is a measured performance question.

Add dependencies when implementing these features. AWS SDK, database, and MQTT support are not used.

## Checks

From the active devenv environment:

```sh
cargo check --workspace --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --all-targets --locked
cargo test --workspace --doc --locked
```

`check:test` runs nextest and fails when it finds no tests. `check:all` runs formatting, Clippy, nextest, and doctests. Nextest does not run doctests. The CI profile disables fail-fast and writes JUnit results under `target/nextest/ci/`.

Tools:

```sh
bacon clippy
cargo llvm-cov nextest --workspace --html
cargo deny check advisories sources
cargo machete
cargo tree --duplicates
```

Review unused-dependency reports as code changes.
