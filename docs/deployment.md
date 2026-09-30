# Local deployment

Run Updraft on the host that runs Home Assistant and can reach the fan over
Bluetooth. Home Assistant and Updraft must share a network namespace: the API
binds to loopback and does not accept remote connections.

## Build

Build with the repository's pinned Rust toolchain and locked dependencies:

```sh
devenv allow
devenv shell -- cargo build --release --locked
```

The executable is `target/release/updraft`.

## Run

Get the fan's BLE identifier with `updraft probe ble --scan-only`, then start
the API:

```sh
target/release/updraft serve --device-id DEVICE_ID
```

The device identifier is local configuration, not an authentication secret.
Updraft does not use or store OEM account credentials. Keep the identifier out
of tracked files and logs. The API does not expose it.

The process writes structured JSON logs to standard error. The default filter
shows Updraft info events. Set `RUST_LOG=updraft=debug` to include debug events.

## Check state

Check that the API process is responding:

```sh
curl --fail http://127.0.0.1:8787/health
```

`/health` checks the HTTP process only. Check
`http://127.0.0.1:8787/api/v1/devices/configured/state` for Bluetooth
availability, state freshness, and the last query error. Home Assistant reads
the same state endpoint through the Updraft integration.

## Upgrade and recover

Stop the running process before replacing the executable. Rebuild from the
selected repository revision with the locked command above, then restart it.
To roll back, rebuild and run the last known-good revision. Updraft stores no
device state on disk; Home Assistant keeps its integration configuration.

The repository does not yet include a host service definition. The service
manager and Bluetooth permissions depend on the deployment host.
