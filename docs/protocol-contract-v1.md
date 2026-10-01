# Legacy GAF BLE protocol contract

**Contract revision:** 1

**Evidence scope:** One GAF BLE peripheral reporting firmware version `030000` (3.0.0).

**Status:** The read and automatic-threshold operations below are verified. Other behaviors are explicitly unverified or out of scope.

This is a revision of the documented contract, not a version negotiated on the wire. It describes the observed BLE subset needed by the local Home Assistant proxy. Wi-Fi/TLS transport, its listener port, and its authentication policy are outside this contract.

## Transport and framing

- BLE service UUID: `00FF`.
- BLE command/response characteristic UUID: `FF01`.
- The central discovers the service and characteristic, enables notifications, then exchanges command and response frames on that characteristic.
- A frame is `#` followed by a three-letter ASCII command ID, a command-specific payload, and LF (`\n`). The command ID is case-sensitive. Observed IDs are lowercase.
- The observed frames are text with hexadecimal payload fields. A four-digit field is parsed as a scalar hexadecimal number; there is no observed byte-order rule or checksum field.
- The implementation rejects frames longer than 1024 bytes as a local safety bound. This is not a measured device limit.
- Fragmented notifications may be joined until LF. Multiple LF-delimited frames may arrive in one notification.

## Verified reads

| Request | Response | Contract |
| --- | --- | --- |
| `idg` | `idr` + identity | The first six decimal characters identify the firmware version. The remaining identity is opaque and must stay redacted; do not treat it as a serial number. |
| `dmg` | `dmr` + 2 characters | Character 1: `a` automatic, `t` timer, `o` OTA. Character 2: `f` controller reports fan off, `n` controller reports fan on. This is controller state, not physical airflow proof. |
| `sdg` | `sdr` + 8 hex characters | Two four-character fields: temperature in tenths of °F, then relative humidity in tenths of a percent. |
| `atg` | `atr` + 8 hex characters | Two four-character fields: automatic temperature threshold in tenths of °F, then humidity threshold in tenths of a percent. |
| `ttg` | `ttr` + 8 hex characters | Two four-character fields: remaining timer minutes, then original timer minutes. |

The five reads above have produced matching exact-device replies in repeated BLE sessions. The client parser agrees with the field order, scale, and units.

## Verified ordinary control

`ams` sets automatic mode and both thresholds. Its payload is two four-character uppercase hexadecimal values: temperature tenths of °F, then humidity tenths of a percent. Only exact `amr0` is treated as a successful acknowledgement. A following `atg` readback must match both requested values, and a `dmg` readback must report automatic mode, before the control is reported as confirmed. Exact-device writes received `amr0`; the paired threshold readbacks matched and the mode readbacks reported automatic.

No other mutating operation is enabled by this contract revision.

## Unverified or unsupported behavior

| Behavior | Current contract state |
| --- | --- |
| `tms` timer setter | Exact-device writes for 0 and 1 minute each received `tmr0` and matching `ttg` readback (`0/0` and `1/1`). A zero clear is confirmed only when `dmg` reports timer mode with the controller fan flag off; a positive timer requires timer mode. Keep the setter excluded from revision 1: the accepted general range, especially its upper bound, is unknown. A timer-expiry pattern before readback is unverified and is not a confirmed outcome. |
| A separate fan on/off command or other mode setter | No supported command is established. Do not synthesize one from the `dmr` state flags. |
| Pairing or authentication sequence | Not captured. The tested BLE path connected and read state, but this does not establish behavior for other devices or pairing states. |
| Wi-Fi/TLS | Not part of this contract. Listener port, authentication, and LAN reachability remain unknown. |
| Connection retries | The client retries transient discovery/connect failures up to three attempts with 100 ms and 300 ms delays. It does not replay a request or control exchange after connection, and it stops retrying if cleanup fails. |
| Accepted control ranges and physical actuation | Unverified. Controller acknowledgement/readback does not prove airflow or motor movement. |
| OTA, reboot, reset, `pptP`, and other unknown commands | Unsupported and must not be sent. |

The experimental BLE probe has a timer-write option, but that CLI capability does not extend this contract or define a Home Assistant capability. Do not expose timer writes through Home Assistant until the accepted range is established and bounded by the integration.

## Application identity and backend boundary

The legacy BLE service keeps the stable local ID `configured` and preserves its v1 response fields, supported presets, and Home Assistant entity identifiers. The BLE peripheral identifier remains local and is never returned by the API.

The service can start without `UPDRAFT_DEVICE_ID`. In that mode, v1 discovery returns an empty inventory, and the legacy configured state and control routes return 404. Cloud devices are not substituted for `configured`; their local IDs are generated independently and mapped to the private `(account, provider ID)` identity in the file selected by `UPDRAFT_IDENTITY_STORE`. The mapping is account-scoped, survives restarts and discovery reordering, and is not included in public device descriptions. New mapping files are created with owner-only permissions. Numeric provider ID `0` is valid.

Application state uses optional measurements, backend-specific settings, capabilities, and source provenance. Each registered local device owns its state and command transaction lock. State and command entity sources are selectable independently per device so HTTP and MQTT do not create duplicate entities or route commands to another device. QuickConnect devices begin read-only; no cloud controls are enabled by this application-model change.

## Error and retry behavior for clients

- A missing or malformed response is a protocol failure; retain the raw response bytes for diagnostics where safe.
- A BLE adapter, service, or peripheral that is absent is unavailable, not a protocol mismatch.
- A platform-level authentication or pairing rejection must remain distinguishable from unavailable and protocol failures when surfaced by the operating system. This device capture did not exercise that path.
- Bound discovery, connection, GATT setup, writes, and response waits. A timeout does not prove a control failed to execute.
- The current BLE probe defaults to a six-second discovery scan and a three-second timeout for each BLE operation and command response; these are client-side bounds, not measured device requirements. The probe can override them.
- Reads may be retried by a higher-level poller after reconnecting. Do not retry a mutating command automatically unless an acknowledgement/readback proves the prior attempt did not take effect.

## Versioning and evidence

Revision 1 is limited to the operations in the verified read table and the `ams` control. Firmware version `030000` is observed device identity, not the contract revision. Add a new contract revision when verified fields, command semantics, framing, or error behavior change. Keep unknowns marked as unknown until captured evidence resolves them.

Sanitized exact-device traces are recorded in the [protocol findings](protocol-findings.md). Rust command enums, wire-unit newtypes, parser negative cases, and captured-frame tests are the executable checks for the supported subset.
