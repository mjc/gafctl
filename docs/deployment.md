# Deployment

Run Updraft on a Linux system with a BLE adapter that can reach the configured controller. Home Assistant must be able to reach the HTTP API or share an MQTT broker with Updraft.

## Build

```sh
devenv allow
devenv shell -- cargo build --release --locked
```

The executable is `target/release/updraft`.

## Configure

Select the controller locally. Keep its identifier and deployment settings in local service configuration, not in tracked files.

The HTTP API binds to loopback by default. Use `--allow-remote` only when the service is protected by appropriate network controls or an authenticated reverse proxy. The API has no built-in authentication.

Set these variables to enable MQTT:

- `UPDRAFT_MQTT_HOST`
- `UPDRAFT_MQTT_PORT`
- `UPDRAFT_MQTT_USERNAME`
- `UPDRAFT_MQTT_PASSWORD`
- `UPDRAFT_MQTT_DISCOVERY` (optional; defaults to disabled)

Load the password from a local secret store or service-manager credential. Do not pass it on the command line or commit it.

Keep the MQTT account limited to the topics required for state, availability, results, commands, and discovery. Use broker authentication and transport encryption when the network is not trusted.

## Choose a Home Assistant source

The native HTTP integration is the default entity source. MQTT can publish state and availability without discovery. To use MQTT entities, enable discovery and remove the HTTP integration entry for the same device. Do not enable both entity sources at once.

HTTP state includes availability, freshness, observation time, query errors, and the latest confirmed values. `/health` reports process health only; it does not confirm BLE availability.

MQTT publishes retained state and availability. The broker's last will marks Updraft offline after an unexpected disconnect. On reconnect, Updraft republishes discovery when enabled, availability, and the latest state.

## Controls

HTTP and MQTT accept the same fixed control presets. They do not accept arbitrary threshold, timer, mode, or power values.

MQTT control requests use QoS 1 and must be non-retained JSON with a request ID, a Unix timestamp in milliseconds, and a supported preset. Updraft rejects malformed, stale, future-dated, retained, and unsupported requests before BLE access. The request queue is bounded. Controls share the BLE transaction lock with polling and report success only after acknowledgement and matching device readback.

MQTT results include the request ID, outcome, preset, message, and readback state when available. Results are non-retained. A missing result does not prove the controller rejected the command; check current state before retrying.

## Verify and recover

Run `devenv tasks run check:all` for formatting, Clippy, Rust tests, and doctests. Broker configuration checks and deployed-service checks are separate.

After deployment, verify the process health endpoint, fresh device state, broker availability, one selected Home Assistant entity source, reconnect behavior, and control acknowledgement/readback. Record deployment-specific values in a private operations log.

To upgrade, stop the service, install a build from the selected revision, and restart it. To roll back, restore the last known-good build. Home Assistant keeps HTTP integration configuration; MQTT discovery, state, and availability are stored by the broker.
