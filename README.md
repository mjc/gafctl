# Updraft

Updraft is a Rust BLE probe for the GAF Wi-Fi Vent. It reads device state and sends threshold and timer commands without changing the firmware.

It discovers the fan, reads state, sets automatic thresholds, and starts or clears the timer. The HTTP API and Home Assistant integration are planned.

## Workspace

| Crate | Purpose |
| --- | --- |
| `updraft-protocol` | Typed commands, values, and frame parsing. |
| `updraft-bluetooth` | BLE discovery, connection, and protocol transport. |
| `updraft-wifi` | Reserved for Wi-Fi transport. |
| `updraft` | CLI. The HTTP service is planned. |

Show the requested command, acknowledgement, controller readback, and running state separately. The fan flag reports controller state, not airflow. Mark stale or unreachable devices unavailable.

## GAF Wi-Fi Vent

The GAF Wi-Fi Vent app (`com.gaf.wifivent`) uses the same command family over BLE and Wi-Fi TCP. BLE service `00FF` and characteristic `FF01` carry commands and replies. The fan access point is `192.168.4.1`; setup connects to `GAFVent_XXXX`.

BLE reads return identity, mode, sensors, thresholds, and timer state. Threshold and timer writes return acknowledgements and readbacks. The Wi-Fi port and TLS configuration are unknown. Home-LAN setup is unconfirmed, and Wi-Fi control is not implemented. See [protocol findings](docs/protocol-findings.md).

Master Flow QuickConnect / Vent Control uses the GAF/Keen Home cloud API. It has not been tested with the GAF Wi-Fi Vent.

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

The probe reads state after a control command. Acknowledgements and readbacks report controller state; they do not measure airflow. Firmware update commands are not exposed.

## Protocol behavior

The protocol has five state queries and two controls. Queries run in sequence. Each snapshot entry keeps the response frame and its decoded value or payload error. Missing replies fail the query; unknown payloads stay in the snapshot. Control results keep the command, acknowledgement, and readback.

`Frame::parse` borrows a slice. `Frame::from_bytes` shares `Bytes` storage. `into_owned()` copies only when needed. The decoder parses complete frames in place and assembles fragments in `BytesMut`. BLE retains notification storage for the matching response.

## Heap tracking

The optional `heap-track` feature prints Rust allocation counts during a BLE probe:

```sh
cargo run --features heap-track -- probe ble
```

Use `--scan-only` to count discovery. The report includes allocations, zeroed allocations, reallocations, deallocations, and requested or released bytes. Startup and output formatting are outside the measurement. Native Bluetooth allocations are not counted.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`:

```sh
devenv allow
devenv shell
devenv tasks run check:all
```

`check:all` runs formatting, Clippy, nextest, and doctests. The workspace has protocol, Bluetooth, and CLI tests. The BLE probe has completed live state reads and control readbacks.

See [development tooling](docs/development.md) for dependency and platform requirements.
