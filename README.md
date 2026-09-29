# Updraft

Updraft is a Rust proxy for GAF Master Flow powered attic vents. The first target is the older **GAF Wi-Fi Vent** generation controlled by the `com.gaf.wifivent` app. The goal is to read fan state and control the fan from Home Assistant while keeping the GAF controller's stock firmware.

This repository is an early scaffold. It does not yet connect to a fan or expose Home Assistant entities.

## How it will work

```text
GAF attic fan <-- verified Wi-Fi or Bluetooth protocol --> Updraft <-- local API --> Home Assistant adapter
```

The legacy app advertises direct Wi-Fi and Bluetooth control. Its exact transport, pairing method, message format, and behavior on the target fan still need to be measured. Updraft will implement only operations confirmed against the device.

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

The workspace has dependencies and development tooling in place, but no protocol or HTTP endpoint implementation. Work starts with redacted captures from the stock app and fan, followed by protocol fixtures and tests. Once the transport is known, the service API and Home Assistant adapter can be fixed to observed capabilities. Bluetooth and Wi-Fi may use different framing; their crates can keep transport-specific framing while sharing verified command and state types from `updraft-protocol`.

## Development

The repository uses devenv and pins Rust in `rust-toolchain.toml`. Review the environment files, then run from the repository root:

```sh
devenv allow
devenv shell
devenv tasks run check:all
```

Inside the environment, Cargo commands run directly. The checks cover formatting, Clippy, nextest, and doctests. There are no tests yet; the test task reports the empty suite explicitly.

See [development tooling and dependencies](docs/development.md) for the crate choices, individual commands, platform requirements, and dependencies to consider when the device protocol is known.

No account credentials, device secrets, private keys, or unredacted traffic captures belong in this repository.
