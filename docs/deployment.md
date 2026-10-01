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

The broker must support MQTT 5. Updraft preserves the publisher's retain flag on control subscriptions so retained commands are rejected before device access.

Set these variables to enable MQTT:

- `UPDRAFT_MQTT_HOST`
- `UPDRAFT_MQTT_PORT`
- `UPDRAFT_MQTT_USERNAME`
- `UPDRAFT_MQTT_PASSWORD`
- `UPDRAFT_MQTT_DISCOVERY` (optional; defaults to disabled)

Load the password from a local secret store or service-manager credential. Do not pass it on the command line or commit it.

Give Updraft and Home Assistant separate MQTT accounts. Grant only the publish and subscribe directions each account needs:

| Account | Operation | Topics |
| --- | --- | --- |
| Updraft | Publish | `updraft/availability`, `updraft/+/state`, `updraft/+/availability`, `updraft/+/control/result` |
| Updraft | Publish discovery | `homeassistant/+/+/+/config` |
| Updraft | Subscribe | `updraft/+/control/set` |
| Home Assistant | Publish | `updraft/+/control/set` |
| Home Assistant | Subscribe | `updraft/availability`, `updraft/+/state`, `updraft/+/availability`, `updraft/+/control/result`, `homeassistant/+/+/+/config` |

Updraft's account must not publish commands, and Home Assistant's account must not publish state, availability, results, or discovery. Avoid broad publish grants for `updraft/#`; they include both command and service-owned topics. Use broker authentication and transport encryption when the network is not trusted.

## Choose a Home Assistant source

`UPDRAFT_MQTT_DISCOVERY` enables discovery for the configured BLE device. QuickConnect discovery follows each device's state and command source; those sources default to HTTP and can be selected independently in the device model. MQTT can still publish state without discovery for manually configured consumers.

HTTP v2 state includes availability, inventory status, optional measurements and settings, diagnostics, and state provenance. `/health` reports process health only; it does not confirm device availability.

MQTT publishes retained state and per-device availability at `updraft/{local-id}/state` and `updraft/{local-id}/availability`. Process availability has its own retained last-will topic, `updraft/availability`; it does not replace any device's availability. On reconnect, Updraft republishes discovery when enabled and the latest state for every registered device. The old `updraft/gaf_vent/...` topics remain aliases for the configured BLE device.

## Controls

HTTP v2 and per-device MQTT accept the same strict tagged device commands listed by each device's capabilities. The legacy BLE topic remains an alias for its fixed presets. Neither transport accepts unsupported commands.

MQTT control requests use QoS 1 and must be non-retained JSON with a request ID, a Unix timestamp in milliseconds, and a typed command. Updraft rejects malformed, stale, future-dated, retained, and unsupported requests before device access. The request queue is bounded. Controls share the device transaction lock with state polling and report success only at the backend's confirmation level.

Generate a unique request ID and the current Unix timestamp in milliseconds for each new command. Requests older than 30 seconds or more than five seconds in the future are rejected. Reuse an ID only when retrying the same command; replay protection is limited to the cached results described in [Home Assistant transports](home-assistant-entities.md#controls).

Per-device MQTT results include the request ID and outcome and are non-retained. The legacy alias keeps its existing result shape. A missing result does not prove the device rejected the command; check current state before retrying.

## Verify and recover

Run `devenv tasks run check:all` for formatting, Clippy, Rust tests, and doctests. Broker configuration checks and deployed-service checks are separate.

After deployment, verify the process health endpoint, fresh device state, broker availability, one selected Home Assistant entity source, reconnect behavior, and control acknowledgement/readback. Record deployment-specific values in a private operations log.

To upgrade, stop the service, install a build from the selected revision, and restart it. To roll back, restore the last known-good build. Home Assistant keeps HTTP integration configuration; MQTT discovery, state, and availability are stored by the broker.
