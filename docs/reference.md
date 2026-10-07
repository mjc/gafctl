# Reference

- [Command line](#command-line)
- [Home Assistant behavior](#home-assistant-behavior)
- [HTTP API](#http-api)
- [MQTT](#mqtt)
- [Bluetooth protocol](#bluetooth-protocol)
- [QuickConnect API](#quickconnect-api)
- [Model sources](#model-sources)

## Command line

Run `gafctl --help` or a subcommand's `--help` for arguments and presets.
`gafctl server` launches `gafctl-server` beside the CLI, falling back to `PATH`.
It forwards arguments; on Unix it replaces the CLI process and receives signals directly.

Service commands use HTTP. The URL comes from `--server`, then
`GAFCTL_SERVER_URL`, then `http://127.0.0.1:8787`. HTTPS and reverse-proxy path
prefixes work; certificates are verified. Credentials, queries, and fragments
in the base URL are rejected. Redirects, proxies, and automatic retries are disabled.

The connection timeout is five seconds, reads default to ten seconds, and
controls to 300 seconds. `--timeout-seconds` overrides read/control deadlines.
Controls generate a UUID and timestamp; `--request-id` overrides the UUID.
IDs contain 1–64 ASCII letters, digits, underscores, or hyphens.

`--format json` writes one JSON value and a newline to stdout; logs and errors
go to stderr. Exit codes are 0 for a completed read/discovery or confirmed
control, 1 for execution or confirmation errors, and 2 for invalid arguments.
An empty scan or unavailable service state is a successful read. Direct BLE
reads can return partial output with exit 1. Disconnect failures remain in the
output without invalidating a complete snapshot or confirmed control.

`gafctl ble scan` reads advertisements without connecting. `ble state` can
select a single unambiguous fan; `ble control` requires `--device-id`. Raw
identity bytes are hidden unless `--show-identity` is supplied; they may contain
a private identifier. Direct operations disconnect before returning.
Use `gafctl-server probe ble --help` for diagnostics; raw `u16` controls have
not been tested across their full range.

Generate completions with `gafctl completions bash`, `zsh`, `fish`, `elvish`, or
`powershell`. The Rust client is `gafctl-client::Client`; shared models live in
`gafctl-api`. `prepare_control` checks capabilities, and consuming `submit`
sends the prepared command once.

## Home Assistant behavior

The original controller provides temperature, humidity, reported Fan state,
firmware, raw controller mode, automatic thresholds, Timer remaining, and Last
timer duration. Controls are described in the [README](../README.md#add-it-to-home-assistant).
Fractional threshold readings are preserved; new settings use whole units.
Fan reports the controller flag, not measured airflow. A stopped timer appears
as Off in the Mode control; its raw controller mode remains timer.

QuickConnect exposes temperature, humidity, and available diagnostics, including
raw signal strength, verification, OTA status, humidity monitoring, and mode
flags. Missing or conflicting mode flags show unknown. Running is estimated;
firmware updates are unsupported. With cloud writes enabled, controls include
Off/Automatic/Timer/Manual modes, temperature and humidity targets, Set timer
(30–360 minutes in 30-minute steps), mode switches, and All off. Saving duration
leaves the mode unchanged; turning off an inactive switch preserves the active mode.

### Timed runs

Starting a positive original-controller timer saves the preceding mode and
thresholds. After expiry, a fresh matching reading restores Automatic, keeps
Off off, or resumes only the estimated time left on a preceding external timer.
Extending a gafctl timer preserves its original return mode. Editing the saved
duration does not change an active countdown.

Mode or target changes cancel pending restoration. Manufacturer-app changes
cancel it when mode, thresholds, original duration, or countdown no longer
match. An external stop in the final minute can look like normal expiry.

Saved duration and pending return persist in the identity store. Restoration
requires the service and Bluetooth; the controller alone ends its timer off.
The saved return is consumed before writing and is never replayed after an
uncertain write. A crash between those steps can leave the fan off. On failure,
inspect `last_error` and select a mode. Stale or unavailable readings never
trigger restoration.

### Availability

Bluetooth polls run three seconds after a completed read on a retained
connection; failed polls back off with jitter up to sixty seconds. QuickConnect
polls every thirty seconds independently. Failed Bluetooth reads immediately
invalidate current state. Readings expire ninety seconds after the earliest
observation/fetch timestamp; later replies and cached reads do not renew them.

The HTTP integration polls cached state at the same three/thirty-second
intervals. During an API connection failure, timeout, or HTTP 5xx, it keeps the
last validated reading until its original expiry, marks `freshness: cached`,
and sets `api_error`. Controls require a live read. Explicit device failures,
identity/ownership changes, or invalid responses invalidate readings immediately.
`last_error` reports device failures or expiry; a successful read clears `api_error`.

HA allows one second of clock difference, never more than ninety seconds until
local expiry; larger future timestamps are rejected. Its private cache survives
orderly restarts/reloads only for the same API address and device identity and
within the original deadline. Controls require a live read after startup.

## HTTP API

The default listener is `127.0.0.1:8787`; remote listeners require `--allow-remote`.
There is no authentication. See [installation](installation.md) for network access.
Configure clients with the base URL, without `/api/v2`.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Process health; does not check the fan |
| GET | `/api/v2/devices` | Inventory and capabilities |
| GET | `/api/v2/devices/{id}/state` | Cached state |
| POST | `/api/v2/devices/{id}/refresh` | New backend reading |
| POST | `/api/v2/devices/{id}/control` | Command |
| PUT | `/api/v2/devices/{id}/sources` | HTTP/MQTT HA ownership |

### Inventory and state

Inventory returns a `devices` array. Each descriptor includes persistent
`proxy_id`, service-local `id`, `name`, `backend`, `capabilities`, `state_source`,
and `command_source`. Backends are `legacy_ble` and `quick_connect`. The single
configured Bluetooth fan has ID `configured`; cloud devices receive `qc-` IDs.
Provider/account and Bluetooth peripheral IDs are omitted. Use the returned ID
in API paths. An unknown ID returns `404`.

An original-controller state response:

```json
{
  "id": "configured",
  "backend": "legacy_ble",
  "timer_duration_minutes": 360,
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

`timer_duration_minutes` is the saved preference, present even while unavailable;
it is `null` for QuickConnect. Raw device timer values are in `state.settings`.
Unknown measurements are `null`. Unavailable devices have `state: null` and
`available: false`. Inventory status is `unknown`, `present`, `missing`, or
`unavailable`. Nullable fields are included. Clients may accept additional
fields; missing required fields or inconsistent values are errors. The CLI
limits HTTP response bodies, including errors, to two MiB.

### Refresh

Overlapping refreshes share a per-device worker and the control transaction
lock. Cancelling a request leaves the worker running. Periodic BLE polls use
this worker; the HA HTTP integration only reads cached state.

The response adds `status`: `fresh` (`200`), `failed` (`502`), or `superseded`
(`409`, replaced by newer state/control). Bluetooth failure clears current
readings; cloud failure can retain readings until expiry. Unknown devices
return `404`, unconfigured backends `503`, and a worker closing without a result
`500`; the next request can replace it. The worker deadline is 270 seconds,
including lock waits; the Rust client allows 300 seconds.

### Controls

The CLI and HA create envelopes automatically. Custom clients send:

```json
{
  "request_id": "attic-timer-1",
  "issued_at_unix_ms": 1790892000000,
  "command": {"kind": "legacy_timer", "minutes": 1}
}
```

Replace the timestamp with current Unix milliseconds and use a new request ID
for each new command. Request IDs follow the device-ID syntax. Unknown fields,
invalid values, requests older than 30 seconds, and timestamps more than five
seconds ahead are rejected before device access. Commands must be advertised
in the device's capabilities. Polls and controls serialize per device.

| Command kind | Fields / values |
| --- | --- |
| `legacy_mode` | `mode`: `automatic`, `timer`, `off` |
| `legacy_automatic_temperature` | `temperature_f`: 90–120 °F, integer |
| `legacy_automatic_humidity` | `humidity_percent`: 30–80%, integer |
| `legacy_timer_duration` | `minutes`: 0–360, integer; save only |
| `legacy_timer` | `minutes`: 0–360, integer; start immediately, zero stops |
| `legacy_preset` | `preset`: values below |
| `quick_connect_mode` | `mode`: `off`, `automatic`, `timer`, `manual` |
| `quick_connect_targets` | Both `temperature_f`: 90–120 °F and `humidity_percent`: 30–80%, integers |
| `quick_connect_automatic_temperature` | `temperature_f`: 90–120 °F, integer |
| `quick_connect_automatic_humidity` | `humidity_percent`: 30–80%, integer |
| `quick_connect_conditional_off` | `only_if_current`: mode to stop |
| `quick_connect_timer_duration` | `minutes`: 30–360 in 30-minute steps; save only |

Presets are `automatic105_f30_percent` (105.0 °F/30.0%),
`automatic105_1_f30_1_percent` (105.1 °F/30.1%), `timer_clear` (Off), and
`timer_one_minute` (one minute with previous-mode restoration). CLI spellings
are `automatic-105-f-30-percent`, `automatic-105-1-f-30-1-percent`,
`timer-clear`, and `timer-one-minute`.

Original threshold changes select Automatic and preserve the untouched raw
tenths value, including a 100% humidity-disable sentinel. Automatic reapplies
both current thresholds. Missing/unsupported settings prevent a write. Timer
starts the saved duration or selects Automatic at zero; Off writes a zero-minute
timer. Saving duration needs no Bluetooth access. See [timed runs](#timed-runs).
The ranges come from the manufacturer app; endpoints have not all been tested.

Cloud writes require opt-in. Single-target changes preserve the other target
under the transaction lock. Conditional off preserves another known active
mode; unknown/conflicting mode prevents a write. All device controls require
acknowledgement and matching readback. Saving a local preference requires a
successful store write instead.

Responses include `request_id` and `status`; only `confirmed` is success.
Unconfirmed outcomes include `unconfirmed`, `submitted_unconfirmed`,
`readback_mismatch`, `readback_unavailable`, `rejected`, and `device_unavailable`.

| HTTP status | Outcome |
| --- | --- |
| 200 | `confirmed` |
| 400 | Malformed JSON |
| 404 | `unknown_device` |
| 422 | Invalid/unsupported/stale request or ID reused with another command |
| 429 | `busy` |
| 500 | `control_failed` |
| 502 | Backend did not confirm |
| 503 | `backend_unavailable` |

Each device caches 64 completed requests. An identical cached ID/command
returns the result; different content returns `request_id_reused`. Eviction or
restart allows an ID to execute again if fresh. At most eight distinct requests
run per device; duplicates join their worker, excess requests return `busy`.
A failed worker records `control_failed`. Timeout does not cancel the worker or
prove failure. Read current state before retrying; sent writes are never
automatically replayed.

### Entity ownership

Devices default to HTTP. PUT `{"state_source":"mqtt","command_source":"mqtt"}`
to `/api/v2/devices/{id}/sources` to switch, or both `http` values to switch back.
The response is the updated descriptor; ownership persists in the identity store.
Split ownership returns `422`, MQTT without broker/discovery `409`, unknown ID
`404`, and persistence failure `500`. Administrative commands remain available
on either transport. Setup and handoff are in [installation](installation.md#mqtt).

## MQTT

Use [MQTT setup](installation.md#mqtt) first. State and availability are retained;
commands and results are not. Read `proxy_id` and `id` from HTTP inventory.

| Topic | Direction / contents |
| --- | --- |
| `gafctl/{proxy_id}/availability` | Process availability, including last will |
| `gafctl/{proxy_id}/{id}/availability` | Device availability |
| `gafctl/{proxy_id}/{id}/state` | Device state |
| `gafctl/{proxy_id}/{id}/control/set` | Command envelope from [Controls](#controls); QoS 1, **retain disabled** |
| `gafctl/{proxy_id}/{id}/control/result` | Correlated result |
| `gafctl/{proxy_id}/{id}/refresh/set` | Refresh envelope; QoS 1, **retain disabled** |
| `gafctl/{proxy_id}/{id}/refresh/result` | Correlated `fresh`, `failed`, or `superseded` result |
| `homeassistant/device/gafctl/{identifier}/config` | Retained HA discovery; identifier includes both IDs |

Refresh accepts only `request_id` and `issued_at_unix_ms`; control-shaped or
malformed payloads are discarded. Retained/stale requests return correlated
rejections without fan access. Accepted reads share HTTP's refresh worker.
Controls reject retained, stale, malformed, or unsupported requests before access.

MQTT waits 300 seconds. A control wait expiring or worker closing publishes
`outcome_unknown` if the broker is available. Result publication has its own
30-second bound. A backend operation can complete after its response is lost.
HA reports the request ID and does not automatically retry uncertain controls.
Reconnect republishes state and discovery; client IDs include the proxy UUID.

Use separate broker accounts. Gafctl publishes state, availability, results, and
`homeassistant/+/gafctl/+/config`; it subscribes to `control/set` and `refresh/set`.
HA needs the reverse permissions, including discovery subscription. Scope device
topics to `gafctl/+/+/...` and process availability to `gafctl/+/availability`.
Do not grant command clients publish access to all of `gafctl/#`.

Device discovery groups entities under `components`. Migration from individual
topics preserves unique IDs: nonretained migration messages precede the new
configuration, and old retained topics clear only after broker acceptance.
Before acceptance, failure preserves the old configurations; after acceptance,
cleanup retries on state updates/reconnect. HA may temporarily unload entities.
If replacement keeps failing, restart HA or reconnect MQTT to replay the old
configurations. Unavailable capabilities receive platform-only removal entries,
including after restart; switching to HTTP clears retained device discovery.

## Bluetooth protocol

Original ERV5SMT/EGV5SMT controllers require firmware 3.0.0. The BLE service is
`00FF`, characteristic `FF01` (read/write/notify). Enable notifications before
requests. Frames are lowercase ASCII `#<command><payload>\n`, reassembled through
LF, limited by gafctl to 1024 bytes; the device limit is unknown. Setters encode
uppercase zero-padded hex `%04X`; replies use lowercase `%04x` or decimal `%1d`.

| Request | Response | Meaning |
| --- | --- | --- |
| `#idg\n` | `#idr%s%s\n` | Identity: first six digits are firmware (`030000` = 3.0.0); redact suffix |
| `#dmg\n` | `#dmr<mode><fan>\n` | Mode `a` automatic, `t` timer, `o` OTA; fan flag `f`/`n` |
| `#sdg\n` | `#sdr%04x%04x\n` | Temperature °F and humidity %, in tenths |
| `#atg\n` | `#atr%04x%04x\n` | Automatic thresholds, in tenths |
| `#ttg\n` | `#ttr%04x%04x\n` | Remaining and original timer minutes; app displays seconds |
| `#ams%04X%04X\n` | `#amr%1d\n` | Automatic thresholds; firmware version 2 or later |
| `#tms%04X\n` | `#tmr%1d\n` | Timer minutes; firmware version 2 or later; app rounds seconds |

`amr0`/`tmr0` acknowledge setters. Automatic confirmation also requires automatic
mode; positive timer confirmation requires timer mode and fan on, clear requires
fan off. Missing, malformed, mismatched, or expired readback is unconfirmed.
The identity has no roof/gable model field. `estimated_running` stays null;
`controller_fan_on` is the raw flag. Airflow is unmeasured.

### Connection lifecycle

Android reference: `com.gafs.android` 2.0.0, version code 3, APK SHA-256
`0dfd606669c7bec6d0d920d38238710a305fb2815d47a5a6b07911caf761eeaa`.
Android scans unfiltered for five seconds (`GAFVent_` names), calls `connectGatt`
with `autoConnect=false`, subscribes via CCCD `2902`, reads FF01, and initializes
identity → sensors → thresholds → mode, then timer only if running in timer mode.
It retains GATT and requests sensors every three seconds; teardown disconnects,
closes GATT, and clears the handle.

Gafctl retains one connection for serialized polls and controls. It subscribes,
awaits the initial read, discards queued startup data, then requests
`idg` → `sdg` → `atg` → `dmg`. Each poll reads sensors, thresholds, mode, and
timer, even when off. It does not copy Android's initial write of saved thresholds.
Controls retain acknowledgement and read settings, mode, sensors, and timer.
Queued replies cannot acknowledge a new request; an incomplete queued frame
must finish within the response deadline before another command is sent. The
control deadline is rechecked after draining. Responses correlate by command ID;
the protocol has no sequence number. Malformed settings remain diagnostic only.

Failed/interrupted exchanges release the session before reconnecting. Cleanup
drops stream/session and disconnects the tracked native peripheral, even during
an incomplete connection attempt; failed cleanup remains pending for retry.
Shutdown closes admission, interrupts the exchange, and retries cleanup within
its deadline. CCCD is not explicitly disabled, matching Android teardown.
btleplug delegates to BlueZ/CoreBluetooth. BlueZ scan transport is `Auto` versus
Android's LE scan. Gafctl does not request pairing or change MTU/PHY,
reset the controller, or send firmware commands. The APK proves callback order
and lifetime, not on-air timing or the cause of a fan disappearing.

Scan defaults to five seconds; GATT setup, write, and response waits to three
seconds. Adapter setup/scan/connect/cleanup allow at least 40 seconds for OS calls.
Transient connection retries use jittered backoff of 0.5–1, 1–2, 2–4, and
4–8 seconds, at most five attempts, with no new attempt after 20 seconds.
Authentication, protocol, and cleanup errors stop recovery. BLE shutdown allows
85 seconds; HTTP/MQTT drain separately for five seconds. Service managers allow
100 seconds before termination. An interrupted sent control stays unconfirmed.

### Captured device evidence

The iOS GAF Wi-Fi Vent 2.1 (`com.gaf.wifivent`) and firmware
`GAFVent_030000.bin` provided separate protocol evidence. The firmware is
1,039,680 bytes, SHA-256
`badf3a57571fd66ca3df76eeaeb988549334ceff72b5c9f7e58082732089f260`,
ESP-IDF `v3.1-dev-1193-g64b56bef-dirty`. It starts `WIFI_MODE_AP` at
`GAFVent_XXXX`; the app uses TCP at `192.168.4.1`. The listener port and
authentication are unknown; no LAN provisioning flow was found. Direct Wi-Fi/TLS,
manual mode, dedicated on/off, OTA, reboot, and reset are unsupported. App OTA
commands are `ois`, `oms`, `ome`, `rbs` (replies `oir`, `osr`, `oer`, `rbr`);
firmware token `#pptP` is unexplained. OTA can append binary data before LF.

Captures on **2026-09-29** used one firmware 3.0.0 controller; identifiers are
redacted. Reads returned `idr030000…`, `dmraf`, `sdr03ca00aa` (97.0 °F/17.0%),
then `sdr03da00a0` (98.6 °F/16.0%), `atr041a012c` (105.0 °F/30.0%), and
`ttr00000000` (zero remaining/original minutes). No pairing was recorded.

| Write | Reply / matching readback |
| --- | --- |
| `#ams041B012D\n` | `#amr0\n`, `#atr041b012d\n`; automatic/off at 98.3 °F/16.9% |
| `#tms0001\n` | `#tmr0\n`; timer/on, 1/1 minute |
| `#tms0000\n` | `#tmr0\n`; timer/off, 0/0 |
| `#ams041A012C\n` | `#amr0\n`; automatic/off, 105.0 °F/30.0%, at 99.7 °F/15.8% |

On **2026-10-01**, the same controller confirmed 110 °F, 40% humidity,
a two-minute timer, and timer clear, then returned to Automatic at 105 °F/30%.
Tests checked acknowledgements/readback, not airflow. Full range endpoints and
repeated physical stop/start recovery still require hardware checks.

## QuickConnect API

Experimental, read-only by default, untested with a live account or fan.
The implementation follows [GAFVentControl-HA](https://github.com/hitchin999/GAFVentControl-HA)
1.1.0 at `336adfd8d8cc0a936b4585bd20301f74d585554c`, based on Android
`com.gaf.quickconnectapp` 1.0.9. See the
[reference license](../LICENSE-QUICKCONNECT-REFERENCE.txt).

Authentication root: `https://gaf-coreservices.aurai.io/cognito/`.
Device root: `https://gaf.keenhome.io/gaf/`. Reference timeout: 20 seconds.
Login pool: `us-east-2_F6aHzg32w`. Roles: `contractor` (default) or `consumer`.
Send the ID token in `Authorization` without `Bearer `.

| Operation | Request / data |
| --- | --- |
| Login | POST `cognito/login`: `userName`, Base64 UTF-8 `password`, `userPoolId`, `userRole`; read `responseData.idToken` |
| Inventory | GET `device/deviceList`; `responseData` list or object with `devices` list |
| Detail | GET `device?deviceId=<id>`; merge `responseData` over inventory |
| Settings | POST `deviceMode/<id>` |
| Firmware info | GET `fw/fwInfo?deviceId=<id>`; diagnostics only |

Malformed inventory is rejected. Reads can retry transient errors and refresh
authentication; settings writes have no automatic retries. Firmware updates are unsupported.

`deviceConfig.setTemperature`/`setHumidity` hold ambient measurements;
`deviceSettings.setTemperature`/`setHumidity` hold targets. Settings also include
`automaticMode`, `timerMode`, `fanMode`, `timerValue` (minutes), and
`humidityMonitor` (readable; the reference reports rejected writes).
Missing/conflicting mode flags produce unknown mode. Estimated running is
unknown without the required data; motor operation is unmeasured.

| Mode | `(automaticMode, timerMode, fanMode)` |
| --- | --- |
| Off | `(false,false,false)` |
| Automatic | `(true,false,false)` |
| Timer | `(false,true,false)` |
| Manual | `(false,false,true)` |

Mode writes send `automaticMode`, `desiredTemp`, `desiredHumidity`, `timerMode`,
`timerValue`, and `fanMode`, preserving targets/duration. Target writes send
`automaticMode`, `desiredTemp`, `desiredHumidity`, preserving the mode flag.
Duration writes send `timerMode`, `timerValue`, preserving timer mode. All writes
read settings first and require a successful response and matching readback;
ambiguous responses remain unconfirmed. Reference UI bounds are in [Controls](#controls);
float writes were rejected with HTTP 417/application status 4444.

[Fixtures](../fixtures/quickconnect/) are synthetic; their
[manifest](../fixtures/quickconnect/manifest.json) records provenance. Request
payloads live under `body`. Accepted-write envelopes, aliases, model-specific
units, timestamps, offline behavior, and timer timing remain unverified.

## Model sources

The [README model table](../README.md) lists original, built-in QuickConnect,
EZ Cool, and retrofit controllers. Standard ERV/EGV or EZ Cool fans need a
QuickConnect controller for cloud access; model suffixes/availability vary.
QuietCool and mechanical-thermostat-only fans are unsupported. The original
controller uses its own Wi-Fi AP; Bluetooth lets the server stay on the LAN.
QuickConnect inventory is not filtered by fan model; devices must return the
fields above. Manufacturer references checked **2026-10-02**:

- [2018 instructions](https://images.thdstatic.com/catalog/pdfImages/20/2027af1f-49ef-4f20-83ce-3de23c8c00c5.pdf): ERV5SMT roof / EGV5SMT gable.
  [App release notes](https://apps.apple.com/us/app/gaf-wi-fi-vent/id1388395737): firmware 3.0.0 adds BLE.
- [RESMF314, page 2](https://www.gaf.com/en-us/document-library/documents/data-sheets/master-flow-wi-fi-attic-vent-resmf314-%2811-22%29-_sell-sheet.pdf): ERV5QCT / EGV5QCT.
- [RESWT189, page 1](https://www.gaf.com/en-us/document-library/documents/warranties/master-flow-powered-ventilation-products-limited-warranty-trilingual-reswt189.pdf): EZCQCR1 / EZCQCG1.
  [RESMF319](https://www.gaf.com/en-us/document-library/documents/data-sheets/master-flow-ez-cool-plug-in-power-vent-resmf319_data-sheet.pdf): April 2024 edition includes QuickConnect options; October 2025 omits them.
- [RESCB100, page 19](https://www.gaf.com/en-us/document-library/documents/brochures-%26-literature/brochure__ventilation_full_line_brochure__rescb100.pdf) and
  [RESMF332](https://www.gaf.com/en-us/document-library/documents/installation-instructions-%26-guides/master-flow-quickconnect-control-module-instructions-trilingual-resmf332-%283-23%29.pdf): QuickConnect retrofit module for ERV/EGV fans, replacing thermostat or humidistat/thermostat control.
