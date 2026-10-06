# Command line interface

`gafctl` reads and controls fans over Bluetooth or through a running service's
HTTP API. Run `gafctl --help`
and any command's `--help` for the full argument reference. The `gafctl`
command `server` launches `gafctl-server` from the same directory, falling back
to `PATH`. It forwards all arguments after `server`, including `--help`.
On Unix, the server replaces the CLI process and receives signals directly.

For development, use the repository's pinned environment:

```sh
devenv allow
devenv shell -- cargo run --no-default-features --features cli --bin gafctl -- devices --format json
```

Cargo features default to `http`, `mqtt`, and `cli`, building both executables. `mqtt` also enables `http`.
Build the service without MQTT with `--no-default-features --features http`, or
build only the control CLI with `--no-default-features --features cli`.

## Start the server

```sh
gafctl server --bind 127.0.0.1:8787
gafctl-server --bind 127.0.0.1:8787
```

Both commands start the same server. Install `gafctl-server` alongside the CLI or on `PATH`.
A CLI-only build can launch a separately installed server.

## Running service

```sh
gafctl devices
gafctl devices --server https://fan.example/gafctl --format json
gafctl state configured --format json
gafctl control configured preset timer-clear
gafctl control configured preset timer-one-minute --request-id attic-timer-1
gafctl control qc-local mode automatic
gafctl control qc-local targets --temperature-f 105 --humidity-percent 40
gafctl control qc-local timer-duration 60
```

Use the local device ID returned by `devices`. A local ID contains 1–64 ASCII
letters, digits, underscores, or hyphens. Use this service-local ID for state
and control. The client and service check inventory and capabilities before
execution.

The URL is chosen in this order:

1. `--server URL`
2. `GAFCTL_SERVER_URL`
3. `http://127.0.0.1:8787`

HTTP and HTTPS are supported, including reverse-proxy prefixes such as
`https://fan.example/gafctl`. Credentials, query strings, and fragments in the
base URL are rejected. TLS certificates are verified. The client disables
redirects, proxies and automatic retries. Service commands use HTTP exclusively;
account credentials and Bluetooth configuration belong to the server.

The service has no built-in authentication. Configure protected HTTP or HTTPS
access as described in [deployment](deployment.md). HA `state_source` and
`command_source` select HA entities. Administrative HTTP and MQTT commands stay
available.

`state` returns the service's cached snapshot. Inspect `available`,
`inventory_status`, `last_error`, and `state.provenance` observation/fetch
timestamps. An unavailable device with `state: null` is a valid read. Use the
[refresh endpoint](http-api.md#refresh) for a new backend reading.

### Controls

CLI preset names map to API values:

| CLI preset | API value | Requested setting |
| --- | --- | --- |
| `automatic-105-f-30-percent` | `automatic105_f30_percent` | Automatic, 105.0 °F and 30.0% |
| `automatic-105-1-f-30-1-percent` | `automatic105_1_f30_1_percent` | Automatic, 105.1 °F and 30.1% |
| `timer-clear` | `timer_clear` | Clear the timer |
| `timer-one-minute` | `timer_one_minute` | One-minute timer |

QuickConnect controls require advertised capabilities and enabled cloud writes:

- `mode`: `off`, `automatic`, `timer`, or `manual`.
- `targets`: both `--temperature-f` and `--humidity-percent` are required.
  Temperature must be an integer from 90–120 °F; humidity must be an integer from
  30–80%.
- `timer-duration`: 30–360 minutes in 30-minute steps. This sets the configured
  duration; saving it leaves timer mode unchanged.

The CLI rejects invalid arguments before accessing the transport. Backend
validation also runs for each command.

### Deadlines and request IDs

The connection timeout is five seconds. Total discovery/state request deadlines
default to ten seconds; control defaults to 300 seconds to allow the backend's
settings read, write and readback. `--timeout-seconds POSITIVE_INTEGER`
overrides read and control deadlines. The service may continue after the client
deadline expires.

Control uses a new UUID by default. `--request-id ID` accepts the same 1–64 ASCII
letter/digit/underscore/hyphen syntax as local IDs. The Unix-millisecond timestamp
is generated immediately before the POST, after capability discovery. Global
options within a command tree work before or after its subcommand, for example:

```sh
gafctl control configured --format json preset timer-clear --request-id attic-1
gafctl control configured preset timer-clear --format json --request-id attic-1
```

Success requires a matching request ID, a successful HTTP status, and the
backend's `confirmed` outcome. Other outcomes are unconfirmed.

A timeout or lost control response means the outcome is unknown. The output
retains the request ID; the worker may continue after the CLI exits. Controls
are sent once and never automatically retried. Reusing an ID relies on the
running service's in-memory replay cache, which resets on restart. Reuse with
different command content is rejected. Read current state before retrying.

## Direct Bluetooth

```sh
gafctl ble scan --format json
gafctl ble state
gafctl ble state --device-id <peripheral-id> --format json
gafctl ble control --device-id <peripheral-id> preset timer-clear --format json
gafctl ble control --device-id <peripheral-id> preset automatic-105-1-f-30-1-percent
```

`scan` discovers advertisements without connecting or sending protocol requests.
`state` reads the device and selects a single unambiguous candidate. With
multiple candidates, supply the peripheral ID returned by the scan. Direct
`control` requires this ID and supports the four tested presets above. It checks
identity, acknowledgement and readback.

BLE discovery defaults to five seconds (`--scan-seconds`). GATT setup, each command
write, and each response wait default to three seconds (`--timeout-seconds`).
Platform adapter setup, scanning, connection, and cleanup allow at least 40
seconds for operating-system calls. Both options require positive integers;
the [recovery limits](protocol-findings.md#timeouts-and-recovery) also apply.
These flags and `--format` work within the BLE
command tree, including after `preset`.

Partial snapshots retain successfully decoded fields, nullable values, field
errors, control acknowledgement/readback, discovery warnings, and disconnect
failures. A write with missing or mismatched readback exits unsuccessfully.
The `controller_fan_on` flag reports the controller's on/off state; motor
operation and airflow are unmeasured. `estimated_running` is null for legacy BLE.
Use timer or automatic controls to change operation.

Raw identity payload bytes are omitted from both text and JSON. Supply
`--show-identity` to include `identity_payload_hex`; it may contain a private
device identifier. Peripheral IDs needed for selection are shown by direct BLE
commands. Service output uses service-local IDs.

The server executable also exposes Bluetooth diagnostics:

```sh
gafctl-server probe ble --scan-only
gafctl-server probe ble --device-id <peripheral-id> --set-auto-thresholds-tenths 1050 300
gafctl-server probe ble --device-id <peripheral-id> --set-timer-minutes 1
```

Diagnostic controls accept raw `u16` values. The controller's full range has not
been tested. Use the four tested presets for direct BLE control. Server options
use the `GAFCTL_` environment variables in the service guide.

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
Other execution failures use this envelope; missing context is `null`:

```json
{"error":{"kind":"timeout","message":"control outcome unknown for request attic-1: service request timed out","request_id":"attic-1","http_status":null}}
```

Error kinds are `configuration`, `timeout`, `transport`, `http`,
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
gafctl completions bash > gafctl.bash
gafctl completions zsh > _gafctl
gafctl completions fish > gafctl.fish
```

Completions are generated from the same clap command definition without network
or Bluetooth initialization. Elvish and PowerShell are also supported.

Rust callers can use `gafctl-client::Client` with `ServerUrl` and
`ClientOptions`. `devices` and `state` return shared `gafctl-api` models.
`prepare_control` resolves capabilities and returns a `PreparedControl` intent;
consuming `submit` sends it once. A `ControlResult::Confirmed` contains a private
`ConfirmedControl` value constructed after HTTP, correlation and backend
confirmation checks.

Automated tests check software behavior with local HTTP servers and fake BLE
transports. Hardware test results are in the [protocol findings](protocol-findings.md).
