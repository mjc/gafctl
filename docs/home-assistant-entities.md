# Home Assistant transports

Updraft supports HTTP and MQTT. Both expose the same state and fixed control presets. They share the BLE poll lock and report control success only after device acknowledgement and matching readback.

## Choose one entity source

The native HTTP integration is the default. It owns Home Assistant setup, entity registration, polling, and errors.

MQTT state publishing can run beside the HTTP integration without discovery. MQTT discovery is optional. Enable it only when MQTT should own the entities, and remove the HTTP integration for the same device. Two entity sources can create duplicate Home Assistant entities.

## State and availability

Updraft reports temperature, humidity, controller mode and fan flag, thresholds, timer state, freshness, and query errors when those values are available from the controller.

The fan flag is a controller report. It does not confirm physical operation or airflow. Updraft does not expose a standalone on/off or manual-mode control unless the protocol can verify it.

HTTP entity availability follows the age of the last complete device snapshot. `/health` reports whether the API process responds; read the device state endpoint to check BLE availability.

MQTT publishes retained state and availability. Its last will marks the publisher offline after an unexpected disconnect. The client reconnects and republishes the latest state and, when enabled, discovery data. Broker connectivity and device-state freshness are separate signals.

## Controls

HTTP and MQTT use the same fixed presets. Unknown fields, unsupported presets, stale requests, future-dated requests, and retained MQTT requests are rejected before BLE access. MQTT requests are size-limited and use a bounded queue.

Controls share a transaction lock with polling. Updraft reports success only after an accepted device acknowledgement and matching state readback. It does not update Home Assistant optimistically. A missing MQTT result does not prove that the device rejected a command; inspect the current state before retrying.

MQTT results include a request ID and are not retained. Repeating an ID with the same preset returns the cached result during the current process lifetime. Reusing an ID for another preset is rejected. The replay cache does not survive a process restart.

## Privacy

Keep controller identifiers, broker addresses, credentials, and deployment settings in local configuration. Do not put them in entity names, diagnostics, logs, or public documentation.
