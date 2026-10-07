# gafctl

Gafctl connects **GAF Master Flow** attic fans to Home Assistant. Its HTTP API,
MQTT bridge, and CLI provide readings, settings, and controls.

| Fan or controller | Models | Connection |
| --- | --- | --- |
| Original Wi-Fi Attic Vent | ERV5SMT (roof), EGV5SMT (gable) | Bluetooth; no GAF account or Internet needed |
| Wi-Fi Attic Vent with QuickConnect | ERV5QCT (roof), EGV5QCT (gable) | QuickConnect cloud API |
| EZ Cool plug-in with QuickConnect | EZCQCR1 (roof), EZCQCG1 (gable) | QuickConnect cloud API |
| QuickConnect retrofit module | ERV/EGV series with the module installed | QuickConnect cloud API |

See [model sources](https://github.com/mjc/gafctl/blob/main/docs/reference.md#model-sources) for manufacturer sources
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
  [QuickConnect service setup](https://github.com/mjc/gafctl/blob/main/docs/installation.md#quickconnect);
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

Then install both `gafctl` and `gafctl-server` from crates.io:

```sh
cargo install gafctl --locked
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

The scan and foreground-server commands below require a native installation.
For Docker or Home Assistant OS, follow their [installation steps](https://github.com/mjc/gafctl/blob/main/docs/installation.md)
to scan and start the service. Against a running Compose server, run CLI commands
inside the container, for example `docker compose exec gafctl gafctl devices`.

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
[Bluetooth behavior](https://github.com/mjc/gafctl/blob/main/docs/reference.md#connection-lifecycle).

For automatic startup and logs, follow the [service setup guide](https://github.com/mjc/gafctl/blob/main/docs/installation.md#background-service).

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
starts it, or selects Automatic at zero. Off stops the fan and disables automatic
operation. Gafctl restores the previous mode after a timed run; it must remain
running and able to read the fan. **Fan** reports the controller's on/off state;
airflow is unmeasured. See [timer and availability behavior](https://github.com/mjc/gafctl/blob/main/docs/reference.md#home-assistant-behavior).

Use **Reconfigure** in the integration entry's menu to change its API address.
The new address must identify the same proxy, device, and backend to preserve
Home Assistant entity IDs.

MQTT discovery is an alternative to the HTTP integration. Use the
[Home Assistant and MQTT guide](https://github.com/mjc/gafctl/blob/main/docs/installation.md#mqtt) if you prefer it.

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

Add `--format json` for scripts. Run `gafctl --help` or a subcommand's `--help`
for arguments. See the [reference](https://github.com/mjc/gafctl/blob/main/docs/reference.md)
for command outcomes, timeouts, and the HTTP/MQTT protocols.

## Documentation

- [Installation](https://github.com/mjc/gafctl/blob/main/docs/installation.md): packages, containers, services, and MQTT setup.
- [Reference](https://github.com/mjc/gafctl/blob/main/docs/reference.md): CLI behavior, HA readings, HTTP/MQTT, and device protocols.
- [Development](https://github.com/mjc/gafctl/blob/main/docs/development.md): builds, checks, and releases.

## License

Gafctl is licensed under [MIT](https://github.com/mjc/gafctl/blob/main/LICENSE). The
[QuickConnect reference notice](https://github.com/mjc/gafctl/blob/main/LICENSE-QUICKCONNECT-REFERENCE.txt) covers the
upstream reference implementation.
