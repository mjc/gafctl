# Original Master Flow Bluetooth protocol

Original GAF Master Flow Wi-Fi Attic Vent models ERV5SMT and EGV5SMT use this
Bluetooth protocol. The description comes from the GAF Wi-Fi Vent app,
bundled firmware, and captures from one controller reporting firmware `030000`
(3.0.0). See [protocol findings](protocol-findings.md) for recorded replies.

The `v1` filename denotes the document revision. The fan does not negotiate
protocol revisions.

## Bluetooth and framing

- Service UUID: `00FF`.
- Command/response characteristic UUID: `FF01`.
- Enable characteristic notifications before exchanging requests.
- Frames are `#`, a three-letter lowercase ASCII command, a payload, and LF (`\n`).
- Numeric fields are hexadecimal scalars. Setters use uppercase hexadecimal;
  captured read replies use lowercase hexadecimal.
- A notification may contain a fragment or several frames. Join fragments through
  LF and split complete frames before decoding.
- Gafctl rejects frames over 1024 bytes. The controller's frame-size limit is
  unknown.

## Reads

| Request | Reply | Decoded fields |
| --- | --- | --- |
| `#idg\n` | `#idr` + identity + LF | First six decimal digits: firmware version. Remaining bytes: private identity suffix. |
| `#dmg\n` | `#dmr` + two characters + LF | First character: `a` automatic, `t` timer, `o` OTA. Second: `f` fan flag off, `n` fan flag on. |
| `#sdg\n` | `#sdr` + eight hex digits + LF | Four digits each: temperature in tenths of °F, humidity in tenths of a percent. |
| `#atg\n` | `#atr` + eight hex digits + LF | Four digits each: automatic temperature threshold and humidity threshold, in the same units. |
| `#ttg\n` | `#ttr` + eight hex digits + LF | Four digits each: remaining timer minutes, original timer minutes. |

All five reads produced decoded replies on the tested controller. The identity
suffix is opaque; the parser does not infer a serial number or roof/gable model
from it. The fan flag reports the controller's on/off state; airflow is not measured.

## Controls

| Operation | Request | Successful acknowledgement | Required readback |
| --- | --- | --- | --- |
| Automatic thresholds | `#ams` + two four-digit uppercase hex fields + LF | `#amr0\n` | Both thresholds match; mode is automatic. |
| Timer | `#tms` + four-digit uppercase hex minutes + LF | `#tmr0\n` | Timer values match; mode is timer. A zero clear also requires the fan flag off. |

The direct Bluetooth CLI exposes four presets captured on the controller.
HTTP and Home Assistant expose the same presets:

- Automatic: 105.0 °F / 30.0% (`041A012C`).
- Automatic: 105.1 °F / 30.1% (`041B012D`).
- Timer clear: zero minutes (`0000`).
- Timer start: one minute (`0001`).

HTTP and Home Assistant also expose adjustable temperature (90–120 °F),
humidity (30–80%), and timer duration (0–360 minutes), in whole-unit steps. These
bounds come from the manufacturer app; zero clears the timer. Changing one
threshold first reads and preserves the other raw threshold under the same
device transaction. If the unchanged threshold cannot be read, the service
rejects the write.

Tests on one controller confirmed 110 °F, 40%, a two-minute timer, and clear,
then restored automatic mode at 105 °F / 30%. The range endpoints have not all
been tested on hardware. See [control tests](protocol-findings.md#adjustable-control-tests).

A mismatched, missing, or undecodable readback
leaves the write unconfirmed. A timer that appears to have expired before readback
is also unconfirmed. Clearing the timer leaves timer mode active; an automatic
write is needed to return to automatic mode.

The diagnostic probe accepts raw `u16` values. The controller's full range,
including its maximum timer duration, has not been tested. Service controls
remain limited to the app-derived ranges.

## Unsupported operations

Gafctl does not expose a separate original-controller on/off command, manual
mode, firmware update, reboot, or reset. OTA/reboot tokens found in the app and
firmware are recorded in the findings for research only. Direct Wi-Fi/TLS control
has not been implemented; its listener port and authentication are unresolved.

The tested Bluetooth connection did not capture a pairing or authentication
exchange. Other firmware versions and pairing states have not been tested.

## Timeouts and recovery

A scan defaults to six seconds. GATT setup, request writes, and response waits
use a three-second timeout by default. Platform adapter setup, scanning,
connection, and cleanup allow at least 40 seconds for operating-system calls.

Transient connection failures can retry after cleanup. Recovery uses exponential
backoff with jitter: 0.5–1, 1–2, 2–4, and 4–8 seconds. There are at most five
attempts; new retries stop after a 20-second recovery window. An already-running
attempt may finish later. Authentication, protocol, and cleanup errors stop
recovery. Once requests begin, control writes are not automatically replayed.

Timeouts do not prove a write failed to take effect. Preserve decoded fields and
per-field errors on partial reads; check current state before retrying a control.

## Service mapping

The service requires a persistent identity store when a fan is configured. It
registers the original Bluetooth fan as `configured`; its Bluetooth peripheral
ID stays out of the public API. The [HTTP API](http-api.md) describes
normalized state and commands. Each device serializes polling and controls so a
state read cannot interleave with a control/readback transaction.

QuickConnect has a separate backend and
[separate cloud protocol](quickconnect-contract.md). Its account/device IDs are
mapped to persistent local IDs in `GAFCTL_IDENTITY_STORE`. The ID `configured`
is reserved for the original Bluetooth controller.
