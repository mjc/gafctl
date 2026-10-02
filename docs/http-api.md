# HTTP API reference

The service provides device discovery, cached state, and controls under
`/api/v2`. There is no v1 HTTP API. Use the service's base URL without `/api/v2`
when configuring the CLI or Home Assistant.

The default listener is `127.0.0.1:8787`. Remote listeners require
`--allow-remote`. The API has no built-in authentication; see
[service setup](deployment.md#listen-for-home-assistant) for network access.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Check that the process responds |
| GET | `/api/v2/devices` | List configured devices and capabilities |
| GET | `/api/v2/devices/{id}/state` | Read a device's cached state |
| POST | `/api/v2/devices/{id}/refresh` | Read the device through its existing backend |
| POST | `/api/v2/devices/{id}/control` | Submit a supported command |
| PUT | `/api/v2/devices/{id}/sources` | Select HTTP or MQTT Home Assistant entities |

## Refresh

```sh
curl -X POST http://127.0.0.1:8787/api/v2/devices/configured/refresh
```

Overlapping refreshes for one device share one read. The read uses the same
backend owner and transaction lock as controls; it does not start a second BLE
connection owner. Cancelling an HTTP request leaves the shared worker running.
Periodic BLE reads use this operation too.

The response contains the state fields shown below plus `status`: `fresh`
(`200`), `failed` (`502`), or `superseded` (`409`). A failed refresh can contain
previous cached readings; its status still records that the new read failed.
A superseded read was displaced by another state or control generation and
was not published. Unknown devices return `404`, and unconfigured backends
return `503`. A worker that closes without a result returns `500` and can be
replaced by the next request.

The shared worker has a 270-second deadline, including time waiting for the
device lock. The Rust client and HA refresh button allow 300 seconds. HA checks
identity and HTTP ownership again after the read, then obtains the coordinator's
current state so a delayed response cannot replace newer readings.

## Discovery

```sh
curl http://127.0.0.1:8787/api/v2/devices
```

A Bluetooth device description looks like this. The capability list below is
abbreviated to one preset; the original-controller list includes four presets
and the three adjustable controls below:

```json
{
  "devices": [
    {
      "proxy_id": "bcfc37de-207e-4403-9d04-e11c53964acd",
      "id": "configured",
      "name": "GAF Wi-Fi Vent",
      "backend": "legacy_ble",
      "capabilities": {
        "read_state": true,
        "commands": [{"kind": "legacy_preset", "value": "timer_clear"}]
      },
      "state_source": "http",
      "command_source": "http"
    }
  ]
}
```

`proxy_id` identifies this Gafctl service and is persisted in its identity
store. `id` identifies a device within that service. The original ERV5SMT/EGV5SMT
controller is `configured`; QuickConnect devices have generated `qc-` IDs.
`legacy_ble` and `quick_connect` identify the two backends. Provider/account IDs
and Bluetooth peripheral IDs are not included.

Use the returned local device ID in subsequent paths. IDs accept 1–64 ASCII
letters, digits, underscores, or hyphens. An unknown ID returns `404`.

## State

```sh
curl http://127.0.0.1:8787/api/v2/devices/configured/state
```

Example original-controller state:

```json
{
  "id": "configured",
  "backend": "legacy_ble",
  "available": true,
  "inventory_status": "present",
  "last_error": null,
  "state": {
    "temperature_f": 98.6,
    "humidity_percent": 42.1,
    "settings": {
      "backend": "legacy_ble",
      "mode": "automatic",
      "controller_fan_on": false,
      "automatic_temperature_tenths_f": 1050,
      "automatic_humidity_tenths_percent": 300,
      "timer_remaining_minutes": 0,
      "timer_original_minutes": 0
    },
    "estimated_running": null,
    "diagnostics": {
      "firmware_version": "3.0.0",
      "signal_strength_raw": null,
      "verified_raw": null,
      "ota_in_progress": null
    },
    "provenance": {
      "backend": "legacy_ble",
      "fetched_at_unix_ms": 1790892000000,
      "observed_at_unix_ms": 1790892000000
    }
  }
}
```

This reads cached state and does not trigger a fan query. The service polls every
30 seconds; original-controller state becomes unavailable after 90 seconds
without a complete reading. Check `available` and timestamps rather than
`/health` for device freshness.

Unknown measurements are `null`. An unavailable device has `state: null` and
`available: false`. `inventory_status` is `unknown`, `present`, `missing`, or
`unavailable`. It describes inventory, not motor operation. `controller_fan_on`
is a controller report; `estimated_running` is an inference and may be `null`.

Nullable fields are included explicitly. Clients may accept additional fields,
but missing required fields or inconsistent values are errors.

## Controls

The CLI and Home Assistant create command envelopes for you. For your own client,
POST a JSON object containing:

| Field | Value |
| --- | --- |
| `request_id` | A new identifier, using the same syntax as device IDs |
| `issued_at_unix_ms` | Current Unix time in milliseconds |
| `command` | One of the command objects below |

Example body, with an illustrative timestamp that must be replaced before sending:

```json
{
  "request_id": "attic-timer-1",
  "issued_at_unix_ms": 1790892000000,
  "command": {"kind": "legacy_preset", "preset": "timer_clear"}
}
```

Original-controller presets are `automatic105_f30_percent`,
`automatic105_1_f30_1_percent`, `timer_clear`, and `timer_one_minute`. Their effects
are listed in the [CLI reference](cli.md#original-fan-controls).

Original-controller adjustable commands:

```json
{"kind":"legacy_automatic_temperature","temperature_f":110}
{"kind":"legacy_automatic_humidity","humidity_percent":40}
{"kind":"legacy_timer","minutes":60}
```

Temperature accepts 90–120 °F, humidity 30–80%, and timer 0–360 minutes,
all in whole-unit steps. Zero clears the timer. These limits come from the
original manufacturer Android app, including its timer picker and setter
encoding. They do not expose the app's manual or humidity-disable sentinels.
Changing either threshold selects automatic mode. The service reads current
thresholds while holding the device transaction, preserves the untouched raw
tenths field, checks request age again, then writes and verifies readback.
Missing or unsupported readback prevents the write. An existing 100% humidity
disable sentinel is preserved when changing temperature.

The original controller advertises `legacy_automatic_temperature`,
`legacy_automatic_humidity`, and `legacy_timer` capabilities. Broader values
within the app ranges still need owned-device readback acceptance; the
previously verified presets remain available.

QuickConnect commands:

```json
{"kind":"quick_connect_mode","mode":"automatic"}
```

```json
{"kind":"quick_connect_targets","temperature_f":110,"humidity_percent":40}
```

```json
{"kind":"quick_connect_automatic_temperature","temperature_f":110}
```

```json
{"kind":"quick_connect_automatic_humidity","humidity_percent":40}
```

```json
{"kind":"quick_connect_conditional_off","only_if_current":"automatic"}
```

Single-target commands preserve the other target from a fresh backend read under
the device transaction. Conditional off changes the mode only when the fresh
backend mode matches `only_if_current`; another known mode confirms without a
write. Unknown or conflicting mode prevents the write. HA number controls and
mode switches use these commands.


```json
{"kind":"quick_connect_timer_duration","minutes":90}
```

QuickConnect mode accepts `off`, `automatic`, `timer`, or `manual`. Targets are
integers: 90–120 °F and 30–80%. Duration accepts 30–360 minutes in 30-minute steps.
Cloud commands require account configuration and the opt-in write setting.
Each device must advertise the command before the service will execute it.

Commands reject unknown fields and invalid values. Requests older than 30 seconds
or more than five seconds ahead are rejected before device access. Polls and
controls are serialized per device.

### Confirmation and retries

Responses include the submitted `request_id` and a `status`. Treat only
`confirmed` as success: it requires a successful backend response and matching
readback. A controller reply or submitted cloud request alone is insufficient.

Each device caches 64 completed requests in memory. Repeating an ID with the
same command returns its cached result; changing the command returns
`request_id_reused`. Evicted IDs can execute again when the request is fresh.
The cache does not survive service restart.

At most eight distinct request IDs can be in flight per device. Repeating an
in-flight ID joins its existing execution. Excess requests return `busy` before
another worker is started. A failed worker records `control_failed` for replay
handling instead of silently allowing another write.

A client timeout does not stop the worker or prove that the write failed. Read
current state before deciding to retry.

| HTTP status | Command outcome |
| --- | --- |
| 200 | `confirmed` |
| 400 | Malformed JSON syntax |
| 404 | `unknown_device` |
| 422 | Invalid command/envelope, unsupported command, stale timestamp, or request ID reused with different command |
| 429 | `busy` |
| 500 | `control_failed` |
| 502 | Backend did not confirm the command |
| 503 | `backend_unavailable` |

Unconfirmed backend outcomes include `unconfirmed`, `submitted_unconfirmed`,
`readback_mismatch`, `readback_unavailable`, `rejected`, and `device_unavailable`.
Preserve the returned status and request ID for diagnostics.

## Home Assistant entity source

Choose one source for a device's state and controls. Defaults are HTTP. For MQTT,
configure a broker and enable MQTT discovery first, then request:

```sh
curl -X PUT http://127.0.0.1:8787/api/v2/devices/configured/sources \
  -H 'Content-Type: application/json' \
  -d '{"state_source":"mqtt","command_source":"mqtt"}'
```

Use both `http` values to switch back. The response is the updated device
description, and the selection is saved in the identity store. Split HTTP/MQTT
ownership is rejected with `422`. Selecting MQTT without a configured broker and
discovery returns `409`; unknown IDs return `404`. Persistence failures return
`500`. Source selection controls Home Assistant entities; it does not prevent an
administrative CLI or API client from submitting a supported command.

See [Home Assistant and MQTT](home-assistant-entities.md) for installation,
discovery, topics, and broker permissions.
