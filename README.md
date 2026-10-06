# gafctl

Gafctl connects **GAF Master Flow** attic fans to Home Assistant. Its HTTP API,
MQTT bridge, and CLI provide readings, settings, and controls.

| Fan or controller | Models | Connection |
| --- | --- | --- |
| Original Wi-Fi Attic Vent | ERV5SMT (roof), EGV5SMT (gable) | Bluetooth; no GAF account or Internet needed |
| Wi-Fi Attic Vent with QuickConnect | ERV5QCT (roof), EGV5QCT (gable) | QuickConnect cloud API |
| EZ Cool plug-in with QuickConnect | EZCQCR1 (roof), EZCQCG1 (gable) | QuickConnect cloud API |
| QuickConnect retrofit module | ERV/EGV series with the module installed | QuickConnect cloud API |

See [fan models and compatibility](docs/hardware.md) for manufacturer sources
and controller identification.

QuickConnect is experimental, untested on hardware, and read-only by default.

## What you need

- **Original controller:** an ERV5SMT or EGV5SMT with Bluetooth firmware **3.0.0**.
  GAF added Bluetooth in that firmware version; older firmware needs the
  **GAF Wi-Fi Vent** app to update it.
- A computer to run Gafctl. For the original controller, it needs Bluetooth within
  range of the fan. For an always-on Bluetooth service,
  use a Linux computer with BlueZ. macOS can run the Bluetooth command line too.
- **QuickConnect controller:** a fan set up in the **GAF Master Flow QuickConnect**
  app, an account, and Internet access. Follow the
  [QuickConnect service setup](docs/deployment.md#quickconnect-experimental);
  the Bluetooth scan and device-ID examples below apply to original controllers.
- Home Assistant, if you want its dashboard and automations.

Use the manufacturer's app for fan firmware updates.

## Install

Choose an installation method for the computer that will connect to the fan:

| Host | Installation |
| --- | --- |
| Home Assistant OS | [Gafctl app](docs/installation.md#home-assistant-os) |
| Ubuntu / Debian | [Native package and systemd service](docs/installation.md#ubuntu-and-debian) |
| Linux with Docker | [Docker Compose](docs/installation.md#docker-compose) |
| NixOS / macOS | [devenv](docs/installation.md#nix) |
| Other Linux distributions | [Source build](docs/installation.md#source-build) |

Install the separate Home Assistant integration through
[HACS or manual installation](docs/installation.md#home-assistant-integration).
The app, package, or container runs the server. Release binaries and registry
images are unpublished; use the source builds in the installation guide.

The server package includes `gafctl` and `gafctl-server`. `gafctl server` launches
its sibling `gafctl-server` and forwards arguments. The examples use
`./target/release/gafctl` after a source build, or `gafctl` after installation.

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
If no fan appears, check Bluetooth and range, close the GAF app, and retry.

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

1. Install through [HACS](docs/installation.md#hacs), or copy this repository's
   `custom_components/gafctl` directory into
   Home Assistant's configuration directory as `custom_components/gafctl`.
   The resulting path should include `custom_components/gafctl/manifest.json`.
2. Restart Home Assistant.
3. Open **Settings → Devices & services → Add integration** and search for
   **Gafctl GAF Vent**.
4. Enter `http://GAFCTL_HOST:8787`, replacing the host with your service's address.
   Use the base address without `/api/v2`. Replace any prefilled address.
5. Select the fans to add. Each selected fan gets its own integration entry.

Home Assistant exposes measurements, diagnostics, selectors, adjustable target
numbers and timer duration for the original controller. Numbers use
90–120 °F, 30–80%, and 0–360 timer minutes in whole-unit steps. The service reads
and preserves the unchanged target before writing. Fixed threshold presets and
one-minute/clear timer presets are also available. The fan flag reports the
controller's on/off state; airflow is unmeasured.

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
- [Installation methods](docs/installation.md)
- [Run as a service; configure QuickConnect](docs/deployment.md)
- [Home Assistant entities and MQTT](docs/home-assistant-entities.md)
- [Command line reference](docs/cli.md)
- [HTTP API](docs/http-api.md)
- [Development and checks](docs/development.md)
- [Bluetooth protocol and captured replies](docs/protocol-findings.md)
- [QuickConnect API](docs/quickconnect-contract.md)

## License

Gafctl is licensed under [MIT](LICENSE). The
[QuickConnect reference notice](LICENSE-QUICKCONNECT-REFERENCE.txt) covers the
upstream reference implementation.
