# gafctl

Gafctl connects **GAF Master Flow** attic fans to Home Assistant. Its HTTP API,
MQTT bridge, and CLI provide readings, settings, and controls.

| Fan or controller | Models | Connection |
| --- | --- | --- |
| Original Wi-Fi Attic Vent | ERV5SMT (roof), EGV5SMT (gable) | Bluetooth; no GAF account or Internet needed |
| Wi-Fi Attic Vent with QuickConnect | ERV5QCT (roof), EGV5QCT (gable) | QuickConnect cloud API |
| EZ Cool plug-in with QuickConnect | EZCQCR1 (roof), EZCQCG1 (gable) | QuickConnect cloud API |
| QuickConnect retrofit module | ERV/EGV series with the module installed | QuickConnect cloud API |

See [fan models and compatibility](https://github.com/mjc/gafctl/blob/main/docs/hardware.md) for manufacturer sources
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
  [QuickConnect service setup](https://github.com/mjc/gafctl/blob/main/docs/deployment.md#quickconnect-experimental);
  the Bluetooth scan and device-ID examples below apply to original controllers.
- Home Assistant, if you want its dashboard and automations.

Use the manufacturer's app for fan firmware updates.

## Install

Choose an installation method for the computer that will connect to the fan:

| Host | Installation |
| --- | --- |
| Home Assistant OS | [Gafctl app](https://github.com/mjc/gafctl/blob/main/docs/installation.md#home-assistant-os) |
| Ubuntu / Debian | [Native package and systemd service](https://github.com/mjc/gafctl/blob/main/docs/installation.md#ubuntu-and-debian) |
| Linux with Docker | [Docker Compose](https://github.com/mjc/gafctl/blob/main/docs/installation.md#docker-compose) |
| NixOS / macOS | [Nix package](https://github.com/mjc/gafctl/blob/main/docs/installation.md#nix) |
| Other Linux distributions | [Binary archive](https://github.com/mjc/gafctl/blob/main/docs/installation.md#linux-binary-archive) or [source build](https://github.com/mjc/gafctl/blob/main/docs/installation.md#source-build) |

Install the separate Home Assistant integration through
[HACS or manual installation](https://github.com/mjc/gafctl/blob/main/docs/installation.md#home-assistant-integration).
The app, package, or container runs the server. The installation guide covers
[release downloads](https://github.com/mjc/gafctl/releases), checksum verification,
the versioned Docker image, and source builds.

### Install with Cargo

Install Rust **1.98.1 or newer** through [rustup](https://rustup.rs/).
On Ubuntu or Debian, install these build dependencies:

```sh
sudo apt install build-essential cmake pkg-config libdbus-1-dev git
```

Then install both `gafctl` and `gafctl-server` from Git:

```sh
cargo install --git https://github.com/mjc/gafctl.git --locked gafctl
```

Make sure Cargo's binary directory, normally `$HOME/.cargo/bin`, is on `PATH`:

```sh
gafctl --version
gafctl server --help
```

`gafctl server` launches its sibling `gafctl-server` and forwards arguments.

### Install with Nix

```sh
nix profile install github:mjc/gafctl#gafctl
```

This supports x86-64 Linux, ARM64 Linux, and ARM64 macOS. For a NixOS service,
use the [NixOS module](https://github.com/mjc/gafctl/blob/main/docs/installation.md#nix).

## Find your fan

```sh
gafctl ble scan
```

Copy the fan's peripheral ID from the output, then read its state:

```sh
gafctl ble state --device-id 'PERIPHERAL_ID'
```

Replace `PERIPHERAL_ID` with the platform-specific Bluetooth ID from the scan.
A successful read shows temperature, humidity, mode, thresholds, and timer values.
If no fan appears, check Bluetooth and range, close the GAF app, and retry.

## Start the service

To let Home Assistant on another computer reach Gafctl:

```sh
gafctl server \
  --device-id 'PERIPHERAL_ID' \
  --identity-store "$HOME/.local/share/gafctl/identities.json" \
  --bind 0.0.0.0:8787 \
  --allow-remote
```

The identity-store path must be private, writable, and outside the checkout.
Keep that file across restarts and upgrades; it stores the service identity and
device configuration.

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
and current readings in the state response. The service polls the original
Bluetooth controller every three seconds on a retained connection. QuickConnect
devices are polled every 30 seconds. See
[Bluetooth behavior](https://github.com/mjc/gafctl/blob/main/docs/bluetooth.md).

For automatic startup and logs, follow the [service setup guide](https://github.com/mjc/gafctl/blob/main/docs/deployment.md).

## Add it to Home Assistant

Requires Home Assistant **2026.9.4 or newer**.

1. In HACS, open **⋮ → Custom repositories**, add
   `https://github.com/mjc/gafctl` with type **Integration**, then download
   **GAF Attic Vent (via gafctl)**. For manual installation, copy this repository's
   `custom_components/gafctl` directory into Home Assistant's configuration
   directory. The resulting path should include
   `custom_components/gafctl/manifest.json`.
2. Restart Home Assistant.
3. Open **Settings → Devices & services → Add integration** and search for
   **GAF Attic Vent (via gafctl)**.
4. Enter `http://GAFCTL_HOST:8787`, replacing the host with your service's address.
   Use the base address without `/api/v2`. Replace any prefilled address.
5. Select the fans to add. Each selected fan gets its own integration entry.

For an original controller, Home Assistant groups these four controls together:

| Control | What it does |
| --- | --- |
| Mode | Select Automatic, Timer, or Off |
| Target temperature | Set 90–120 °F in 1 °F steps and select Automatic |
| Target humidity | Set 30–80% in 1% steps and select Automatic |
| Timer duration | Save 0–360 minutes for the next timed run; defaults to 360 |

Automatic uses the current thresholds. Changing either target preserves the
other target. Editing Timer duration leaves the mode unchanged. Selecting Timer
starts the saved duration, or selects Automatic when it is zero. Off stops the
fan and disables automatic operation. When a timed run ends, gafctl restores the
previous mode and thresholds after a fresh device reading. Gafctl must be running
and able to reach the fan to restore Automatic. Measurements and read-only
diagnostics remain available. **Fan** reports the controller's on/off state;
airflow is unmeasured.

To change the API address, open the entry's menu and choose **Reconfigure**.
The new address must report the same persistent proxy UUID, device ID and
backend. Existing HA entity and device IDs are preserved.

MQTT discovery is an alternative to the HTTP integration. Use the
[Home Assistant and MQTT guide](https://github.com/mjc/gafctl/blob/main/docs/home-assistant-entities.md) if you prefer it.

## Use the command line

With the service running:

```sh
gafctl devices
gafctl state configured
gafctl control configured preset automatic-105-f-30-percent
```

For a remote service, add `--server http://GAFCTL_HOST:8787`. To control the fan
directly over Bluetooth:

```sh
gafctl ble control --device-id 'PERIPHERAL_ID' preset timer-one-minute
```

Add `--format json` for scripts. See the [command line guide](https://github.com/mjc/gafctl/blob/main/docs/cli.md) for
all presets, QuickConnect commands, timeouts, and exit codes.

## More documentation

- [Fan models and compatibility](https://github.com/mjc/gafctl/blob/main/docs/hardware.md)
- [Installation methods](https://github.com/mjc/gafctl/blob/main/docs/installation.md)
- [Run as a service; configure QuickConnect](https://github.com/mjc/gafctl/blob/main/docs/deployment.md)
- [Home Assistant entities and MQTT](https://github.com/mjc/gafctl/blob/main/docs/home-assistant-entities.md)
- [Command line reference](https://github.com/mjc/gafctl/blob/main/docs/cli.md)
- [HTTP API](https://github.com/mjc/gafctl/blob/main/docs/http-api.md)
- [Development and checks](https://github.com/mjc/gafctl/blob/main/docs/development.md)
- [Bluetooth protocol and captured replies](https://github.com/mjc/gafctl/blob/main/docs/protocol-findings.md)
- [QuickConnect API](https://github.com/mjc/gafctl/blob/main/docs/quickconnect-contract.md)

## License

Gafctl is licensed under [MIT](https://github.com/mjc/gafctl/blob/main/LICENSE). The
[QuickConnect reference notice](https://github.com/mjc/gafctl/blob/main/LICENSE-QUICKCONNECT-REFERENCE.txt) covers the
upstream reference implementation.
