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

The automatic selector sets both thresholds and automatic mode. Clearing the
timer leaves the controller in timer mode; use an automatic preset to return to
automatic operation. A selector shows unknown when the current settings do not
match an available choice. An expired timer does not show as an active one-minute
timer. These thresholds are tested presets, not recommendations for your attic.

QuickConnect devices expose temperature, humidity, and available diagnostics.
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
seconds and reads the service's cached state. A manual refresh in Home Assistant
does not force a new Bluetooth query. Original-controller snapshots become
unavailable after 90 seconds without a complete successful reading.

`/health` checks whether the service responds. To check the fan, use
`/api/v2/devices/{id}/state` and inspect `available`, `last_error`, and the
state timestamps. A cloud outage affects cloud devices without stopping Bluetooth
polling.

Home Assistant adds one integration entry per selected fan. Add the integration
again to select another fan. Keep the QuickConnect identity store across restarts
so existing cloud devices keep their IDs. The service currently configures one
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

Restart Updraft. Home Assistant should discover the Bluetooth fan through MQTT.
Do not also add that fan through the HTTP integration: the explicit Bluetooth
MQTT discovery option publishes its entities independently of HTTP ownership.
Keep the environment file private. The password is required when MQTT is enabled;
there is currently no MQTT password-file option.

Updraft's MQTT connection currently uses plain TCP. Setting port 8883 alone does
not enable TLS. Use a trusted network or a local TLS tunnel if your broker
requires encrypted connections.

QuickConnect defaults to HTTP for both state and controls. Enabling
`UPDRAFT_MQTT_DISCOVERY` alone does not move cloud entities to MQTT. To select
MQTT for a cloud device, use its local ID with the
[entity-source endpoint](http-api.md#home-assistant-entity-source), setting both
sources to `mqtt`. State and controls must use the same source. The CLI does not
currently expose a source-selection command. The selection persists in the
identity store; keep a broker and discovery configured while any device uses MQTT.

Leave `UPDRAFT_MQTT_DISCOVERY` unset or false to publish MQTT state without
Home Assistant discovery.

## Topics and broker access

Use separate broker accounts for Updraft and Home Assistant. Give each account
only these permissions:

| Account | Operation | Topics |
| --- | --- | --- |
| Updraft | Publish | `updraft/availability`, `updraft/+/state`, `updraft/+/availability`, `updraft/+/control/result` |
| Updraft | Publish discovery | `homeassistant/+/+/+/config` |
| Updraft | Subscribe | `updraft/+/control/set` |
| Home Assistant | Publish | `updraft/+/control/set` |
| Home Assistant | Subscribe | `updraft/availability`, `updraft/+/state`, `updraft/+/availability`, `updraft/+/control/result`, `homeassistant/+/+/+/config` |

Avoid a publish grant on all of `updraft/#`; that would let a command client
publish service state too. Existing `updraft/gaf_vent/...` topics remain aliases
for the original Bluetooth fan; use the per-device topics for new clients.

Device state and availability messages are retained. Process availability has
its own last-will topic, `updraft/availability`. It does not replace each fan's
availability. After reconnecting, Updraft republishes current state and enabled
discovery messages.

## Controls

The Home Assistant integration and discovered MQTT controls create requests for
you. Custom MQTT clients publish JSON to `updraft/{id}/control/set` with QoS 1
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

Results arrive at `updraft/{id}/control/result`, include the request ID, and are
not retained. A successful control requires acknowledgement and matching state
readback. Home Assistant does not display a requested setting as if it had
already succeeded.

If no result arrives, read current state before retrying: the write might have
completed. Updraft caches 64 completed requests per device. Repeating a cached
ID with the same command returns its result; a different command with that ID is
rejected. The cache is in memory and is lost on restart. It cannot guarantee that
a retry after a restart will avoid another write.
