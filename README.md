# Updraft

Updraft connects **GAF Master Flow Wi-Fi Attic Vent** fans to Home Assistant:

- **ERV5SMT** — roof mount.
- **EGV5SMT** — gable mount.

These are the original models that use the **GAF Wi-Fi Vent** app. Updraft talks
to their controller over Bluetooth, then provides temperature, humidity, fan
settings, and controls through a service on your network. You can also use the
command line without Home Assistant. No GAF account or Internet connection is
needed for these models.

The newer **Master Flow QuickConnect** models use a different app and cloud
service. Updraft includes an experimental backend for those; see
[fan models and compatibility](docs/hardware.md) before choosing a setup.

## What you need

- An installed ERV5SMT or EGV5SMT with Bluetooth firmware **3.0.0**. GAF added
  Bluetooth in that firmware version; older firmware needs the manufacturer's
  app to update it.
- A computer with Bluetooth within range of the fan. For an always-on service,
  use a Linux computer with BlueZ. macOS can run the Bluetooth command line too.
- Home Assistant, if you want its dashboard and automations.

Updraft connects directly over Bluetooth. You do not need to join the fan's
`GAFVent_XXXX` Wi-Fi network. It does not install or update fan firmware.

## Install

Build on the computer that will connect to the fan. Install
[Nix](https://nixos.org/download/) and [devenv](https://devenv.sh/getting-started/),
then run:

```sh
git clone https://github.com/mjc/updraft.git
cd updraft
devenv allow
devenv shell -- cargo build --release --features cli --locked
```

This builds the `updraft` service and the separate `updraftctl` control CLI. The
examples below use `target/release/updraft` and `target/release/updraftctl` from
the repository root. A service-only build can omit `--features cli`. On Linux, install and start BlueZ using your
distribution's package manager. On macOS, allow Bluetooth access if prompted.

## Find your fan

```sh
./target/release/updraftctl ble scan
```

Copy the fan's peripheral ID from the output, then read its state:

```sh
./target/release/updraftctl ble state --device-id 'PERIPHERAL_ID'
```

Replace `PERIPHERAL_ID` with the ID from the scan. It is a platform-specific
Bluetooth identifier, not the fan's model number. A successful read shows
temperature, humidity, mode, thresholds, and timer values. If no fan appears,
check Bluetooth, move the computer closer, and close the GAF app before retrying.

## Start the service

To let Home Assistant on another computer reach Updraft:

```sh
./target/release/updraft serve \
  --device-id 'PERIPHERAL_ID' \
  --identity-store /absolute/path/to/updraft-identities.json \
  --bind 0.0.0.0:8787 \
  --allow-remote
```

Replace the identity-store path with a private writable location outside the
checkout. Keep that file across restarts and upgrades; it stores the service
identity and device configuration.

Keep this process running. Allow port 8787 only from trusted computers; the HTTP
API has no login. For access beyond your trusted network, put it behind an
authenticated reverse proxy or a private network connection.

From the Home Assistant computer or another computer on the same network, check:

```sh
curl http://UPDRAFT_HOST:8787/api/v2/devices
curl http://UPDRAFT_HOST:8787/api/v2/devices/configured/state
```

Replace `UPDRAFT_HOST` with the address of the computer running Updraft. The
Bluetooth fan has the service device ID `configured`. Look for `available: true`
and current readings in the state response. The service polls every 30 seconds.

For automatic startup and logs, follow the [service setup guide](docs/deployment.md).

## Add it to Home Assistant

1. Copy this repository's `custom_components/updraft` directory into
   Home Assistant's configuration directory as `custom_components/updraft`.
   The resulting path should include `custom_components/updraft/manifest.json`.
2. Restart Home Assistant.
3. Open **Settings → Devices & services → Add integration** and search for
   **Updraft GAF Vent**.
4. Enter `http://UPDRAFT_HOST:8787`, replacing the host with your service's address.
   Use the base address without `/api/v2`. Replace any prefilled address.
5. Select the fans to add. Each selected fan gets its own integration entry.

Home Assistant exposes measurements, diagnostics, selectors, adjustable target
numbers, timer duration and refresh for the original controller. Numbers use
90–120 °F, 30–80%, and 0–360 timer minutes in whole-unit steps. The service reads
and preserves the unchanged target before writing. The existing verified
threshold and one-minute/clear presets remain available. A separate on/off
switch is not exposed for the original controller. The controller fan flag
reports controller state, without measuring airflow.

To change the API address, open the entry's menu and choose **Reconfigure**.
The new address must report the same persistent proxy UUID, device ID and
backend. Existing HA entity and device IDs are preserved.

MQTT discovery is an alternative to the HTTP integration. Use the
[Home Assistant and MQTT guide](docs/home-assistant-entities.md) if you prefer it.

## Use the command line

With the service running:

```sh
./target/release/updraftctl devices
./target/release/updraftctl state configured
./target/release/updraftctl control configured preset automatic-105-f-30-percent
```

For a remote service, add `--server http://UPDRAFT_HOST:8787`. The separate `updraftctl` binary can also control the fan directly over Bluetooth:

```sh
./target/release/updraftctl ble control --device-id 'PERIPHERAL_ID' preset timer-one-minute
```

Add `--format json` for scripts. See the [command line guide](docs/cli.md) for
all presets, QuickConnect commands, timeouts, and exit codes.

## More documentation

- [Fan models and compatibility](docs/hardware.md)
- [Run as a service; configure QuickConnect](docs/deployment.md)
- [Home Assistant entities and MQTT](docs/home-assistant-entities.md)
- [Command line reference](docs/cli.md)
- [HTTP API](docs/http-api.md)
- [Development and checks](docs/development.md)
- [Bluetooth protocol and captured device replies](docs/protocol-findings.md)
- [Bluetooth protocol contract](docs/protocol-contract-v1.md)
- [QuickConnect API research](docs/quickconnect-contract.md)
