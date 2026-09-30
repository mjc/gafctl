# Home Assistant entity and capability contract

This contract describes the legacy GAF Wi-Fi Vent path. It does not inherit
capabilities from the newer cloud-connected QuickConnect product. The source
comparison is the [GAFVentControl-HA README at the reviewed
commit](https://github.com/hitchin999/GAFVentControl-HA/blob/336adfd8d8cc0a936b4585bd20301f74d585554c/README.md).

## Integration boundary

Implement a thin native Home Assistant custom integration backed by Updraft's
local Rust API. The integration owns configuration flow, device/entity
registration, polling, and Home Assistant errors. Rust remains authoritative
for BLE transport, protocol parsing, capability reporting, write semantics,
per-device transaction serialization, and state freshness. The integration
must not connect to GAF's cloud.

Prefer this over MQTT discovery: it does not require a separately configured
broker, and it can use Home Assistant's native device registry, config flow,
availability, and update coordinator. Keep the HA integration thin; put durable
protocol behavior and control/readback invariants in Rust. The local API and
its access boundary are part of Updraft's service design.

## Verified legacy capabilities

The installed legacy app and one nearby target fan establish the following
subset. Exact model/controller identification is still pending.

| Capability | Evidence | Contract |
| --- | --- | --- |
| Firmware identity | Exact-device `idg` reply begins with version `030000`. | Report software version `3.0.0`; do not expose the redacted identity suffix as a serial number. |
| Temperature and humidity | Exact-device `sdg` reads; app decoding agrees with the raw reply. | Temperature is Fahrenheit in tenths; relative humidity is percent in tenths. Convert tenths to Home Assistant's numeric units (for example, `986` becomes `98.6 °F`). |
| Automatic thresholds | Exact-device `atg` readback reports 105.0 °F and 30.0%. `ams` changed both to 105.1 °F / 30.1%, returned the app-verified success ACK, and the following `atg` matched. The original values were then restored and read back. | Expose paired temperature/humidity number controls only within verified device bounds. Each edit sends both values: fetch fresh paired `atg` first, preserve the untouched value, then issue `ams`. `ams` selects automatic mode. Confirm only with accepted `amr0`, both threshold readbacks matching, and `dmr` reporting automatic. |
| Controller mode and fan flag | Exact-device `dmg` reads report automatic and timer modes, with the controller fan flag both off and on across captures. | Preserve these observations as controller state. They do not establish physical airflow or motor operation. Automatic-threshold writes select automatic mode; timer writes select timer mode. No standalone off/manual mode command is verified. |
| Timer | Exact-device `ttg` reads report remaining/original minutes. `tms` started a one-minute timer and cleared it with zero; both returned the app-verified success ACK and matching timer readback. | Expose remaining time as a sensor and a timer-duration number control after its accepted upper bound is established. Zero clears the timer but leaves the controller in timer mode with its fan flag off. A positive duration selects timer mode. |
| BLE reachability | The target advertises GAF service `00FF`; Updraft reads characteristic `FF01`. | Availability is based on successful Updraft polls, not a cloud account or the newer product's `Verified` field. |

The exact-device captures and control readbacks are recorded in [protocol
findings](protocol-findings.md#control-tests). Those observations establish
specific accepted values, not the full numeric limits.

The verified table is deliberately narrower than the upstream cloud
integration. Its reported model includes a derived Running sensor, account
verification, serial number, manual mode, signal strength, and firmware-update
entities. Those are cloud/product-specific or unsupported here. In particular,
Updraft must not claim a physical Running state from the legacy controller's
mode/fan flag.

## Entity mapping

Create one Home Assistant device per configured physical fan. Use a locally
generated opaque stable identifier in the HA device registry. Keep the BLE
selector in local integration configuration; never put its raw value, the
device identity suffix, credentials, or keys in entity names, diagnostics, or
logs. Until the label is recorded, report the model as `GAF Wi-Fi Vent` and
leave the exact model/controller revision unknown.

| Entity | Home Assistant representation | Availability and write behavior |
| --- | --- | --- |
| Ambient temperature | Temperature sensor, °F, measurement | Convert the wire tenths to °F; update only from a decoded `sdg` observation. |
| Relative humidity | Humidity sensor, %, measurement | Convert the wire tenths to percent; update only from a decoded `sdg` observation. |
| Automatic temperature threshold | Number control, °F | Read from `atg`. In a serialized per-device transaction, read both thresholds fresh, preserve humidity, then issue `ams`. It writes both values and selects automatic mode; confirm with accepted ACK, matching paired `atg`, and automatic `dmr`. Keep disabled until the device-accepted range is established. |
| Automatic humidity threshold | Number control, % | Read from `atg`. In a serialized per-device transaction, read both thresholds fresh, preserve temperature, then issue `ams`. It writes both values and selects automatic mode; confirm with accepted ACK, matching paired `atg`, and automatic `dmr`. Keep disabled until the device-accepted range is established. |
| Timer remaining | Duration sensor, minutes | Read from `ttg`; never present the original duration as remaining time. |
| Timer duration | Number control, minutes | `tms` is verified for 0 (clear) and 1 minute. Zero clears the timer; positive values select timer mode. Keep the upper range disabled until its accepted bound is established. Confirm with accepted ACK and matching `ttr`; accept a timer that expires before readback only when the original duration matches, remaining time is zero, and controller state is consistent with expiration. |
| Controller mode | Diagnostic sensor | Report decoded automatic/timer observations. Do not present a separate mode selector: available commands select automatic or timer, and no independent off/manual operation is verified. |
| Controller fan flag | Diagnostic sensor or attribute | Label explicitly as a controller report. Do not expose it as physical `running` or airflow. |
| Firmware version | Diagnostic sensor/device software version | Use the decoded version prefix only. No serial number entity. |
| BLE signal | Diagnostic sensor, dBm, if the platform supplies RSSI | Identify it as transport RSSI; do not imply it is a fan sensor or use it as the sole availability signal. |

Do not create a generic `climate` entity: the verified legacy protocol does not
provide ordinary heating/cooling modes, and the newer cloud integration's
`Manual`/`Off` mode set cannot be assumed to match this controller. Do not add a
`Running` binary sensor, OTA/reboot controls, or QuickConnect cloud entities
until exact-fan evidence supports them. A later contract change must cite the
fixture and controlled exact-device readback that establish each added
capability.

## Identity, freshness, and command outcomes

- Keep the entity unique ID stable across restarts using the locally stored
  opaque integration identifier. Do not derive public IDs or labels from a raw
  BLE address or the identity suffix.
- Poll a complete five-reply snapshot every 30 seconds. Timestamp each
  successful snapshot and expire availability when monotonic elapsed time since
  the last complete decoded snapshot reaches 90 seconds. Enforce the age limit
  even while a poll is pending or blocked on a timeout. Keep consecutive poll
  failures as additional diagnostics; they do not define freshness. Retain the
  last successful values only as stale diagnostics. A successful complete poll
  restores availability.
- A partial, malformed, or undecodable reply does not replace the last
  confirmed value. Expose its typed error as diagnostic state and keep the
  device unavailable once the freshness limit is crossed.
- For controls, track requested value, device acknowledgement, state readback,
  and mode readback as separate facts. Do not optimistically update HA state.
  Serialize each fan's full transaction, including scheduled polls and all
  other controls, so concurrent edits cannot overwrite a paired threshold
  update. Within the lock/queue, fetch fresh values, preserve the untouched
  threshold, send the paired command, then read back and release the next
  operation.
  Threshold confirmation requires accepted `amr0`, both requested values in a
  fresh matching `atg`, and automatic mode in `dmr`. Timer confirmation
  requires accepted `tmr0`, matching original duration in `ttr`, and timer mode;
  if it expires before readback, accept only the verified expiration case
  (matching original duration, zero remaining, and automatic mode). A mismatch
  or timeout leaves the last confirmed value intact and reports the failure.
- The controller fan flag is never physical proof. Do not synthesize a
  `Running` sensor from thresholds or mode; that inference belongs only to the
  distinct upstream cloud product and is not validated for this legacy fan.

## Evidence still required before enabling controls

- Read the exact fan/controller model and app-pairing state from its label or
  UI and record it without publishing device identifiers.
- Establish device-accepted temperature, humidity, and timer upper bounds
  before publishing editable number limits; the documented exact-device tests
  establish only the values exercised there.
- Verify any additional mode/off transition before adding a mode control. The
  current contract exposes no standalone mode selector.
- Prove the controller's reported fan flag against physical airflow before
  adding any entity named `Running`.

The exact model, validated control ranges, and measured airflow are acceptance
requirements before exposing the corresponding entities. Keep OTA, reboot,
reset, and firmware replacement outside the integration.
