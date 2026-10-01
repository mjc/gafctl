# Updraft

Updraft is a Rust service for reading supported BLE and QuickConnect ventilation controllers and exposing state to Home Assistant over HTTP, MQTT, or both. Cloud account support is optional. Controls use capability-specific commands, and cloud writes are disabled by default.

## Workspace

| Crate | Purpose |
| --- | --- |
| `updraft-protocol` | Typed commands, values, and frame parsing. |
| `updraft-bluetooth` | BLE discovery, connection, and protocol transport. |
| `updraft-quickconnect` | Optional QuickConnect authentication, HTTP client, and state model. |
| `updraft` | CLI, HTTP API, and optional MQTT bridge. |

The `updraft` executable lives in root `src/`. Libraries live under `crates/`.
The executable depends on Bluetooth and protocol; Bluetooth depends on protocol.
Protocol has no transport or application dependencies.

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

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`. Run the workspace checks with:

```sh
devenv tasks run check:all
```
