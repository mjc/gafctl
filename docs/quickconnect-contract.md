# QuickConnect API

QuickConnect controllers use GAF's cloud API. See [fan models](hardware.md#quickconnect)
and [service setup](deployment.md#quickconnect-experimental).

This backend is experimental. Its implementation follows
[GAFVentControl-HA](https://github.com/hitchin999/GAFVentControl-HA), revision
`336adfd8d8cc0a936b4585bd20301f74d585554c`, version 1.1.0, which was based on
Android app `com.gaf.quickconnectapp` 1.0.9. Repository fixtures are synthetic; live
account and hardware compatibility are untested. Attribution is in
[LICENSE-QUICKCONNECT-REFERENCE.txt](../LICENSE-QUICKCONNECT-REFERENCE.txt).

## Authentication and endpoints

Authentication root: `https://gaf-coreservices.aurai.io/cognito/`.
Device root: `https://gaf.keenhome.io/gaf/`. The reference timeout is 20 seconds.

| Operation | Request | Reference behavior |
| --- | --- | --- |
| Login | POST `cognito/login` | Send `userName`, Base64-encoded UTF-8 `password`, `userPoolId`, `userRole`; read `responseData.idToken`. |
| Inventory | GET `device/deviceList` | Read `responseData` as a list or an object with a `devices` list. |
| Detail | GET `device?deviceId=<id>` | Merge `responseData` over the inventory record. |
| Settings | POST `deviceMode/<id>` | Send the write fields below. |
| Firmware information | GET `fw/fwInfo?deviceId=<id>` | Read firmware diagnostics. |

The login pool is `us-east-2_F6aHzg32w`. Roles are `contractor` (default) and
`consumer`. Send the ID token in `Authorization` without a `Bearer ` prefix.

Gafctl rejects malformed inventory. Reads can retry transient failures and
refresh authentication. Settings writes have no automatic retries. Firmware
updates are unsupported.

## State fields

| Field | Meaning |
| --- | --- |
| `deviceConfig.setTemperature` | Ambient temperature, °F |
| `deviceConfig.setHumidity` | Ambient relative humidity, % |
| `deviceSettings.setTemperature` | Temperature target, °F |
| `deviceSettings.setHumidity` | Humidity target, % |
| `deviceSettings.automaticMode` | Automatic-mode flag |
| `deviceSettings.timerMode` | Timer-mode flag |
| `deviceSettings.fanMode` | Manual-mode flag |
| `deviceSettings.timerValue` | Configured duration, minutes |
| `deviceSettings.humidityMonitor` | Readable setting; the reference reports rejected writes |

Gafctl reports missing or conflicting mode flags as unknown. Running is
estimated from mode and measurements and is unknown when required data is
missing. Motor operation is unmeasured.

## Write bodies

| Operation | Body fields | Preserved settings |
| --- | --- | --- |
| Set mode | `automaticMode`, `desiredTemp`, `desiredHumidity`, `timerMode`, `timerValue`, `fanMode` | Both targets and duration |
| Set targets | `automaticMode`, `desiredTemp`, `desiredHumidity` | Automatic-mode flag |
| Set duration | `timerMode`, `timerValue` | Timer-mode flag |

Mode flags `(automaticMode, timerMode, fanMode)`:

| Mode | Flags |
| --- | --- |
| Off | `(false,false,false)` |
| Automatic | `(true,false,false)` |
| Timer | `(false,true,false)` |
| Manual | `(false,false,true)` |

Writes use integers: 90–120 °F, 30–80% humidity, and 30–360 timer minutes in
30-minute steps. These ranges come from the reference UI. It reports rejected
float writes with HTTP 417 and application status 4444. Saving duration leaves
timer mode unchanged.

Gafctl reads settings before a write and checks readback afterward. An ambiguous
response is unconfirmed; `confirmed` requires a successful response and matching
readback. Writes are disabled by default.

## Fixtures

[fixtures/quickconnect](../fixtures/quickconnect/) contains generated request and
response data. Its [manifest](../fixtures/quickconnect/manifest.json) records
provenance. Request bodies are nested under `body` to separate them from fixture
metadata.

The fixtures cover login, inventory, state, request bodies and rejected
settings. A real accepted-write envelope, identifier aliases, model-specific
units, timestamps, offline behavior and timer timing are unverified.
