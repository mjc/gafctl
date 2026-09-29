# Updraft

Updraft is a Rust proxy for GAF Master Flow powered attic vents. The target is the **GAF Wi-Fi Vent**, controlled by the GAF Wi-Fi Vent app (`com.gaf.wifivent`). The goal is to read fan state and control the fan from Home Assistant while keeping the GAF controller's stock firmware.

The repository contains a BLE diagnostic probe for the GAF Wi-Fi Vent. It discovers the fan, reads its state, and can set automatic temperature and humidity thresholds or a timer. The HTTP service and Home Assistant API are not implemented.

## Architecture

```text
GAF attic fan <-- verified Wi-Fi or Bluetooth protocol --> Updraft <-- local API --> Home Assistant adapter
```

Static analysis found that the GAF Wi-Fi Vent app uses the same command family over BLE GATT and Wi-Fi TCP. BLE uses service `00FF` and characteristic `FF01`; the fan access point uses `192.168.4.1`. The app setup guide directs the phone to join `GAFVent_XXXX`. The probe has read identity, mode, sensors, thresholds, and timer state over BLE. Threshold and timer writes have received successful acknowledgements and matching readbacks. See [protocol findings](docs/protocol-findings.md).

The Cargo workspace has four crates:

| Crate | Responsibility |
| --- | --- |
| `updraft-protocol` | Typed commands, readings, device capabilities, and verified message encoding/decoding. It has no device I/O, HTTP, or Home Assistant code. |
| `updraft-bluetooth` | Bluetooth discovery, connection lifecycle, and transfer of protocol messages. |
| `updraft-wifi` | Wi-Fi discovery, connection lifecycle, and transfer of protocol messages. |
| `updraft` | The running service: coordinates transports, polls and reconciles device state, validates controls, and exposes a local API for Home Assistant. |

A Home Assistant adapter will expose device state and controls as entities. The Rust service will provide a local HTTP/JSON API using Axum and Tokio; the adapter can be a separate Python integration.

Track these states independently:

- **Requested:** the command sent by Home Assistant.
- **Acknowledged:** a response from the controller, if the protocol provides one.
- **Read back:** the configuration or telemetry reported after the command.
- **Running:** reported directly only if the controller exposes it; otherwise clearly labeled as inferred.

Use acknowledgements and readbacks to track controller-reported settings. Mark stale or unreachable devices unavailable in Home Assistant.

## Planned functionality

- Discover and identify GAF Wi-Fi Vent fans.
- Read available temperature, humidity, operating mode, targets, and timer state.
- Control supported on/off, mode, temperature/humidity targets, and timer settings.
- Expose stable device identities, state freshness, availability, and errors to Home Assistant.
- Support more than one fan if the verified protocol permits it.

Device evidence will determine supported entities and value ranges. Firmware updates, firmware replacement, resets, and pairing changes are out of scope. Updraft will not replace the GAF Wi-Fi Vent firmware with ESPHome.

## Related GAF software

[GAFVentControl-HA](https://github.com/hitchin999/GAFVentControl-HA) is an MIT-licensed Home Assistant integration for the newer **Master Flow QuickConnect / Vent Control** generation (`com.gaf.quickconnectapp`). It uses a GAF/Keen Home cloud API and provides a useful reference for Home Assistant entities and control behavior. That cloud API has not been shown to support the GAF Wi-Fi Vent. QuickConnect support is outside the current target.

ESP32 and ESPHome fan projects are not verified for GAF hardware. Updraft will not replace the device firmware.

## Current implementation

`updraft-protocol` encodes five state queries and two ordinary controls, and incrementally parses complete response lines while retaining payload bytes unchanged. `updraft-bluetooth` scans for the GAF service, selects a peripheral, subscribes to the response characteristic, sends queries, and can set automatic thresholds or timer duration. Firmware update operations are not implemented. Identity output is redacted by default; `--show-identity` prints the raw response and may reveal a device identifier.

`Frame::parse` borrows raw wire slices, while `Frame::from_bytes` takes shared `Bytes` storage. Payload and complete wire bytes are available as slices. `into_owned()` copies a raw borrow when needed; frames backed by `Bytes` can be retained or cloned without copying their contents. The decoder takes transport buffers as `Bytes`, visits complete frame slices directly, and assembles fragments in `BytesMut` before freezing them into shared storage. Bluetooth moves each notification's byte vector into `Bytes` and retains only its first matching response, while checking every complete frame for errors before accepting it. A retained slice keeps its backing allocation alive until the last shared frame is dropped.

A query returns the selected device and a `DeviceSnapshot` containing identity, mode, sensors, automatic thresholds, and timer state. It reads these values sequentially. Each observation retains its response frame and either a decoded value or a payload error. Missing replies fail the query; unknown payloads remain available in the snapshot. Control results contain the request, acknowledgement, and typed readback. The protocol crate interprets them, and the CLI formats them.

Each `Request` carries its wire encoding, expected response identifier, and diagnostic name. Query output identifies the responding device. Scan-only and ambiguous results list candidate devices.

Run a scan without connecting or sending protocol commands:

```sh
cargo run -- probe ble --scan-only
```

If exactly one GAF peripheral is found, the read-only probe can run directly:

```sh
cargo run -- probe ble
```

If multiple fans are nearby, pass one ID printed by scan-only mode:

```sh
cargo run -- probe ble --device-id <peripheral-id>
```

To exercise the automatic-mode setting write and immediately read it back, provide temperature in tenths of a degree Fahrenheit followed by humidity in tenths of a percent. The currently observed settings are 1050 and 300:

```sh
cargo run -- probe ble --set-auto-thresholds-tenths 1050 300
```

This sends the normal fan-control command `ams` and checks the subsequent threshold response. The acknowledgement and readback report the controller settings. They do not measure airflow. Firmware update commands are unavailable.

To exercise timer mode, pass a duration in whole minutes. A one-minute timer may run the fan; the probe reads the remaining and original timer values immediately afterward:

```sh
cargo run -- probe ble --set-timer-minutes 1
```

The fan's Wi-Fi server port and TLS configuration remain unknown. The Wi-Fi client is not implemented.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`. From the repository root, run:

```sh
devenv allow
devenv shell
devenv tasks run check:all
```

Inside the environment, Cargo commands run directly. The checks cover formatting, Clippy, nextest, and doctests. Protocol framing and response decoding have integration tests. The Bluetooth probe has also completed live state reads and control readbacks.

Build with `heap-track` to print Rust global-allocator counts around one BLE probe:

```sh
cargo run --features heap-track -- probe ble
```

Use `--scan-only` to measure discovery without connecting. The report counts successful Rust allocator calls across the workspace and dependencies during `probe()`, including allocation, zeroed allocation, reallocation, and deallocation events and their requested byte totals. Runtime startup and output formatting are outside the measured interval; allocations made directly by native Bluetooth libraries are not counted.

See [development tooling and dependencies](docs/development.md) for crate choices, commands, and platform requirements.

No account credentials, device secrets, private keys, or unredacted traffic captures belong in this repository.
