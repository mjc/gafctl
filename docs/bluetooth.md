# Original-controller Bluetooth

The reference is the Android GAF Wi-Fi Vent app (`com.gafs.android`, version
2.0.0, version code 3). The APK SHA-256 is
`0dfd606669c7bec6d0d920d38238710a305fb2815d47a5a6b07911caf761eeaa`.
The iOS app and firmware analysis in [protocol findings](protocol-findings.md)
provide separate protocol evidence.

## Connection and requests

Gafctl retains one connection to the configured original controller while the
server runs. Polls and controls share that connection and execute serially.
A direct CLI operation closes its connection before returning.

| Step | Android behavior | Gafctl behavior |
| --- | --- | --- |
| Discovery | Five-second unfiltered BLE scan; names start with `GAFVent_` | Five-second scan; the same name prefix for automatic selection, exact peripheral ID for a configured fan |
| Connection | `connectGatt` with `autoConnect=false`; retain GATT across readings | Explicit btleplug connection; retain the native peripheral and notification stream |
| Services | Service `00FF`, characteristic `FF01` | Same UUIDs; require read, write, and notify properties |
| Responses | Enable local notifications, write CCCD `2902`, then read FF01 | Register native notifications, subscribe, then read FF01; the OS handles CCCD |
| Initialization | Identity → sensors → thresholds → mode | `idg` → `sdg` → `atg` → `dmg`, awaiting each reply |
| Timer initialization | Read timer for timer mode with fan on | Request `ttg` on every poll, including when the fan is off |
| Normal polling | Request sensors every three seconds | Read sensors, thresholds, mode, and timer three seconds after the preceding successful poll completes |
| Ordinary controls | `ams` / `tms`, ASCII and LF, firmware version 2 or later | Same encoding and firmware gate; no sent control is automatically replayed |
| Teardown | Disconnect, close GATT, clear the handle | Drop the stream/session and disconnect the tracked native peripheral |

Android starts initialization from its first characteristic read or
notification. Gafctl awaits the initial read before sending identity. Queued
startup data is discarded because it predates the identity request.

The Android app has overlapping initialization callbacks and ignores some GATT
callback errors. Gafctl awaits each operation, matches response command IDs,
and rejects malformed frames. It does not copy Android's first-connect write of
saved automatic thresholds: connecting for readings must not change settings.

## Settings and controls

Identity is connection-scoped metadata. Each published poll reads sensors,
automatic thresholds, mode, and timer. The Android
app's recurring request reads sensors only; its mode and timer display can be
stale. Gafctl reads the changing fields together so Home Assistant receives
current settings and fan status. These reads share the retained connection.
Timer reads also run when the fan is off, preserving the controller's reported
remaining time and starting duration.

The reading timestamp starts when the sensor reply arrives. Later replies and
unsolicited frames do not renew that timestamp. HTTP and MQTT freshness use
both observation and fetch timestamps. A failed poll or invalid control
readback immediately makes readings unavailable; a complete successful reading
restores availability. Last raw observations remain in diagnostics.

Controls retain their acknowledgement and then read settings, mode, sensors,
and timer for confirmation. Partial threshold changes first read current
settings so the unchanged threshold is preserved. Automatic reapplies both
current raw thresholds. Timer starts the saved duration, or selects Automatic
when it is zero. Timer duration edits only save a preference. Off sends a
zero-minute timer command. The service restores the preceding mode after timer
expiry and fresh matching readback; see [timed runs](home-assistant-entities.md#returning-from-a-timed-run). These extra reads provide
Gafctl's control confirmation contract; Android's setter has no equivalent
acknowledgement/readback check.

Replies already queued before a request cannot acknowledge that request.
An incomplete queued frame must finish within the response deadline before a
new command can be sent. A control deadline is checked again after that drain.
The protocol has no request sequence number; a response received after a write
is correlated by command ID. Failed or interrupted exchanges release the
session before reconnecting.

Malformed initialization settings remain in diagnostic output and are not
retained in the connection cache. Failed reads invalidate
the session and make the device unavailable; process health does not imply
fan availability. Authentication and protocol failures are reported distinctly.

## Recovery and shutdown

Transient connection failures retry with the bounded exponential backoff and
jitter described in [protocol findings](protocol-findings.md#timeouts-and-recovery).
Each retry closes the preceding failed connection. Terminal setup cleanup
belongs to the backend. Failed polls also back off with jitter, up to sixty
seconds between attempts, and return to three-second polling after success.

Tracked connection attempts are explicitly disconnected on every platform,
including when the native connection has not completed. CoreBluetooth reports
an in-progress connection as not connected; that flag does not cancel the
connection attempt. Failed or timed-out cleanup remains pending for retry.

Shutdown closes admission, interrupts the active exchange, drops the retained
session, and disconnects the tracked peripheral. Cleanup remains bounded and
is retried within the shared shutdown deadline if it fails. It does not disable
CCCD explicitly; Android has no such teardown handshake.

btleplug delegates GATT lifetime and CCCD handling to BlueZ on Linux and
CoreBluetooth on macOS. The pinned BlueZ scan implementation requests transport
`Auto`; Android's `startLeScan` requests LE scanning. Both discover LE devices,
but their radio discovery filters are not byte-for-byte equivalent. Gafctl does
not request pairing, change MTU/PHY, reset the controller, or send firmware commands.

## Evidence limits

The APK establishes command bytes, callback ordering, connection lifetime,
polling cadence, and teardown calls. It does not establish on-air timing or
which difference caused a vent to stop advertising. Tests cover request traces,
queued replies, fragmentation, deadlines, invalid settings, and cleanup ownership.
Physical recovery and repeated stop/start checks require the actual fan.
