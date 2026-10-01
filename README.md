# Updraft

Updraft is a Rust service for reading supported BLE and QuickConnect ventilation controllers and exposing state to Home Assistant over HTTP, MQTT, or both. Cloud account support is optional. Controls use capability-specific commands, and cloud writes are disabled by default.

## Workspace

| Crate | Purpose |
| --- | --- |
| `updraft-api` | Shared v2 device, capability, state, and control contract. |
| `updraft-client` | Reusable HTTP client for a running Updraft service. |
| `updraft-protocol` | Typed commands, values, and frame parsing. |
| `updraft-bluetooth` | BLE discovery, connection, and protocol transport. |
| `updraft-quickconnect` | Optional QuickConnect authentication, HTTP client, and state model. |
| `updraft` | HTTP service and optional MQTT bridge. |
| `updraftctl` | Separate service-client and direct-BLE control CLI. |

The service executable lives in root `src/`; `updraftctl` is a separate binary
target. Libraries live under `crates/`. Bluetooth depends on the transport-free
protocol crate. The service client uses the shared API contract without importing
the server runtime.

The default Cargo features are `http` and `mqtt`; `mqtt` enables `http` and adds
the broker bridge. The control CLI is opt-in. Build the service without MQTT
using `--no-default-features --features http`; build only `updraftctl` with
`--no-default-features --features cli`.

## Run

Use the pinned development environment:

```sh
devenv allow
devenv shell -- cargo run -- serve
```

For the BLE controller, add `--device-id <peripheral-id>` or set `UPDRAFT_DEVICE_ID`. Without BLE configured, the service starts with an empty device inventory. The API binds to loopback by default. Remote access requires `--allow-remote` and suitable access controls. The API does not provide authentication. See the [HTTP API](docs/http-api.md) for the v2 contract.

Set `UPDRAFT_IDENTITY_STORE` to a private local file path whenever QuickConnect is configured. QuickConnect startup requires this persistent identity store so cloud devices keep stable Home Assistant identities across restarts. New identity files are created with owner-only permissions. Provider and account identifiers stay in that file and do not appear in public device payloads. This path alone does not enable cloud authentication or polling.

QuickConnect is enabled by setting `UPDRAFT_QUICKCONNECT_USERNAME` and exactly one of `UPDRAFT_QUICKCONNECT_PASSWORD` or `UPDRAFT_QUICKCONNECT_PASSWORD_FILE`. `UPDRAFT_QUICKCONNECT_ROLE` accepts `contractor` (default) or `consumer`. Prefer a service-manager credential file with owner-only permissions. QuickConnect works in cloud-only or mixed mode without a BLE device ID. Set `UPDRAFT_QUICKCONNECT_WRITES_ENABLED=true` only after field acceptance; otherwise QuickConnect remains read-only.

Set `UPDRAFT_MQTT_HOST`, `UPDRAFT_MQTT_PORT`, `UPDRAFT_MQTT_USERNAME`, and `UPDRAFT_MQTT_PASSWORD` to enable MQTT. Supply the password through a service manager or another local secret store. Do not commit deployment credentials.

The HTTP integration is the default Home Assistant entity source. MQTT discovery is optional. Use one entity source at a time to avoid duplicate entities. See [deployment](docs/deployment.md) and [Home Assistant transports](docs/home-assistant-entities.md).

The controller's reported fan flag is diagnostic state. It does not prove motor operation or airflow.

## CLI

Use the standalone control CLI to read or control devices through a running service:

```sh
updraftctl devices
updraftctl state configured --format json
updraftctl control configured preset timer-clear
updraftctl devices --server https://fan.example/updraft
```

Access a fan directly over BLE:

```sh
updraftctl ble scan
updraftctl ble state --device-id <peripheral-id> --format json
updraftctl ble control --device-id <peripheral-id> preset automatic-105-f-30-percent
```

During development, run these commands with
`devenv shell -- cargo run --no-default-features --features cli --bin updraftctl --`. See the
[CLI guide](docs/cli.md) for commands, controls, JSON output, timeouts, and exit
codes. The service keeps its `serve` and diagnostic `probe ble` commands.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`. Run the workspace checks with:

```sh
devenv tasks run check:all
```
