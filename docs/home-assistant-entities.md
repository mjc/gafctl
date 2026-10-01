# Home Assistant transports

Updraft supports HTTP and MQTT for multiple registered devices. Each transport exposes the same per-device state and capability-based command contract. Backends report their own confirmation outcome.

## Choose one entity source

Explicit MQTT discovery publishes the configured BLE entities. QuickConnect discovery follows each device's independently selected state and command sources; those sources default to HTTP.

The Updraft Home Assistant integration creates one config entry per selected device. Add Updraft again to add another device. State entities follow each device's `state_source`; controls follow its `command_source`. HTTP only creates entities for the side it owns, so mixed ownership exposes only the HTTP-owned state or controls and does not duplicate MQTT entities. Existing legacy entries keep their unique IDs during migration, and new entries use the local device ID shared with MQTT discovery.

MQTT state publishing can run beside the HTTP API without discovery. MQTT discovery is optional and follows the selected MQTT source. The legacy BLE topics remain aliases routed through the same control owner, so they do not create another device or submit commands twice.

## State and availability

Updraft reports temperature, humidity, controller mode and fan flag, thresholds, timer state, freshness, and query errors when those values are available from the controller.

The fan flag is a controller report. It does not confirm physical operation or airflow. Updraft does not expose a standalone on/off or manual-mode control unless the protocol can verify it.

HTTP entity availability follows the age of the last complete device snapshot. `/health` reports whether the API process responds; read the device state endpoint to check BLE availability.

MQTT publishes retained state and per-device availability under `updraft/{local-id}/`. The process last will is `updraft/availability`; each device's availability is updated from that device's own state. An unavailable cloud account or device does not mark other devices unavailable. On reconnect, the client republishes current states and enabled discovery data. Broker connectivity and device-state freshness remain separate signals.

## Controls

HTTP and per-device MQTT use the same typed commands advertised by each device's capabilities. The legacy BLE topic remains an alias for its preset commands. Unknown fields, unsupported commands, stale requests, future-dated requests, and retained MQTT requests are rejected before device access. MQTT requests are size-limited and use a bounded queue.

Controls share a per-device transaction lock with state polling. Updraft does not update Home Assistant optimistically. A missing MQTT result does not prove that the device rejected a command; inspect its current state before retrying.

MQTT results include a request ID and are not retained. Updraft caches the 64 most recent requests per local device ID. While a result is cached, repeating its ID with the same complete typed command returns that result; reusing it for a different command is rejected. An evicted ID can execute again if its timestamp is still fresh. The cache does not survive a process restart.

The MQTT preset select resets to unknown when the current settings do not match a supported preset. An expired one-minute timer is not reported as an active one-minute preset.

For QuickConnect over HTTP, Home Assistant exposes current temperature and humidity as measurements, a mode select, and target temperature, target humidity, and configured timer duration numbers when those commands are advertised. The number limits are 90–120 °F by 1 °F, 30–80% by 1%, and 30–360 minutes by 30 minutes. Configured duration is not a remaining countdown. Running estimate is a diagnostic binary sensor marked with inferred provenance. Unknown or stale settings remain unavailable; signal strength and verification flags are not presented as connectivity proof.

## Privacy

Keep controller identifiers, broker addresses, credentials, and deployment settings in local configuration. Do not put them in entity names, diagnostics, logs, or public documentation.
