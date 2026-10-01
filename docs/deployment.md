# Deployment

Run Updraft on a Linux system. For the legacy controller, provide a BLE adapter that can reach it. Home Assistant must be able to reach the HTTP API or share an MQTT broker with Updraft.

## Build

```sh
devenv allow
devenv shell -- cargo build --release --locked
```

The executable is `target/release/updraft`.

## Configure

To enable the BLE backend, set `UPDRAFT_DEVICE_ID` or pass `--device-id` in local service configuration. Keep the peripheral identifier out of tracked files. Without a BLE identifier the service starts with an empty inventory. The v2 state and control routes return 404 for unregistered local IDs. See the [HTTP API](http-api.md) for request and response schemas.

Set `UPDRAFT_IDENTITY_STORE` to a private local path when persisting cloud account/provider identity mappings. Newly created identity files use owner-only permissions. This setting alone does not enable cloud authentication or polling. Cloud credentials and provider identifiers must remain in local configuration, not tracked files or public API payloads.

The HTTP API binds to loopback by default. Use `--allow-remote` only when the service is protected by appropriate network controls or an authenticated reverse proxy. The API has no built-in authentication.

The broker must support MQTT 5. Updraft preserves the publisher's retain flag on command subscriptions so it can reject retained commands before BLE access.

Set these variables to enable MQTT:

- `UPDRAFT_MQTT_HOST`
- `UPDRAFT_MQTT_PORT`
- `UPDRAFT_MQTT_USERNAME`
- `UPDRAFT_MQTT_PASSWORD`
- `UPDRAFT_MQTT_DISCOVERY` (optional; defaults to disabled)

Load the password from a local secret store or service-manager credential. Do not pass it on the command line or commit it.

Give Updraft and Home Assistant separate MQTT accounts with these permissions for this device:

| Account | Operation | Topics |
| --- | --- | --- |
| Updraft | Publish | `updraft/gaf_vent/state`, `updraft/gaf_vent/availability`, `updraft/gaf_vent/control/result` |
| Updraft | Publish discovery | `homeassistant/sensor/updraft/+/config`, `homeassistant/select/updraft/control/config` |
| Updraft | Subscribe | `updraft/gaf_vent/control/set` |
| Home Assistant | Publish | `updraft/gaf_vent/control/set` |
| Home Assistant | Subscribe | The state, availability, result, and discovery topics above |

Updraft's account must not publish commands. Avoid a publish grant for `updraft/gaf_vent/#`, which includes the command topic. Use broker authentication and transport encryption when the network is not trusted.

## Choose a Home Assistant source

The native HTTP integration is the default entity source. MQTT can publish state and availability without discovery. To use MQTT entities, enable discovery and remove the HTTP integration entry for the same device. Do not enable both entity sources at once.

HTTP v2 state includes availability, inventory status, optional measurements and settings, diagnostics, and state provenance. `/health` reports process health only; it does not confirm device availability.

MQTT publishes retained state and availability. The broker's last will marks Updraft offline after an unexpected disconnect. On reconnect, Updraft republishes discovery when enabled, availability, and the latest state.

## Controls

HTTP v2 accepts strict tagged device commands listed by each device's capabilities. MQTT accepts its existing fixed BLE presets. Neither transport accepts arbitrary unsupported values.

MQTT control requests use QoS 1 and must be non-retained JSON with a request ID, a Unix timestamp in milliseconds, and a supported preset. Updraft rejects malformed, stale, future-dated, retained, and unsupported requests before BLE access. The request queue is bounded. Controls share the BLE transaction lock with polling and report success only after acknowledgement and matching device readback.

Generate a unique request ID and the current Unix timestamp in milliseconds for each new command. Requests older than 30 seconds or more than five seconds in the future are rejected. Reuse an ID only when retrying the same command; replay protection is limited to the cached results described in [Home Assistant transports](home-assistant-entities.md#controls).

MQTT results include the request ID, outcome, preset, message, and readback state when available. Results are non-retained. A missing result does not prove the controller rejected the command; check current state before retrying.

## Verify and recover

Run `devenv tasks run check:all` for formatting, Clippy, Rust tests, and doctests. Broker configuration checks and deployed-service checks are separate.

After deployment, verify the process health endpoint, fresh device state, broker availability, one selected Home Assistant entity source, reconnect behavior, and control acknowledgement/readback. Record deployment-specific values in a private operations log.

To upgrade, stop the service, install a build from the selected revision, and restart it. To roll back, restore the last known-good build. Home Assistant keeps HTTP integration configuration; MQTT discovery, state, and availability are stored by the broker.
