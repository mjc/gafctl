# QuickConnect cloud API research

QuickConnect is the newer GAF Master Flow Wi-Fi controller, used in the ERV5QCT
and EGV5QCT product families. It has a separate cloud API from the original
ERV5SMT/EGV5SMT Bluetooth controller. See [fan models](hardware.md) for product
sources and [deployment](deployment.md#quickconnect-experimental) for configuration.

These notes record the API described by a community Home Assistant integration.
All repository fixtures are synthetic. There is no live account capture or
physical QuickConnect fan test in this repository.

## Reference source

- [GAFVentControl-HA](https://github.com/hitchin999/GAFVentControl-HA), revision
  `336adfd8d8cc0a936b4585bd20301f74d585554c`.
- Integration version: `1.1.0`.
- Its stated protocol source: Android app `com.gaf.quickconnectapp` 1.0.9.
- MIT attribution: [LICENSE-QUICKCONNECT-REFERENCE.txt](../LICENSE-QUICKCONNECT-REFERENCE.txt).
- Fixture provenance: [manifest.json](../fixtures/quickconnect/manifest.json).

## Authentication and endpoints

The reference uses `https://gaf-coreservices.aurai.io/cognito/` for authentication
and `https://gaf.keenhome.io/gaf/` for device requests, with a 20-second timeout.
Updraft uses those roots too; their live compatibility is unverified.

| Operation | Request | Reference behavior |
| --- | --- | --- |
| Login | POST `cognito/login` | Sends `userName`, Base64-encoded UTF-8 `password`, `userPoolId`, and `userRole`; reads `responseData.idToken`. |
| Inventory | GET `device/deviceList` | Accepts `responseData` as a list or an object containing a `devices` list. |
| Detail | GET `device?deviceId=<id>` | Reads `responseData`; merges detail over the inventory record. |
| Settings | POST `deviceMode/<id>` | Uses the write field names listed below. |
| Firmware information | GET `fw/fwInfo?deviceId=<id>` | Reads firmware diagnostics. Updraft does not expose firmware updates. |

The login pool is `us-east-2_F6aHzg32w`; roles are `contractor` (default) and
`consumer`. The ID token is sent literally in `Authorization`, without a
`Bearer ` prefix. The reference logs in again and retries once after a 401/403.
It does not establish a refresh-token grant or proactive renewal flow.

Unlike the reference's fallback to an empty inventory, Updraft reports malformed
inventory as an error. Read requests can retry transient failures and refresh
authentication. Settings writes are submitted once and are not automatically
retried after a timeout or authentication error.

## State fields

| Field | Interpretation in the reference |
| --- | --- |
| `deviceConfig.setTemperature` | Current temperature, °F |
| `deviceConfig.setHumidity` | Current relative humidity, % |
| `deviceSettings.setTemperature` | Temperature target, °F |
| `deviceSettings.setHumidity` | Humidity target, % |
| `deviceSettings.automaticMode` | Automatic-mode flag |
| `deviceSettings.timerMode` | Timer-mode flag |
| `deviceSettings.fanMode` | Manual-mode flag |
| `deviceSettings.timerValue` | Configured duration in minutes, not remaining time |
| `deviceSettings.humidityMonitor` | Readable setting; the reference says writes reject it |

The reference chooses a displayed mode by prioritizing truthy flags. Updraft
keeps missing and conflicting mode flags explicit. Its Running value is inferred
from mode and measurements and remains unknown when required data is missing.
It is not direct motor feedback.

## Write bodies

Read and write field names differ. The reference sends these field sets:

| Operation | Body fields | Values retained from current settings |
| --- | --- | --- |
| Set mode | `automaticMode`, `desiredTemp`, `desiredHumidity`, `timerMode`, `timerValue`, `fanMode` | Both targets and timer duration |
| Set automatic targets | `automaticMode`, `desiredTemp`, `desiredHumidity` | Automatic-mode flag |
| Set timer duration | `timerMode`, `timerValue` | Timer-mode flag |

For `(automaticMode, timerMode, fanMode)`, Off is `(false,false,false)`, Automatic
is `(true,false,false)`, Timer is `(false,true,false)`, and Manual is
`(false,false,true)`. Saving timer duration preserves whether timer mode is
active; it does not activate it.

The call sites send JSON integers. Although an API docstring mentions floats,
the number platform says the service rejects floats with HTTP 417 and service
status 4444. Updraft accepts integer temperature targets from 90–120 °F, humidity
from 30–80%, and timer durations from 30–360 minutes in 30-minute steps. These
bounds come from the reference UI, not tests of each physical model.

Updraft reads settings before preparing a write and checks them again afterward.
A submitted request with an ambiguous response is unconfirmed. A successful
response needs matching readback before the service reports `confirmed`.

## Fixtures and unresolved behavior

Every JSON file in [fixtures/quickconnect](../fixtures/quickconnect/) is generated
test data. Request examples store the body under a separate `body` key so fixture
metadata is not sent to the service. The fixtures cover login, inventory shapes,
state fields, request bodies, malformed data, and a rejected settings response.
No fixture records a real successful settings response.

Live account/device evidence is still needed for:

- The accepted-write envelope and application status fields.
- Whether the endpoints still accept the pinned reference's requests.
- Identifier aliases returned by different models and accepted by write routes.
- Model-specific units, ranges, and preservation of omitted settings.
- Device timestamps, cache age, signal-strength units, offline behavior, and the
  meaning of `isVerified`.
- Timer activation, expiry, and readback timing.

Cloud writes stay disabled by default because these questions are unresolved.
Keep passwords, account/device identifiers, tokens, and unredacted traffic out
of test fixtures.
