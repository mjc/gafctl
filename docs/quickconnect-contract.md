# GAF Vent Control cloud contract notes

This document records behavior in the pinned Python Home Assistant reference, synthetic examples for tests, and unresolved questions separately. It does not claim a live cloud capture or hardware validation.

## Reference

- Repository: [GAFVentControl-HA](https://github.com/hitchin999/GAFVentControl-HA)
- Inspected revision: `336adfd8d8cc0a936b4585bd20301f74d585554c`
- Integration manifest version: `1.1.0`
- Protocol source described by the integration: Android app `com.gaf.quickconnectapp` 1.0.9
- License: MIT; attribution and permission text is in [`LICENSE-QUICKCONNECT-REFERENCE.txt`](../LICENSE-QUICKCONNECT-REFERENCE.txt).
- Fixture evidence: every JSON file under [`fixtures/quickconnect`](../fixtures/quickconnect/) is synthetic. None came from an account, live service, or physical device.
- The integration describes a cloud API contract; source inspection alone does not prove any particular physical device is compatible.

## Endpoints and authentication described by the source

The reference uses `https://gaf-coreservices.aurai.io/cognito/` for login and `https://gaf.keenhome.io/gaf/` for device requests. Its request timeout is 20 seconds.

| Operation | Request | Behavior in the reference |
| --- | --- | --- |
| Login | POST `cognito/login`; `userName`, Base64 of UTF-8 `password`, `userPoolId`, `userRole` | Reads `responseData.idToken`. Username is trimmed. Pool is `us-east-2_F6aHzg32w`; role defaults to `contractor`, with `consumer` accepted in config flow. |
| Inventory | GET `device/deviceList` | Accepts `responseData` as a list or an object with a `devices` list. Other shapes become an empty list in the reference; Updraft must retain the error/unknown distinction. |
| Detail | GET `device?deviceId=<id>` | Reads `responseData`. List records are enriched by detail. The reference merges detail over list, retaining the list record after a detail error. |
| Write settings | POST `deviceMode/<id>` | The body field names differ from state-read names; use only the typed bodies below. |
| Firmware information | GET `fw/fwInfo?deviceId=<id>` | Reads firmware diagnostics. Firmware-update methods are out of scope; the reference's button triggers an update. |

Device requests pass the returned ID token literally as the `Authorization` header value, without `Bearer `. On 401/403, the reference logs in again and retries once. The source does not establish refresh-token grant behavior or proactive token renewal.

## Read values and write values

| Field | Meaning in the Python entity code |
| --- | --- |
| `deviceConfig.setTemperature` | Current temperature reading, °F |
| `deviceConfig.setHumidity` | Current relative-humidity reading, % |
| `deviceSettings.setTemperature` | Temperature target, °F |
| `deviceSettings.setHumidity` | Humidity target, % |
| `deviceSettings.automaticMode`, `timerMode`, `fanMode` | Automatic, Timer, Manual mode flags |
| `deviceSettings.timerValue` | Configured timer duration, minutes; not a remaining-time observation |
| `deviceSettings.humidityMonitor` | Read by the integration; the reference says the settings endpoint rejects it in a write body |

Source call sites produce numeric JSON integers. The API method docstring says temperature/humidity values are floats, but the number platform says the service rejects float JSON with HTTP 417 and service `statusCode` 4444. Candidate app bounds are temperature 90–120 °F in 1-degree steps, humidity 30–80% in 1-point steps, timer duration 30–360 minutes in 30-minute steps. These are app UI limits, not verified model limits.

| Write operation | Exact field set | Preservation rule |
| --- | --- | --- |
| Set mode | `{automaticMode, desiredTemp, desiredHumidity, timerMode, timerValue, fanMode}` | Use exclusive mode flags; preserve current validated thresholds and duration. |
| Save automatic targets | `{automaticMode, desiredTemp, desiredHumidity}` | Preserve current mode and the unmodified target. |
| Save timer duration | `{timerMode, timerValue}` | Preserve current timer-mode flag. Setting duration does not activate the timer. |

For `(automaticMode, timerMode, fanMode)`, the integration maps Off to `(false,false,false)`, Automatic to `(true,false,false)`, Timer to `(false,true,false)`, and Manual to `(false,false,true)`. Its display selection prioritizes truthy flags; Updraft should represent missing/conflicting flags as unknown instead of assuming that precedence is authoritative.

The Python integration estimates “Running” from mode and threshold readings. It is not direct motor or airflow feedback. Updraft should mark such a state as an estimate and preserve unknown when required readings are absent or stale.

## Synthetic fixture inventory

[`manifest.json`](../fixtures/quickconnect/manifest.json) pins provenance, marks fixture data synthetic, and lists cases checked by `tests/quickconnect_contract.rs`. Request-body fixtures use a separate `body` object, so evidence metadata cannot be confused with fields sent on the wire. The settings-success envelope remains unknown; no fixture represents it.

## Unresolved contract questions

- Actual response envelope and application status fields for accepted settings writes.
- Whether HTTP 200 with missing or nonstandard JSON can occur in production.
- Which identifier aliases each model returns and which identifier the detail/write routes accept; synthetic IDs do not resolve this.
- Actual temperature/RH/time encodings and model-specific write ranges.
- Device observation timestamps, cloud cache age, `isVerified`, signal-strength units, and reliable offline semantics.
- Whether partial writes preserve every omitted setting across supported firmware versions.
- Timer countdown, activation, expiry, and readback behavior.
- Whether the cloud endpoints still accept the contract at the pinned reference revision.

Resolve these with authorized account/device evidence before enabling cloud writes. Never store credentials, real provider/account identifiers, tokens, or unredacted traffic in fixtures.
