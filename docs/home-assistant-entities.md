# Home Assistant and MQTT

For the HTTP integration, follow the
[README installation steps](../README.md#add-it-to-home-assistant). Updraft runs
on the computer that communicates with the fan; Home Assistant connects to its
HTTP API. Home Assistant itself does not need Bluetooth for this setup.

## Entities

For an original **GAF Master Flow Wi-Fi Attic Vent** (ERV5SMT or EGV5SMT):

| Entity | Value or control |
| --- | --- |
| Ambient temperature | Attic temperature, °F |
| Relative humidity | Attic relative humidity, % |
| Controller mode | Automatic, timer, or OTA |
| Controller fan flag | The controller's reported on/off state |
| Firmware version | Controller firmware version |
| Automatic temperature threshold | Current threshold, °F |
| Automatic humidity threshold | Current threshold, % |
| Timer remaining | Remaining timer minutes |
| Automatic thresholds | Select 105.0 °F / 30.0% or 105.1 °F / 30.1% |
| Fan timer | Select Clear timer or 1 minute |
| Refresh readings | Request a new device reading through the service |

The automatic selector sets both thresholds and automatic mode. Clearing the
timer leaves the controller in timer mode; use an automatic preset to return to
automatic operation. A selector shows unknown when the current settings do not
match an available choice. An expired timer does not show as an active one-minute
timer. These thresholds are tested presets, not recommendations for your attic.

QuickConnect devices expose temperature, humidity, and available diagnostics.
They include the Refresh readings button without enabling cloud writes.
When experimental cloud writes are enabled, they also expose:

| Entity | Choices or range |
| --- | --- |
| Mode | Off, Automatic, Timer, Manual |
| Target temperature | 90–120 °F in 1 °F steps |
| Target humidity | 30–80% in 1% steps |
| Timer duration | 30–360 minutes in 30-minute steps |

QuickConnect timer duration is a configured setting, not a countdown. Saving the
duration does not start the timer. Its Running diagnostic is estimated from the
reported mode and measurements. Neither that estimate nor the original
controller's fan flag measures airflow.

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

MQTT discovery can add the original Bluetooth fan's entities instead of the HTTP
integration. Configure Home Assistant's MQTT integration first. The broker must
support **MQTT 5**.

Set these variables in the service environment, using your own broker and account:

```ini
UPDRAFT_MQTT_HOST=BROKER_HOST
UPDRAFT_MQTT_PORT=1883
UPDRAFT_MQTT_USERNAME=UPDRAFT_BROKER_USER
UPDRAFT_MQTT_PASSWORD=YOUR_BROKER_PASSWORD
UPDRAFT_MQTT_DISCOVERY=true
```

Restart Updraft, then select MQTT ownership through the
[entity-source endpoint](http-api.md#home-assistant-entity-source). Enabling
discovery alone leaves HTTP ownership unchanged. Each fan has one HA owner for
both readings and controls. Both administrative transports remain available.
Keep the environment file private. The password is required when MQTT is enabled;
there is currently no MQTT password-file option.

Updraft's MQTT connection currently uses plain TCP. Setting port 8883 alone does
not enable TLS. Use a trusted network or a local TLS tunnel if your broker
requires encrypted connections.

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

Leave `UPDRAFT_MQTT_DISCOVERY` unset or false to publish MQTT state without
Home Assistant discovery.

## Topics and broker access

Use separate broker accounts for Updraft and Home Assistant. Give each account
only these permissions:

| Account | Operation | Topics |
| --- | --- | --- |
| Updraft | Publish | `updraft/+/availability`, `updraft/+/+/state`, `updraft/+/+/availability`, `updraft/+/+/control/result` |
| Updraft | Publish discovery | `homeassistant/+/+/+/config` |
| Updraft | Subscribe | `updraft/+/+/control/set` |
| Home Assistant | Publish | `updraft/+/+/control/set` |
| Home Assistant | Subscribe | `updraft/+/availability`, `updraft/+/+/state`, `updraft/+/+/availability`, `updraft/+/+/control/result`, `homeassistant/+/+/+/config` |

Avoid a publish grant on all of `updraft/#`; that would let a command client
publish service state too. Every proxy has its own namespace. There are no unscoped aliases.

Read `proxy_id` and device `id` from `/api/v2/devices`. Device topics use
`updraft/{proxy_id}/{id}/...`; discovery identifiers also include both IDs. MQTT
client IDs include the proxy UUID so separate services can share a broker.

Device state and availability messages are retained. Process availability has
its own last-will topic, `updraft/{proxy_id}/availability`. It does not replace each fan's
availability. After reconnecting, Updraft republishes current state and enabled
discovery messages.

## Controls

The Home Assistant integration and discovered MQTT controls create requests for
you. Custom MQTT clients publish JSON to `updraft/{proxy_id}/{id}/control/set` with QoS 1
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

Results arrive at `updraft/{proxy_id}/{id}/control/result`, include the request ID, and are
not retained. A successful control requires acknowledgement and matching state
readback. Home Assistant does not display a requested setting as if it had
already succeeded.

If no result arrives, read current state before retrying: the write might have
completed. Updraft caches 64 completed requests per device. Repeating a cached
ID with the same command returns its result; a different command with that ID is
rejected. The cache is in memory and is lost on restart. It cannot guarantee that
a retry after a restart will avoid another write.
