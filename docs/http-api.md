# HTTP API

Updraft exposes device discovery, state, and controls at `/api/v2`. There is no v1 HTTP API.

## Discovery

`GET /api/v2/devices` returns every registered device with its stable local ID, backend, capabilities, and selected state and command sources:

```json
{
  "devices": [
    {
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

The response contains no account or provider identifiers. Clients use the returned local ID in state and control paths. Refresh discovery after device inventory changes; an unknown or removed ID returns `404`.

## State

`GET /api/v2/devices/{local-id}/state` returns normalized optional measurements, backend settings, diagnostics, and timestamps describing when the state was fetched and observed:

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
    "diagnostics": {"firmware_version": "3.0.0"},
    "provenance": {
      "backend": "legacy_ble",
      "fetched_at_unix_ms": 2000,
      "observed_at_unix_ms": 1234
    }
  }
}
```

Unknown measurements stay `null`. `available` is true only while a usable state snapshot is fresh. `inventory_status` is `unknown`, `present`, `missing`, or `unavailable`; it describes device inventory, not motor operation. `estimated_running` is an estimate and may be null. The controller's fan flag is not proof of airflow.

## Controls

`POST /api/v2/devices/{local-id}/control` accepts a strict tagged command, unique request ID, and Unix timestamp in milliseconds:

```json
{
  "request_id": "ha-command-123",
  "issued_at_unix_ms": 2000,
  "command": {"kind": "legacy_preset", "preset": "timer_clear"}
}
```

Legacy preset values are `automatic105_f30_percent`, `automatic105_1_f30_1_percent`, `timer_clear`, and `timer_one_minute`. QuickConnect command shapes are:

```json
{"kind":"quick_connect_mode","mode":"automatic"}
{"kind":"quick_connect_targets","temperature_f":110,"humidity_percent":40}
{"kind":"quick_connect_timer_duration","minutes":90}
```

Each device must advertise the matching capability before a request reaches its backend. QuickConnect cloud controls remain disabled by default and require configured account credentials plus the explicit write gate; unsupported controls return `422`. If cloud-write capabilities are enabled without an available control service, the API returns `503`.

Requests older than 30 seconds or more than five seconds in the future are rejected. Reusing a request ID with the same device and command returns the cached result; reusing it for a different command returns `request_id_reused`. Replay results are retained in a bounded in-memory cache.

Control responses correlate with `request_id`. Statuses are `confirmed`, `unconfirmed` (legacy BLE control did not verify), `rejected`, `submitted_unconfirmed`, `readback_mismatch`, `readback_unavailable`, `unsupported_command`, `stale_request`, `request_id_reused`, `unknown_device`, `device_unavailable`, `backend_unavailable`, and `control_failed`. A command is confirmed only after backend acknowledgement and matching readback. Unsupported capability, stale timestamps, request-ID conflicts, unknown fields, and invalid typed commands return `422`; malformed JSON syntax returns `400`; unknown IDs return `404`; an unavailable cloud control service returns `503`; backend outcomes without confirmation return `502`. A worker failure returns `500` with `control_failed`; a retry with the same request ID is rejected instead of dispatching another write.

The API has no built-in authentication. It binds to loopback by default. Remote access requires `--allow-remote` and an authenticated, network-protected deployment.
