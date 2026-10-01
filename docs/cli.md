# Command line reference

The `updraft` executable can connect directly to an original GAF Master Flow
Wi-Fi Attic Vent over Bluetooth, or act as a client of a running Updraft service.
Build instructions are in the [README](../README.md#install).

The examples assume `updraft` is on your `PATH`. From a source build, replace it
with `./target/release/updraft`. During development, use `cargo run --` inside
the devenv shell. Run `updraft --help` or a subcommand's `--help` for its arguments.

## Read through the service

```sh
updraft devices
updraft state configured
updraft devices --server http://UPDRAFT_HOST:8787 --format json
updraft state configured --server http://UPDRAFT_HOST:8787 --format json
```

Use an ID from `devices`. The original Bluetooth fan is `configured`; cloud IDs
start with `qc-`. These service IDs differ from Bluetooth peripheral IDs.

The server address is selected from `--server`, then `UPDRAFT_SERVER_URL`, then
`http://127.0.0.1:8787`. HTTP, HTTPS, and reverse-proxy path prefixes are supported.
Use a base address without `/api/v2`. Embedded credentials, query strings, and
fragments are rejected. TLS certificates are checked. The client does not follow
redirects, use proxy environment settings, or automatically retry requests.

`state` reads the service's cached snapshot. It does not request a new reading
from the fan. Inspect `available`, `inventory_status`, `last_error`, and
`state.provenance` for freshness. A response with `available: false` and
`state: null` is a successful read of an unavailable device.

Service client commands do not initialize Bluetooth or need QuickConnect
credentials. Account credentials belong on the service computer.

## Original fan controls

Through the running service:

```sh
updraft control configured preset automatic-105-f-30-percent
updraft control configured preset timer-one-minute
updraft control configured preset timer-clear
```

Directly over Bluetooth, with the peripheral ID from `ble scan`:

```sh
updraft ble control --device-id 'PERIPHERAL_ID' preset automatic-105-f-30-percent
updraft ble control --device-id 'PERIPHERAL_ID' preset timer-one-minute
updraft ble control --device-id 'PERIPHERAL_ID' preset timer-clear
```

| CLI preset | API value | Effect |
| --- | --- | --- |
| `automatic-105-f-30-percent` | `automatic105_f30_percent` | Set automatic mode, 105.0 °F, and 30.0% humidity |
| `automatic-105-1-f-30-1-percent` | `automatic105_1_f30_1_percent` | Set automatic mode, 105.1 °F, and 30.1% humidity |
| `timer-clear` | `timer_clear` | Clear the timer; leave the controller in timer mode |
| `timer-one-minute` | `timer_one_minute` | Start a one-minute timer |

Select an automatic preset to resume automatic operation after clearing a timer.
The presets are the values tested on the original controller, not recommended
attic settings. Normal controls do not accept arbitrary thresholds, longer
Bluetooth timers, or a separate on/off command.

## QuickConnect controls

These work through the service only, when the device advertises the command and
experimental cloud writes are enabled. Configure the account on the service as
described in [deployment](deployment.md#quickconnect-experimental).

Replace `CLOUD_DEVICE_ID` with an ID returned by `devices`:

```sh
updraft control CLOUD_DEVICE_ID mode automatic
updraft control CLOUD_DEVICE_ID targets --temperature-f 105 --humidity-percent 40
updraft control CLOUD_DEVICE_ID timer-duration 60
```

| Command | Accepted values |
| --- | --- |
| `mode` | `off`, `automatic`, `timer`, `manual` |
| `targets` | Both flags required: temperature 90–120 °F and humidity 30–80%, integers |
| `timer-duration` | 30–360 minutes in 30-minute steps |

Setting timer duration saves the duration; it does not activate timer mode or
report a remaining countdown. These limits come from the reference app's UI;
live model compatibility is still unverified.

## Direct Bluetooth reads

```sh
updraft ble scan
updraft ble scan --format json
updraft ble state
updraft ble state --device-id 'PERIPHERAL_ID' --format json
```

A scan reads advertisements without connecting. A state command connects and
queries the fan. Without an ID, `ble state` selects a fan only if discovery finds
one unambiguous candidate. Direct controls always require an explicit ID.

The scan defaults to six seconds. `--scan-seconds` changes it.
`--timeout-seconds` defaults to three seconds for each Bluetooth operation and
command response; connection setup and recovery have longer limits. Both values
must be positive integers. These options and `--format` work throughout the
`ble` command tree.

A partial read preserves decoded values, field errors, and disconnect errors.
Raw identity bytes are hidden unless you add `--show-identity`; those bytes may
contain a private identifier. The peripheral ID needed for selection is shown.
The original controller's `estimated_running` value stays unknown: its fan flag
is controller state, not a motor or airflow measurement.

## Timeouts and retries

Service discovery and state requests default to a ten-second deadline. Control
requests default to 300 seconds, allowing time for preparation, the write, and
readback. Connections have a five-second timeout. Use
`--timeout-seconds POSITIVE_INTEGER` on a service command to override its read
or control deadline.

A control gets a new UUID request ID by default. For an explicit ID:

```sh
updraft control configured preset timer-clear --request-id attic-1 --format json
```

Request IDs and service device IDs accept 1–64 ASCII letters, digits, underscores,
or hyphens. The CLI adds the current Unix timestamp immediately before submission.

A control is successful only when the HTTP status, returned request ID, and
backend's `confirmed` outcome all agree. A timeout means the outcome is unknown;
the service worker may continue after the CLI exits. Commands are submitted once.
Read current state before retrying.

Repeating an ID with the same command can return the running service's cached
result. Reusing an ID for different command content is rejected. The cache is
bounded and lost on restart, so it does not prevent duplicate writes across
service restarts. See the [HTTP API](http-api.md#controls) for details.

## Output and exit codes

Text is the default. `--format json` writes one JSON value followed by a newline
to stdout. Logs and text error diagnostics go to stderr. A closed stdout pipe
exits cleanly.

| Exit code | Meaning |
| --- | --- |
| 0 | Discovery/read completed, or control confirmed |
| 1 | Transport or response error, ambiguous/missing Bluetooth target, incomplete direct read, or rejected/unconfirmed control |
| 2 | Invalid arguments or input |

An empty scan or service inventory is successful discovery. Reading an unavailable
service device also exits 0. A direct read with malformed or missing fields
returns partial output and exits 1. A disconnect error remains in the output but
does not invalidate an otherwise complete read or confirmed control.

### Service JSON

Discovery and state use the [HTTP API shapes](http-api.md). Control includes
request ID, outcome, and HTTP status:

```json
{"request_id":"attic-1","status":"confirmed","http_status":200}
```

Execution errors use this envelope; unavailable context is `null`:

```json
{"error":{"kind":"timeout","message":"control outcome unknown for request attic-1: service request timed out","request_id":"attic-1","http_status":null}}
```

Kinds include `configuration`, `timeout`, `transport`, `http`, `response_too_large`,
`decoding`, `contract`, `correlation`, `unknown_device`, `unsupported_command`,
`clock`, and `ble`. Structured control outcomes are preserved for non-2xx
responses too. Responses are limited to two MiB. Additional response fields are
accepted; missing required fields or inconsistent values are rejected.

### Bluetooth JSON

The `status` field is `no_devices`, `discovered`, `ambiguous`,
`discovery_incomplete`, or `queried`. Discovery results include `devices`; a query
includes `device` and `query`. Each device has `peripheral_id`, nullable `name`,
and nullable `rssi_dbm`.

`query` contains nullable normalized `state`, `field_errors`, nullable `control`,
nullable `state_error`, `discovery_failures`, and a `disconnect` outcome. Control
includes acknowledgement and readback results; `confirmed` is true only if the
required checks passed. `identity_payload_hex` appears only with `--show-identity`.

## Diagnostic probe

The older probe commands remain available:

```sh
updraft probe ble --scan-only
updraft probe ble --device-id 'PERIPHERAL_ID' --set-auto-thresholds-tenths 1050 300
updraft probe ble --device-id 'PERIPHERAL_ID' --set-timer-minutes 1
```

The threshold values use tenths of °F and percent. This interface accepts raw
`u16` settings whose general hardware range is unknown. Use the normal presets
for routine control. It does not update firmware.

## Completions and Rust client

```sh
updraft completions bash > updraft.bash
updraft completions zsh > _updraft
updraft completions fish > updraft.fish
```

Elvish and PowerShell are also supported. Completion generation does not connect
to the network or Bluetooth.

Rust applications can use `updraft-client::Client`, `ServerUrl`, and
`ClientOptions`. `devices` and `state` return `updraft-api` models.
`prepare_control` checks capabilities and returns a `PreparedControl`; `submit`
sends it once. `ControlResult::Confirmed` is constructed only after HTTP status,
request correlation, and backend confirmation checks.
