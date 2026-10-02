# gafctl

Gafctl connects **GAF Master Flow Wi-Fi Attic Vent** fans to Home Assistant:

- **ERV5SMT** — roof mount.
- **EGV5SMT** — gable mount.

These models use the **GAF Wi-Fi Vent** app. Gafctl reads and controls them over
Bluetooth. Its HTTP API and MQTT bridge provide temperature, humidity, settings,
and controls to Home Assistant. The CLI can also read and control a fan directly.
These models require no GAF account or Internet connection.

The newer **Master Flow QuickConnect** models use a different app and cloud
service. Their backend is experimental; see
[fan models and compatibility](docs/hardware.md).

## What you need

- An installed ERV5SMT or EGV5SMT with Bluetooth firmware **3.0.0**. GAF added
  Bluetooth in that firmware version; older firmware needs the manufacturer's
  app to update it.
- A computer with Bluetooth within range of the fan. For an always-on service,
  use a Linux computer with BlueZ. macOS can run the Bluetooth command line too.
- Home Assistant, if you want its dashboard and automations.

Gafctl uses Bluetooth and leaves the computer on its normal network. Fan
firmware updates require the manufacturer's app.

## Install

Build on the computer that will connect to the fan. Install
[Nix](https://nixos.org/download/) and [devenv](https://devenv.sh/getting-started/),
then run:

```sh
git clone https://github.com/mjc/gafctl.git
cd gafctl
devenv allow
devenv shell -- cargo build --release --locked
```

The build produces `gafctl` and `gafctl-server`. `gafctl server` launches
`gafctl-server` and forwards its arguments. Run the examples from the repository
root. For a service-only build, add
`--no-default-features --features http,mqtt --bin gafctl-server`.
On Linux, install and start BlueZ using your
distribution's package manager. On macOS, allow Bluetooth access if prompted.

## Find your fan

```sh
./target/release/gafctl ble scan
```

Copy the fan's peripheral ID from the output, then read its state:

```sh
./target/release/gafctl ble state --device-id 'PERIPHERAL_ID'
```

Replace `PERIPHERAL_ID` with the platform-specific Bluetooth ID from the scan.
A successful read shows temperature, humidity, mode, thresholds, and timer values.
If no fan appears,
check Bluetooth, move the computer closer, and close the GAF app before retrying.

## Start the service

To let Home Assistant on another computer reach Gafctl:

```sh
./target/release/gafctl server \
  --device-id 'PERIPHERAL_ID' \
  --identity-store /absolute/path/to/gafctl-identities.json \
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
curl http://GAFCTL_HOST:8787/api/v2/devices
curl http://GAFCTL_HOST:8787/api/v2/devices/configured/state
```

Replace `GAFCTL_HOST` with the address of the computer running Gafctl. The
Bluetooth fan has the service device ID `configured`. Look for `available: true`
and current readings in the state response. The service polls every 30 seconds.

For automatic startup and logs, follow the [service setup guide](docs/deployment.md).

## Add it to Home Assistant

1. Copy this repository's `custom_components/gafctl` directory into
   Home Assistant's configuration directory as `custom_components/gafctl`.
   The resulting path should include `custom_components/gafctl/manifest.json`.
2. Restart Home Assistant.
3. Open **Settings → Devices & services → Add integration** and search for
   **Gafctl GAF Vent**.
4. Enter `http://GAFCTL_HOST:8787`, replacing the host with your service's address.
   Use the base address without `/api/v2`. Replace any prefilled address.
5. Select the fans to add. Each selected fan gets its own integration entry.

Home Assistant exposes measurements, diagnostics, selectors, adjustable target
numbers, timer duration and refresh for the original controller. Numbers use
90–120 °F, 30–80%, and 0–360 timer minutes in whole-unit steps. The service reads
and preserves the unchanged target before writing. Fixed threshold presets and
one-minute/clear timer presets are also available. A separate on/off
switch is not exposed for the original controller. The fan flag reports the
controller's on/off state; airflow is not measured.

To change the API address, open the entry's menu and choose **Reconfigure**.
The new address must report the same persistent proxy UUID, device ID and
backend. Existing HA entity and device IDs are preserved.

MQTT discovery is an alternative to the HTTP integration. Use the
[Home Assistant and MQTT guide](docs/home-assistant-entities.md) if you prefer it.

## Use the command line

With the service running:

```sh
./target/release/gafctl devices
./target/release/gafctl state configured
./target/release/gafctl control configured preset automatic-105-f-30-percent
```

For a remote service, add `--server http://GAFCTL_HOST:8787`. To control the fan
directly over Bluetooth:

```sh
./target/release/gafctl ble control --device-id 'PERIPHERAL_ID' preset timer-one-minute
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
