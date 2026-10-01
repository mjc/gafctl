# Updraft

Updraft is a Rust service for reading a supported BLE ventilation controller and exposing its state to Home Assistant over HTTP, MQTT, or both. Controls use a small set of supported presets. Updraft reports success only after a device acknowledgement and matching readback.

## Workspace

| Crate | Purpose |
| --- | --- |
| `updraft-protocol` | Typed commands, values, and frame parsing. |
| `updraft-bluetooth` | BLE discovery, connection, and protocol transport. |
| `updraft` | CLI, HTTP API, and optional MQTT bridge. |

The `updraft` executable lives in root `src/`. Libraries live under `crates/`.
The executable depends on Bluetooth and protocol; Bluetooth depends on protocol.
Protocol has no transport or application dependencies.

## Run

Use the pinned development environment:

```sh
devenv allow
devenv shell -- cargo run -- serve --device-id <local-device-id>
```

The API binds to loopback by default. Remote access requires `--allow-remote` and suitable access controls. The API does not provide authentication.

Set `UPDRAFT_MQTT_HOST`, `UPDRAFT_MQTT_PORT`, `UPDRAFT_MQTT_USERNAME`, and `UPDRAFT_MQTT_PASSWORD` to enable MQTT. Supply the password through a service manager or another local secret store. Do not commit deployment credentials.

The HTTP integration is the default Home Assistant entity source. MQTT discovery is optional. Use one entity source at a time to avoid duplicate entities. See [deployment](docs/deployment.md) and [Home Assistant transports](docs/home-assistant-entities.md).

The controller's reported fan flag is diagnostic state. It does not prove motor operation or airflow.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`. Run the workspace checks with:

```sh
devenv tasks run check:all
```
