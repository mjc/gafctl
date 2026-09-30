# Updraft

Updraft is a Rust BLE proxy for the GAF Wi-Fi Vent. It reads device state and has verified threshold and timer controls. Its HTTP API is read-only; the Home Assistant integration exposes state and diagnostics only. The service supports both HTTP polling and optional MQTT state publishing.

## Workspace

| Crate | Purpose |
| --- | --- |
| `updraft-protocol` | Typed commands, values, and frame parsing. |
| `updraft-bluetooth` | BLE discovery, connection, and protocol transport. |
| `updraft-wifi` | Reserved for Wi-Fi transport. |
| `updraft` | CLI, read-only HTTP API, and optional MQTT publisher. |

## GAF Wi-Fi Vent

The GAF Wi-Fi Vent app (`com.gaf.wifivent`) uses the same command family over BLE and Wi-Fi TCP. BLE service `00FF` and characteristic `FF01` carry commands and replies. The fan access point is `192.168.4.1`; setup connects to `GAFVent_XXXX`.

BLE reads return identity, mode, sensors, thresholds, and timer state. Threshold and timer writes return acknowledgements and readbacks. The Wi-Fi port and TLS configuration are unknown. Home-LAN setup is unconfirmed, and Wi-Fi control is not implemented. See [protocol findings](docs/protocol-findings.md).

## BLE probe

Run commands inside the repository's devenv:

```sh
cargo run -- probe ble --scan-only
cargo run -- probe ble
```

Use `--scan-only` to list nearby fans. If several appear, pass one ID to `--device-id`.

Set automatic thresholds in tenths of a degree Fahrenheit and tenths of a percent. The current values are 1050 and 300:

```sh
cargo run -- probe ble --set-auto-thresholds-tenths 1050 300
```

Set or clear the timer in minutes:

```sh
cargo run -- probe ble --set-timer-minutes 1
cargo run -- probe ble --set-timer-minutes 0
```

## Home Assistant

Run the HTTP API on the same host as Home Assistant, or use `--allow-remote` to
let Home Assistant poll a host on the LAN. MQTT push is optional and publishes
retained state with Home Assistant MQTT discovery:

```sh
cargo run -- serve --device-id DEVICE_ID_FROM_SCAN
```

The API defaults to `127.0.0.1:8787`; non-loopback binding requires the explicit
`--allow-remote` flag. The Home Assistant integration polls the API. To also
publish MQTT state, configure `UPDRAFT_MQTT_HOST`, `UPDRAFT_MQTT_PORT`,
`UPDRAFT_MQTT_USERNAME`, and `UPDRAFT_MQTT_PASSWORD`. The MQTT user only needs
write access to the Updraft state and Home Assistant discovery topics.

Copy `custom_components/updraft` into Home Assistant's `custom_components`
directory, restart Home Assistant, then add **Updraft GAF Vent** and enter the
HTTP API URL. MQTT discovery can be used alongside the polling integration.

The integration reports temperature, humidity, controller mode and fan flag,
firmware, thresholds, timer state, availability, and freshness. The controller
fan flag is diagnostic; it does not prove physical airflow. Controls and a
physical running entity are not exposed.

The probe reads state after a control command. Acknowledgements and readbacks report controller state; they do not measure airflow. Firmware update commands are not exposed.

## Protocol behavior

The protocol has five state queries and two controls. Queries run in sequence. Each snapshot entry keeps the response frame and its decoded value or payload error. Missing replies fail the query; unknown payloads stay in the snapshot. Control results keep the command, acknowledgement, and readback.

`Frame::parse` borrows a slice. `Frame::from_bytes` shares `Bytes` storage. `into_owned()` copies only when needed. The decoder parses complete frames in place and assembles fragments in `BytesMut`. BLE retains notification storage for the matching response.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`:

```sh
devenv allow
devenv shell
devenv tasks run check:all
```

`check:all` runs formatting, Clippy, nextest, and doctests. The workspace has protocol, Bluetooth, and CLI tests. The BLE probe has completed live state reads and control readbacks.

See [development tooling](docs/development.md) for dependency and platform requirements.
See [local deployment](docs/deployment.md) for build, run, health, logging, and recovery steps.
