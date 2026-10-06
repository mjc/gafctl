# Home Assistant and MQTT

For the HTTP integration, follow the
[README installation steps](../README.md#add-it-to-home-assistant). Gafctl runs
on the computer that communicates with the fan; Home Assistant connects to its
HTTP API. The Gafctl host supplies Bluetooth for original controllers.

## Entities

For an original **GAF Master Flow Wi-Fi Attic Vent** (ERV5SMT or EGV5SMT):

| Entity | Value or control |
| --- | --- |
| Ambient temperature | Attic temperature, °F |
| Relative humidity | Attic relative humidity, % |
| Controller mode | Automatic, timer, or OTA |
| Controller fan flag | Binary diagnostic of the controller's reported on/off state |
| Firmware version | Controller firmware version |
| Automatic temperature threshold | Current threshold, °F |
| Automatic humidity threshold | Current threshold, % |
| Timer remaining | Remaining timer minutes |
| Original timer setting | Original timer field reported by the controller, minutes |
| Automatic thresholds | Select 105.0 °F / 30.0% or 105.1 °F / 30.1% |
| Fan timer | Select Clear timer or 1 minute |
| Target temperature | 90–120 °F in 1 °F steps |
| Target humidity | 30–80% in 1% steps |
| Timer duration | 0–360 minutes in 1-minute steps; zero clears |
| Refresh readings | Request a new device reading through the service |

The automatic selector sets both thresholds and automatic mode. Clearing the
timer leaves the controller in timer mode; use an automatic preset to return to
automatic operation. A selector shows unknown when the current settings do not
match an available choice. An expired one-minute timer shows unknown. The
presets were tested on one controller.

The adjustable numbers use ranges and whole-unit steps from the original
manufacturer app. Temperature and humidity commands change only the selected
value; the service reads and preserves the other raw threshold under the same
transaction before writing. Both select automatic mode. Timer duration starts
timer mode and reports the requested minutes. Remaining time is a separate
sensor. Fractional threshold readback is displayed without rounding; new
settings use whole units. The full ranges have not been tested on hardware.

[QuickConnect models and retrofit controllers](hardware.md#quickconnect) expose
temperature, humidity, and available diagnostics.
Refresh readings is available with writes disabled. Diagnostics include raw
signal strength and verification, OTA status, humidity monitoring, and mode
flags. Signal strength and verification are exposed verbatim. Missing or
conflicting mode flags show unknown. Firmware updates are unsupported. Enable
experimental cloud writes to add these controls:

| Entity | Choices or range |
| --- | --- |
| Mode | Off, Automatic, Timer, Manual |
| Target temperature | 90–120 °F in 1 °F steps |
| Target humidity | 30–80% in 1% steps |
| Timer duration | 30–360 minutes in 30-minute steps |
| Automatic, timer, manual switches | Select one active mode |
| All off | Select off mode |

Mode selectors, switches, and All off use the same serialized mode command.
They recheck proxy identity, HTTP ownership, current capabilities and state
before writing, then require confirmed control and matching current mode.
Turning off a switch for an inactive mode leaves the active mode unchanged.
Conditional off rejects unknown mode. These cloud controls are tested with
synthetic responses and have not been tested on a QuickConnect fan.

QuickConnect timer duration saves a setting and leaves the mode unchanged.
Running is estimated from mode and measurements. Airflow is unmeasured.

## Availability and updates

The service polls every 30 seconds. The HTTP integration also updates every 30
seconds and reads the service's cached state. The **Refresh readings** button
requests a device read, sharing any refresh already in progress for that device.
It reports failed or superseded reads and checks current identity and ownership
before updating HA. It can refresh a configured device whose readings are
unavailable. Original-controller snapshots become
unavailable after 90 seconds without a complete successful reading.

`/health` checks whether the service responds. To check the fan, use
`/api/v2/devices/{id}/state` and inspect `available`, `last_error`, and the
state timestamps. A cloud outage affects cloud devices without stopping Bluetooth
polling.

Home Assistant adds one integration entry per selected fan, identified by the
persistent proxy UUID and local device ID. Add the integration again to select
another fan. Keep the identity store across restarts so proxy and device IDs stay
stable. The service currently configures one
original Bluetooth fan, using local ID `configured`.

## MQTT setup

MQTT discovery exposes applicable entities for both original Bluetooth and
QuickConnect devices, using the same persistent proxy UUID and local device ID
as HTTP. Configure Home Assistant's MQTT integration first. The broker must
support **MQTT 5**.

Set these variables in the service environment, using your own broker and account:

```ini
GAFCTL_MQTT_HOST=BROKER_HOST
GAFCTL_MQTT_PORT=1883
GAFCTL_MQTT_USERNAME=GAFCTL_BROKER_USER
GAFCTL_MQTT_PASSWORD=YOUR_BROKER_PASSWORD
GAFCTL_MQTT_DISCOVERY=true
```

Restart Gafctl, then select MQTT ownership through the
[entity-source endpoint](http-api.md#home-assistant-entity-source). Each fan has
one HA owner for readings and controls. Administrative HTTP and MQTT commands
stay available.
Keep the environment file private. The password is required when MQTT is enabled;
there is currently no MQTT password-file option.

Gafctl uses plain TCP on every MQTT port. Use a trusted network or a local TLS
tunnel for a broker that requires encryption.

All devices default to HTTP ownership. Set both source fields to `mqtt` to move
a fan's HA entities to MQTT. Split ownership is rejected. The selection persists
in the identity store; MQTT owners in enabled backends require a configured
broker and discovery at startup. A broker outage does not change ownership.

The HTTP adapter removes its obsolete registry entities and empty device record
when it observes the change, including after a restart. MQTT discovery is removed
by publishing empty retained configurations, including for saved devices absent
from the active inventory. The adapter polls every 30 seconds, so handoff can
briefly expose both owners. HA gives integrations separate device records;
changing the owner can change HA device and entity IDs. Update automations that
refer to the old owner.

Leave `GAFCTL_MQTT_DISCOVERY` unset or false to publish MQTT state without
Home Assistant discovery.

## Topics and broker access

Use separate broker accounts for Gafctl and Home Assistant. Give each account
only these permissions:

| Account | Operation | Topics |
| --- | --- | --- |
| Gafctl | Publish | `gafctl/+/availability`, `gafctl/+/+/state`, `gafctl/+/+/availability`, `gafctl/+/+/control/result`, `gafctl/+/+/refresh/result` |
| Gafctl | Publish discovery | `homeassistant/+/gafctl/+/config` |
| Gafctl | Subscribe | `gafctl/+/+/control/set`, `gafctl/+/+/refresh/set` |
| Home Assistant | Publish | `gafctl/+/+/control/set`, `gafctl/+/+/refresh/set` |
| Home Assistant | Subscribe | `gafctl/+/availability`, `gafctl/+/+/state`, `gafctl/+/+/availability`, `gafctl/+/+/control/result`, `gafctl/+/+/refresh/result`, `homeassistant/+/gafctl/+/config` |

Avoid a publish grant on all of `gafctl/#`; that would let a command client
publish service state too. Every proxy has its own namespace.

Read `proxy_id` and device `id` from `/api/v2/devices`. Device topics use
`gafctl/{proxy_id}/{id}/...`; discovery uses
`homeassistant/device/gafctl/{identifier}/config`, with both IDs in the
identifier. Each retained device configuration contains all its entities under
`components`, with shared device and availability metadata. Entity unique IDs
stay unchanged when upgrading from individual discovery topics. Gafctl sends
nonretained HA migration messages before publishing the device configuration,
then clears the previous retained topics after the broker accepts the new
configuration. Keep the discovery ACL above during the upgrade.
MQTT client IDs include the proxy UUID so separate services can share a broker.

If migration markers or replacement publication fail before the broker accepts
the new configuration, the previous retained configurations remain unchanged.
After acceptance, interrupted cleanup is retried while the new configuration
remains retained. Gafctl retries on the next state update or broker reconnect.
HA can temporarily unload entities after receiving a migration message; a
successful retry restores them. If the replacement continues to fail, restarting
HA or reconnecting its MQTT integration replays the previous configurations.

Device state and availability messages are retained. Process availability uses
the last-will topic `gafctl/{proxy_id}/availability`. Each fan has a separate
availability topic. Reconnect republishes current state and discovery.

## Controls

The Home Assistant integration and discovered MQTT controls create requests for
you. Custom MQTT clients publish JSON to `gafctl/{proxy_id}/{id}/control/set` with QoS 1
and **retain disabled**:

```json
{
  "request_id": "attic-timer-1",
  "issued_at_unix_ms": 1790892000000,
  "command": {"kind": "legacy_preset", "preset": "timer_one_minute"}
}
```

Replace the example timestamp with the current Unix time in milliseconds and
use a new request ID for each new command. The
[HTTP API reference](http-api.md#controls) lists command shapes and outcomes;
MQTT uses the same commands. Requests more than 30 seconds old, more than five
seconds ahead, retained, malformed, or unsupported are rejected before fan access.

Results arrive at `gafctl/{proxy_id}/{id}/control/result`, include the request ID, and are
not retained. A successful control requires acknowledgement and matching state
readback. Home Assistant displays confirmed settings.

HA and the Rust client allow 300 seconds for a control response. Timeout,
disconnect, malformed or mismatched confirmation leaves the outcome unknown;
HA reports the submitted request ID and sends no automatic retry. MQTT publishes
a correlated `outcome_unknown` result when its 300-second wait expires or the
worker closes, if the broker is available. Result publication has a separate
30-second bound so stalled brokers cannot hold result slots indefinitely.

If no result arrives, read current state before sending another command: the
write might have completed. Gafctl caches 64 completed requests per device. Repeating a cached
ID with the same command returns its result; a different command with that ID is
rejected. Restart clears the cache, so reusing an ID afterward can write again.

### Refresh over MQTT

Publish a non-retained QoS 1 request to `gafctl/{proxy_id}/{id}/refresh/set`:

```json
{"request_id":"attic-refresh-1","issued_at_unix_ms":1790892000000}
```

Use the current timestamp. Refresh accepts only the request ID and timestamp;
control-shaped or malformed payloads are discarded. Retained and stale requests
return correlated rejections without reading the fan. Accepted reads use the same
per-device refresh worker as HTTP, including coalescing, serialization, and the
270-second backend deadline. A `fresh`, `failed`, or `superseded` result arrives
on `gafctl/{proxy_id}/{id}/refresh/result` without retention. Current state is
published on the usual state topic. MQTT waits up to 300 seconds; a backend read
may complete even if its result is lost.

The discovered refresh button requires process availability but can read an
unavailable device. Other controls require both process and device availability.
Numbers display confirmed state and send whole, bounded values. Switch-off uses
the backend conditional-off command so a different current mode is preserved.
Selectors for original-controller thresholds and timer settings are separate.
Unavailable capabilities receive platform-only removal entries in the device
configuration, including after a service restart. Switching to HTTP ownership
clears the retained device configuration.
