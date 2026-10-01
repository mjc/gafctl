# Command line interface

The `updraft` executable can access a fan directly over Bluetooth or act as an
HTTP client of a running Updraft service. Run `updraft --help` and any command's
`--help` for the full argument reference.

For development, use the repository's pinned environment:

```sh
devenv allow
devenv shell -- cargo run -- devices --format json
```

## Running service

```sh
updraft devices
updraft devices --server https://fan.example/updraft --format json
updraft state configured --format json
updraft control configured preset timer-clear
updraft control configured preset timer-one-minute --request-id attic-timer-1
updraft control qc-local mode automatic
updraft control qc-local targets --temperature-f 105 --humidity-percent 40
updraft control qc-local timer-duration 60
```

Use the local device ID returned by `devices`. A local ID contains 1–64 ASCII
letters, digits, underscores, or hyphens. It is distinct from a BLE peripheral ID
and a cloud provider's private ID. State and control require an explicit local
ID. The client checks the inventory and advertised capabilities before reading
state or submitting control. The service checks them again and owns execution,
write gates, serialization, and replay handling.

The URL is chosen in this order:

1. `--server URL`
2. `UPDRAFT_SERVER_URL`
3. `http://127.0.0.1:8787`

HTTP and HTTPS are supported, including reverse-proxy prefixes such as
`https://fan.example/updraft`. Credentials, query strings, and fragments in the
base URL are rejected. TLS certificates are verified. The client disables
redirects, proxies, and automatic retries. It never falls back to BLE when a
service request fails. Client commands require no QuickConnect account secrets
and do not initialize Bluetooth.

The service has no built-in authentication. Use the existing protected HTTP or
HTTPS deployment; see [deployment](deployment.md). HA `state_source` and
`command_source` describe entity ownership and do not gate administrative CLI
access.

`state` returns the service's cached snapshot. Inspect `available`,
`inventory_status`, `last_error`, and `state.provenance` observation/fetch
timestamps. An unavailable device with `state: null` is a valid read. This GET
does not start a fresh physical fan query.

### Controls

The friendly legacy preset names map to the existing API wire values:

| CLI preset | API value | Requested setting |
| --- | --- | --- |
| `automatic-105-f-30-percent` | `automatic105_f30_percent` | Automatic, 105.0 °F and 30.0% |
| `automatic-105-1-f-30-1-percent` | `automatic105_1_f30_1_percent` | Automatic, 105.1 °F and 30.1% |
| `timer-clear` | `timer_clear` | Clear the timer |
| `timer-one-minute` | `timer_one_minute` | One-minute timer |

QuickConnect controls are available only when the selected device advertises
them and the service's write gate admits them:

- `mode`: `off`, `automatic`, `timer`, or `manual`.
- `targets`: both `--temperature-f` and `--humidity-percent` are required.
  Temperature must be an integer from 90–120 °F; humidity must be an integer from
  30–80%.
- `timer-duration`: 30–360 minutes in 30-minute steps. This sets configured
  duration, not a remaining countdown.

The CLI rejects invalid arguments before accessing the transport. Backend
validation remains authoritative.

### Deadlines and request IDs

The connection timeout is five seconds. Total discovery/state request deadlines
default to ten seconds; control defaults to 300 seconds to allow the backend's
preparation, write, and readback phases. `--timeout-seconds POSITIVE_INTEGER`
overrides read and control deadlines. It is a client deadline, not a service
completion guarantee.

Control uses a new UUID by default. `--request-id ID` accepts the same 1–64 ASCII
letter/digit/underscore/hyphen syntax as local IDs. The Unix-millisecond timestamp
is generated immediately before the POST, after capability discovery. Global
options within a command tree work before or after its subcommand, for example:

```sh
updraft control configured --format json preset timer-clear --request-id attic-1
updraft control configured preset timer-clear --format json --request-id attic-1
```

Success requires a matching request ID, a successful HTTP status, and the
backend's `confirmed` outcome. Acknowledgement alone is insufficient. Unknown
future backend outcomes are retained and count as unconfirmed.

A timeout or lost control response means the outcome is unknown. The output
retains the request ID; the worker may continue after the CLI exits. Controls
are sent once and never automatically retried. Reusing an ID relies on the
running service's bounded, in-memory replay cache. It does not provide durable
idempotency across service restarts. The service also rejects reuse with
different command content. Inspect state and the outcome before deciding
whether to retry.

## Direct Bluetooth

```sh
updraft ble scan --format json
updraft ble state
updraft ble state --device-id <peripheral-id> --format json
updraft ble control --device-id <peripheral-id> preset timer-clear --format json
updraft ble control --device-id <peripheral-id> preset automatic-105-1-f-30-1-percent
```

`scan` discovers advertisements without connecting or sending protocol requests.
`state` performs a fresh direct device query. It auto-selects only one
unambiguous candidate under the existing discovery rules. With multiple
candidates, supply the platform peripheral ID returned by the scan. Direct
`control` always requires this ID and exposes exactly the four verified presets
listed above. It uses the existing transport, identity validation, and readback
confirmation logic.

BLE discovery defaults to six seconds (`--scan-seconds`). Each BLE operation and
command response defaults to three seconds (`--timeout-seconds`). Both options
require positive integers; connection setup and recovery retain the existing
transport-specific limits. These flags and `--format` work within the BLE
command tree, including after `preset`.

Partial snapshots retain successfully decoded fields, nullable values, field
errors, control acknowledgement/readback, discovery warnings, and disconnect
failures. A write with missing or mismatched readback exits unsuccessfully.
The `controller_fan_on` flag is reported controller state and does not prove
motor operation or airflow. There is no verified standalone legacy on/off
command. `estimated_running` stays unknown for legacy BLE.

Raw identity payload bytes are omitted from both text and JSON. Supply
`--show-identity` to include `identity_payload_hex`; it may contain a private
device identifier. Peripheral IDs needed for selection are shown by direct BLE
commands. Service output uses service-local IDs.

The original diagnostic interface remains compatible:

```sh
updraft probe ble --scan-only
updraft probe ble --device-id <peripheral-id> --set-auto-thresholds-tenths 1050 300
updraft probe ble --device-id <peripheral-id> --set-timer-minutes 1
```

Diagnostic controls accept the existing raw `u16` values. That interface does
not establish verified hardware limits for arbitrary settings; use the normal
verified presets for routine BLE control. `serve` retains its existing flags
and environment variables.

## Output and exit codes

`--format text` is the default. `--format json` emits one complete JSON value
and a newline on stdout. Tracing logs and text error diagnostics go to stderr.
Usage errors from clap go to stderr. Closed stdout pipes exit cleanly.

| Exit | Meaning |
| --- | --- |
| 0 | Completed discovery/read, or confirmed control |
| 1 | Execution, transport, or contract error; missing/ambiguous direct query target; rejected or unconfirmed control |
| 2 | Invalid command line or input |

An empty scan or device inventory is successful discovery. A service state
result with `available: false` is a successful read. Direct reads with malformed
fields or a missing snapshot return partial output with exit 1. A disconnect
failure remains in the output without invalidating an otherwise complete
snapshot or confirmed control.

### JSON contract

Service discovery and state use the [v2 API response shapes](http-api.md).
Service control retains `request_id` and `status`, adding the HTTP status:

```json
{"request_id":"attic-1","status":"confirmed","http_status":200}
```

Structured control outcomes are preserved even for non-2xx HTTP responses.
Other execution failures use this envelope; absent context is explicit null:

```json
{"error":{"kind":"timeout","message":"control outcome unknown for request attic-1: service request timed out","request_id":"attic-1","http_status":null}}
```

Error kinds include `configuration`, `timeout`, `transport`, `http`,
`response_too_large`, `decoding`, `contract`, `correlation`, `unknown_device`,
`unsupported_command`, `clock`, and `ble`. HTTP response bodies are limited to
two MiB, including declared or streamed error bodies. Incomplete or malformed
contracts are errors. Additional response fields are accepted; missing required
fields, invalid identities, and inconsistent state are rejected.

Direct BLE uses a `status` discriminator:

- `no_devices`
- `discovered`, with `devices`
- `ambiguous`, with `devices`
- `discovery_incomplete`, with `devices` and `failures`
- `queried`, with `device` and `query`

Each device contains `peripheral_id`, nullable `name`, and nullable `rssi_dbm`.
Discovery failures contain `peripheral_id` and `message`. A `query` contains:

- Nullable `state` using normalized v2 settings, diagnostics, and provenance.
- `field_errors` with `field` and `message`.
- Nullable `control` with `confirmed`, `acknowledgement` (`accepted` or
  `unrecognized`), `readback`, and `mode_readback`. Each readback has `status`
  and `message`; statuses are `matches`, `differs`, `decode_error`, `unavailable`,
  `fan_flag_differs`, or `unverified_timer_expiry` as applicable.
- Nullable `state_error` and an array of `discovery_failures`.
- `disconnect` with `status: disconnected`, or `status: failed` and `message`.
- `identity_payload_hex` only with the identity opt-in.

## Completions and reusable Rust client

```sh
updraft completions bash > updraft.bash
updraft completions zsh > _updraft
updraft completions fish > updraft.fish
```

Completions are generated from the same clap command definition without network
or Bluetooth initialization. Elvish and PowerShell are also supported.

Rust callers can use `updraft-client::Client` with `ServerUrl` and
`ClientOptions`. `devices` and `state` return shared `updraft-api` models.
`prepare_control` resolves capabilities and returns a `PreparedControl` intent;
consuming `submit` sends it once. A `ControlResult::Confirmed` contains a private
`ConfirmedControl` value constructed only after HTTP, correlation, and backend
confirmation checks. Libraries do not depend on clap, MQTT, the BLE runtime,
or service handlers.

Automated tests use local HTTP servers and the existing fake BLE transports.
They establish software behavior; this change adds no physical device acceptance
claim.
