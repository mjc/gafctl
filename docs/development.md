# Development

## Environment

Use `devenv shell` from the repository root. `devenv.lock` pins Nix inputs, `rust-toolchain.toml` pins Rust, and `Cargo.lock` pins Rust dependencies. Commit all three when changing the toolchain or dependencies.

If `DEVENV_ROOT` already matches this checkout, run Cargo directly. If it names a different checkout, start a fresh shell in this repository.

The environment includes Clippy, rustfmt, rust-analyzer, Rust sources, LLVM coverage tools, cargo-nextest, cargo-llvm-cov, cargo-deny, cargo-machete, and Bacon.

## Dependencies

Versions are declared in `[workspace.dependencies]`. Each crate opts into the dependencies and features it uses. The scaffold includes these dependencies ahead of their implementation.

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

[Axum](https://docs.rs/axum/0.8.9/axum/) fits the Tokio runtime and Tower middleware. HTTP/JSON is the planned interface between the service and a thin Home Assistant adapter. No endpoint paths or device schema are fixed yet.

[btleplug](https://github.com/deviceplug/btleplug) is a candidate for the legacy app's Bluetooth path. It supports BLE central operation on macOS and Linux; it does not support Bluetooth Classic. Device captures still need to confirm GAF's services, characteristics, and handshake. Linux builds need D-Bus development files, included by devenv; runtime access needs BlueZ and a usable adapter. On macOS, the process needs Bluetooth permission before device operations can work.

Wi-Fi starts with Tokio's TCP/UDP support. Socket availability does not establish whether the fan accepts connections on a home LAN or only its own access point.

## Add when needed

- `reqwest` with rustls for a verified HTTP device API or a later QuickConnect cloud backend.
- `tokio-rustls` for a verified TLS socket protocol.
- `bytes` and `tokio-util` codecs if captured framing requires buffered streaming.
- `mdns-sd` if the fan advertises services through mDNS.
- An MQTT client if MQTT becomes an explicit integration requirement.
- OpenAPI generation once the local API schema exists.
- Fuzzing once there is a parser, and benchmarks once there is a measured performance question.

These are candidates, not installed dependencies. No AWS SDK, database, or MQTT broker is required for the current scaffold.

## Checks

From the active devenv environment:

```sh
cargo check --workspace --all-targets --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked --no-tests=warn
cargo test --workspace --doc --locked
```

`devenv tasks run check:all` runs formatting, Clippy, nextest, and doctests. Nextest does not run doctests, so they have their own command. The scaffold currently has no tests; `--no-tests=warn` makes that visible without failing setup. Remove the allowance when the first tests are added. The nextest CI profile disables fail-fast and writes JUnit results under `target/nextest/ci/`.

Useful tools as implementation grows:

```sh
bacon clippy
cargo llvm-cov nextest --workspace --html
cargo deny check advisories sources
cargo machete
cargo tree --duplicates
```

Coverage becomes useful once tests exist. Machete will currently report planned dependencies as unused; review those findings as code lands. Do not suppress them globally. The dependency check command covers advisories and sources; a project license policy has not yet been selected.
