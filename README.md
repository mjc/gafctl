# Updraft

Updraft is a Rust proxy for GAF Master Flow powered attic vents. The first target is the older **GAF Wi-Fi Vent** generation controlled by the `com.gaf.wifivent` app. The goal is to read fan state and control the fan from Home Assistant while keeping the GAF controller's stock firmware.

The repository contains a BLE diagnostic probe for the legacy fan. It can discover a fan, query state, and optionally set automatic temperature/humidity thresholds or a timer. The service and Home Assistant API are still scaffolding.

## How it will work

```text
GAF attic fan <-- verified Wi-Fi or Bluetooth protocol --> Updraft <-- local API --> Home Assistant adapter
```

Static reverse engineering found the legacy app uses the same command family over BLE GATT and a Wi-Fi TCP connection. BLE uses service `00FF` and characteristic `FF01`; the app's Wi-Fi host is `192.168.4.1`. The installed app's setup guide joins the fan's `GAFVent_XXXX` access point. The probe has received identity, mode, sensor, threshold, and timer replies over BLE without joining that access point. Automatic-threshold and timer writes have both received successful acknowledgements and matching readbacks. The capture and remaining uncertainties are documented in [protocol findings](docs/protocol-findings.md).

The Cargo workspace has four crates:

| Crate | Responsibility |
| --- | --- |
| `updraft-protocol` | Typed commands, readings, device capabilities, and verified message encoding/decoding. It has no device I/O, HTTP, or Home Assistant code. |
| `updraft-bluetooth` | Bluetooth discovery, connection lifecycle, and transfer of protocol messages. |
| `updraft-wifi` | Wi-Fi discovery, connection lifecycle, and transfer of protocol messages. |
| `updraft` | The running service: coordinates transports, polls and reconciles device state, validates controls, and exposes a local API for Home Assistant. |

A small Home Assistant adapter will turn the local HTTP/JSON API's device state and supported controls into entities. Axum will serve the API, with Tokio handling asynchronous work. The adapter may be delivered separately because Home Assistant integrations run in Python. The Rust service will own protocol and device-state semantics.

The service will keep these states separate:

- **Requested:** the command sent by Home Assistant.
- **Acknowledged:** a response from the controller, if the protocol provides one.
- **Read back:** the configuration or telemetry reported after the command.
- **Running:** reported directly only if the controller exposes it; otherwise clearly labeled as inferred.

An accepted command will not be treated as proof that the fan changed state. Stale or unreachable devices should become unavailable in Home Assistant.

## Initial feature target

- Discover and identify supported legacy GAF fans.
- Read available temperature, humidity, operating mode, targets, and timer state.
- Control supported on/off, mode, temperature/humidity targets, and timer settings.
- Expose stable device identities, state freshness, availability, and errors to Home Assistant.
- Support more than one fan if the verified protocol permits it.

The exact entity set and value ranges will come from device evidence. Firmware updates, firmware replacement, resets, and pairing changes are outside the project scope. Updraft will not flash the user's equipment with ESPHome.

## Related GAF software

[GAFVentControl-HA](https://github.com/hitchin999/GAFVentControl-HA) is an MIT-licensed Home Assistant integration for the newer **Master Flow QuickConnect / Vent Control** generation (`com.gaf.quickconnectapp`). It uses a GAF/Keen Home cloud API and provides a useful reference for Home Assistant entities and control behavior. That API has not been shown to work with the older Wi-Fi Vent app or fan. Updraft's first backend is for the older device; support for QuickConnect would be separate work.

Open ESP32 and ESPHome fan projects are useful design references. They are not assumed to be compatible with GAF hardware, and replacing device firmware is not part of Updraft.

## Development status

`updraft-protocol` encodes five state queries and two ordinary controls, and incrementally parses complete response lines while retaining payload bytes unchanged. `updraft-bluetooth` scans for the GAF service, selects a peripheral, subscribes to the response characteristic, sends queries, and can set automatic thresholds or timer duration. Firmware update operations are not implemented. Identity output is redacted by default; `--show-identity` prints the raw response and may reveal a device identifier.

`Frame::parse` borrows raw wire slices, while `Frame::from_bytes` takes shared `Bytes` storage. Payload and complete wire bytes are available as slices. `into_owned()` copies a raw borrow when needed; frames backed by `Bytes` can be retained or cloned without copying their contents. The decoder takes transport buffers as `Bytes`, visits complete frame slices directly, and assembles fragments in `BytesMut` before freezing them into shared storage. Bluetooth moves each notification's byte vector into `Bytes` and retains only its first matching response, while checking every complete frame for errors before accepting it. A retained slice keeps its backing allocation alive until the last shared frame is dropped.

A successful query identifies the selected device and returns a `DeviceSnapshot` with identity, mode, sensors, automatic thresholds, and timer observations. These are five sequential reads, not an atomic sample. Each observation retains its original frame alongside either its decoded value or a payload error, so unfamiliar device data remains available for inspection. Missing transport replies fail the query; unfamiliar payloads remain in the completed snapshot. Optional control results keep the requested command, acknowledgement, and typed readback comparison together. The protocol crate interprets those results, and the CLI formats them.

Command exchange takes a `Request` that supplies its wire encoding, expected response identifier, and diagnostic operation name together. Successful query output identifies the device that answered; scan-only and ambiguous results retain their candidate lists.

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

This sends the normal fan-control command `ams` and checks the subsequent threshold response. The acknowledgement and configuration readback do not prove physical airflow. Firmware update commands are not exposed.

To exercise timer mode, pass a duration in whole minutes. A one-minute timer may run the fan; the probe reads the remaining and original timer values immediately afterward:

```sh
cargo run -- probe ble --set-timer-minutes 1
```

The Wi-Fi server port and TLS trust setup remain unknown, so the Wi-Fi client is not implemented yet. If Wi-Fi investigation is still needed, the target can run from one of the Linux boxes whose Wi-Fi is available for joining the fan AP; that is separate from the working BLE path.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`. From the repository root, run:

```sh
devenv allow
devenv shell
devenv tasks run check:all
```

Inside the environment, Cargo commands run directly. The checks cover formatting, Clippy, nextest, and doctests. Protocol framing and incremental response decoding have focused integration tests. The Bluetooth probe has passed a live read/write/readback cycle against the nearby vent; that device evidence is separate from automated tests.

See [development tooling and dependencies](docs/development.md) for the crate choices, individual commands, platform requirements, and dependencies to consider when the device protocol is known.

No account credentials, device secrets, private keys, or unredacted traffic captures belong in this repository.
