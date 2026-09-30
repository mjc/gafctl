# Local deployment

Run Updraft on a Linux host that can reach the fan over Bluetooth. For this
setup, Updraft runs on Tali and Home Assistant runs on Tina on the same LAN.
The API defaults to loopback; `--allow-remote` enables LAN polling when needed.
The optional MQTT publisher connects outbound to the broker on Tina, so HA can
receive retained state and discover entities without connecting to Updraft.

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
UPDRAFT_DEVICE_ID=DEVICE_ID \
UPDRAFT_MQTT_HOST=192.168.1.5 \
UPDRAFT_MQTT_PORT=1883 \
UPDRAFT_MQTT_USERNAME=updraft \
target/release/updraft serve --bind 0.0.0.0:8787 --allow-remote
```

The MQTT password must also be loaded from a protected credential file into
`UPDRAFT_MQTT_PASSWORD` by the service manager. Do not put it in the command
line or a tracked environment file.

The device identifier is local configuration, not an authentication secret.
Updraft does not use or store OEM account credentials. Keep the identifier out
of tracked files and logs. Set it with `UPDRAFT_DEVICE_ID` or `--device-id`;
the API does not expose it.

The process writes structured JSON logs to standard error. The default filter
shows Updraft info events. Set `RUST_LOG=updraft=debug` to include debug events.

## Check state

Check that the API process is responding:

```sh
curl --fail http://tali.local:8787/health
```

`/health` checks the HTTP process only. Check
`http://tali.local:8787/api/v1/devices/configured/state` for Bluetooth
availability, state freshness, and the last query error. The Home Assistant
integration reads the endpoint directly; the MQTT publisher also sends retained
state, availability, diagnostics, and discovery messages when configured.

## Upgrade and recover

Stop the running process before replacing the executable. Rebuild from the
selected repository revision with the locked command above, then restart it.
To roll back, rebuild and run the last known-good revision. Updraft stores no
device state on disk; Home Assistant keeps its integration configuration.

The NixOS configuration provides the Tali system service and opens TCP 8787
only on Tali's wired LAN interface.
