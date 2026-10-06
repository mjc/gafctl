# Install Gafctl

Install the Gafctl server and its Home Assistant integration separately. The
integration requires Home Assistant **2026.9.4 or newer**. HACS installs the
integration.

| Server host | Install method |
| --- | --- |
| Home Assistant OS | [Home Assistant app](#home-assistant-os) |
| Ubuntu 24.04+, Debian 12+ | [Debian package or source build](#ubuntu-and-debian) |
| Other Linux distributions | [Source build](#source-build) or [Docker Compose](#docker-compose) |
| NixOS / nix-darwin | [Nix package and NixOS module](#nix) |
| macOS | [Nix package](#nix); Bluetooth access runs natively |

Original ERV5SMT and EGV5SMT controllers need a Linux Bluetooth adapter and BlueZ
for an always-on service. Containers use the host's BlueZ over D-Bus. Keep one
server connected to the fan. QuickConnect needs Internet access and a GAF
account; it is experimental, with writes disabled by default.

Release binaries and registry images are unpublished. Use the source builds,
Compose, HACS custom repository, or Home Assistant app below. See
[releases](development.md#releases) for package builds and publication.

## Ubuntu and Debian

Install build dependencies and BlueZ for original controllers:

```sh
sudo apt update
sudo apt install build-essential cmake pkg-config libdbus-1-dev ca-certificates git bluez
sudo systemctl enable --now bluetooth
```

Follow the [source build](#source-build), then build and install the package:

```sh
sudo apt install dpkg-dev jq
./packaging/package.sh
sudo apt install ./dist/gafctl_0.1.0_*.deb
```

Use the package matching your architecture: `amd64` for x86-64 or `arm64` for
64-bit ARM. Packages built by the Dockerfile target Debian 12 / Ubuntu 24.04
and newer. On Ubuntu 22.04, build from source or use the container.

The package installs both executables, a systemd unit, a private configuration
directory, and a D-Bus policy allowing the service account to use BlueZ. It
creates the `gafctl` user and preserves device identity across reinstallations.
Configure and start the service as shown below.

For an original controller, scan and enter the ID in the configuration file:

```sh
sudo gafctl ble scan
sudoedit /etc/gafctl/gafctl.env
```

Uncomment `GAFCTL_DEVICE_ID` and replace its value with your scan's ID. For
QuickConnect, follow [credential setup](deployment.md#quickconnect-experimental).
The package sets the identity store to `/var/lib/gafctl/identities.json`.

The systemd unit listens on loopback by default. If Home Assistant runs on
another host, use `sudo systemctl edit gafctl.service` and add:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/gafctl-server --bind 0.0.0.0:8787 --allow-remote
```

Allow TCP port 8787 only from your Home Assistant host or trusted network.
Then start the service:

```sh
sudo systemctl enable --now gafctl
systemctl status gafctl
journalctl -u gafctl -n 50 --no-pager
```

After a package upgrade, run `sudo systemctl restart gafctl`. Removing or purging
the package keeps `/var/lib/gafctl` and the service account. Delete them yourself
only when you intend to discard the proxy identity and device configuration.

### Build packages with Docker or Podman

On a native Linux host with Docker Buildx:

```sh
docker buildx build --target artifacts --output type=local,dest=dist .
```

The build emits a `.deb` and a binary archive for the host's architecture. With
Podman, build `--target packages`, then copy `/src/dist` out of that image.
Use native ARM64 hardware to build ARM64 packages; no emulation is configured.

## Docker Compose

Install Docker Engine and Compose 2.24 or newer on Linux, then clone this repo:

```sh
git clone https://github.com/mjc/gafctl.git
cd gafctl
cp packaging/gafctl.env gafctl.env
```

Edit `gafctl.env` to set the backend and optional MQTT credentials. Keep the
configuration private with `chmod 600 gafctl.env`. The named volume `gafctl-data`
stores identities and settings; keep it across upgrades. Do not use
`docker compose down --volumes` unless you intend to discard them.

By default, Compose exposes the API only on loopback. To let a separate Home
Assistant host connect, set `GAFCTL_LISTEN_IP` to this server's trusted LAN IP.
The HTTP API has no login. Docker-published ports need Docker-aware firewall
rules; do not assume an ordinary UFW rule limits them.

### Original Bluetooth controller

Start BlueZ on the Linux host. Build the image and scan through the host bus:

```sh
sudo systemctl enable --now bluetooth
docker compose build
docker run --rm --mount type=bind,src=/run/dbus/system_bus_socket,dst=/run/dbus/system_bus_socket,readonly \
  --entrypoint gafctl gafctl:local ble scan
export GAFCTL_DEVICE_ID='PERIPHERAL_ID'
export GAFCTL_LISTEN_IP='SERVER_LAN_IP'
docker compose -f compose.yaml -f compose.bluetooth.yaml up -d
```

Replace both placeholders. The Bluetooth overlay requires a device ID and an
existing D-Bus socket. The container runs as root to use the host's BlueZ
policy, drops Linux capabilities, and uses a read-only root filesystem. Avoid
changing its user without also granting that user access to the host bus and
the identity volume. Bluetooth on Docker Desktop is not supported; use native
Linux for the service or the native macOS CLI.

### QuickConnect controller

In `gafctl.env`, uncomment the username and account role. The Compose overlay
sets the password path inside the container, so leave the sample
`GAFCTL_QUICKCONNECT_PASSWORD_FILE` commented out. Create `quickconnect-password`
using your editor, with only the account password in the file:

```sh
sudo chown root:root quickconnect-password
sudo chmod 600 quickconnect-password
export GAFCTL_LISTEN_IP='SERVER_LAN_IP'
docker compose -f compose.yaml -f compose.quickconnect.yaml up -d --build
```

For rootful Docker Engine, the password bind mount must be a regular file owned
by root with mode `0600`, so the container can read it with capabilities dropped.
Use `sudoedit quickconnect-password` for later changes. Compose's
standard secret mounts use permissions that the server rejects. To use both
backends, include both overlays and set the Bluetooth device ID as above.

Check the service without changing fan settings:

```sh
docker compose logs --tail 50 gafctl
curl http://SERVER_LAN_IP:8787/health
curl http://SERVER_LAN_IP:8787/api/v2/devices
```

To update, pull this repository and repeat your original Compose command with
`--build`. Use the same overlays, environment, and named volume.

## Home Assistant OS

1. Open **Settings → Apps → App store → ⋮ → Repositories**. Older Home Assistant
   versions label these screens **Add-ons** instead of **Apps**.
2. Add `https://github.com/mjc/gafctl` and install **Gafctl**.
3. Open its **Configuration** tab. For an original controller, enter the
   Bluetooth `device_id`. For QuickConnect, enter the account username,
   password, and role. Leave QuickConnect writes disabled until you have
   verified readings.
4. Set MQTT host, username, and password only if you want MQTT. With the
   Mosquitto app, use `core-mosquitto` as the broker hostname and a dedicated
   Home Assistant MQTT user. Enable discovery when you want MQTT entities.
5. Start the app and enable **Start on boot**. Check its log and
   `http://HOME_ASSISTANT_HOST:8787/health`.
6. Discovery publishes only devices assigned to MQTT. Read the inventory and
   select MQTT ownership using the commands below before expecting entities.
7. [Install the integration](#home-assistant-integration) for HTTP, or use
   Home Assistant's MQTT integration for MQTT discovery.

To assign a fan to MQTT, read its actual local ID from inventory:

```sh
curl http://HOME_ASSISTANT_HOST:8787/api/v2/devices
curl --fail -X PUT http://HOME_ASSISTANT_HOST:8787/api/v2/devices/DEVICE_ID/sources \
  -H 'Content-Type: application/json' \
  -d '{"state_source":"mqtt","command_source":"mqtt"}'
curl http://HOME_ASSISTANT_HOST:8787/api/v2/devices
```

Replace `DEVICE_ID` with the inventory ID. Keep both sources equal. MQTT and
MQTT discovery must be configured before this change is accepted. Returning both
sources to `http` removes MQTT discovery for that device; choose one Home
Assistant representation per fan.

This app currently builds the Rust binaries locally from a pinned source
revision during installation. The first build takes time and needs Internet
access and free disk space. Supported architectures are `amd64` and `aarch64`.
The app stores identities under `/data`, which Home Assistant backups include. App passwords
also appear in its private options and backups; treat those backups as secrets.

The app can use Home Assistant OS's Bluetooth adapter through the host's BlueZ
service. Peripheral IDs can differ between computers: scan on this host with
`gafctl ble scan` in the app container, using Home Assistant OS debug SSH if needed, then copy the ID into
its configuration. See the app's Documentation tab for the command.

Home Assistant Container has no app store; run the Gafctl Compose service beside
it. Use the service host's LAN address, or a shared Docker network and the
service name, in the integration.

## Home Assistant integration

### HACS

1. In HACS, open **⋮ → Custom repositories**.
2. Add `https://github.com/mjc/gafctl` with type **Integration**.
3. Find **GAF Attic Vent (via gafctl)**, download it, and restart Home Assistant.
4. Open **Settings → Devices & services → Add integration → GAF Attic Vent (via gafctl)**.
5. Enter `http://GAFCTL_HOST:8787` and select the fans to add.

HACS updates the Python integration separately from the server. Keep their
versions aligned. Choose one entity source per device as described in
[MQTT setup](home-assistant-entities.md).

### Manual installation

Copy `custom_components/gafctl` into your Home Assistant configuration directory.
The original controller exposes Mode, Target temperature, Target humidity, and
Run fan for together in Controls. See the [entity guide](home-assistant-entities.md)
for their behavior and the available readings.
The resulting path must be `custom_components/gafctl/manifest.json`. Restart Home
Assistant, then add the integration as above. On upgrades, replace that directory
and restart; preserve Home Assistant's configuration and the server identity store.

## Source build

Install Rust **1.98.1** through [rustup](https://rustup.rs/). The checked-in
`rust-toolchain.toml` selects that version. Linux also needs a C/C++ toolchain,
CMake, pkg-config, and D-Bus development headers. Then:

```sh
git clone https://github.com/mjc/gafctl.git
cd gafctl
cargo build --release --locked --bins
cargo install --path . --locked
```

`cargo install` installs both `gafctl` and `gafctl-server` into `~/.cargo/bin`.
Keep them together: `gafctl server` launches its sibling `gafctl-server`.
The release build also produces both executables in `target/release` for
packaging or copying to another compatible host. The Linux archives built by
the Dockerfile need `libdbus-1-3` and CA certificates on Debian/Ubuntu.

Follow [service setup](deployment.md) for persistent operation. On other Linux
distributions, install the corresponding D-Bus/BlueZ packages through their
package manager. There is no Windows service package.

## Nix

The flake provides `gafctl` for x86-64 Linux, ARM64 Linux, and ARM64 macOS.
Both executables are installed together:

```sh
nix profile install github:mjc/gafctl#gafctl
gafctl --help
gafctl server --help
```

For a local build, use the repository's devenv environment:

```sh
git clone https://github.com/mjc/gafctl.git
cd gafctl
devenv allow
devenv shell -- nix build .#gafctl
```

### NixOS service

Add `inputs.gafctl.url = "github:mjc/gafctl"` to your system flake and include
`inputs.gafctl.nixosModules.default` in its NixOS modules. Configure an original
controller with its host-local Bluetooth peripheral ID:

```nix
services.gafctl = {
  enable = true;
  bluetooth.deviceId = "PERIPHERAL_ID";
};
```

The module creates the `gafctl` user, enables BlueZ for an original controller,
grants that user D-Bus access, and persists identities in `/var/lib/gafctl`.
Bluetooth is enabled when `bluetooth.deviceId` is set. The service restarts
after failures and loads passwords through systemd credentials.

For MQTT, add:

```nix
services.gafctl.mqtt = {
  enable = true;
  host = "BROKER_HOST";
  username = "gafctl";
  passwordFile = "/run/secrets/gafctl-mqtt";
  discovery = true;
};
```

For experimental QuickConnect access, add:

```nix
services.gafctl.quickconnect = {
  enable = true;
  username = "GAF_ACCOUNT";
  passwordFile = "/run/secrets/gafctl-quickconnect";
  role = "consumer";
};
```

Provision password files at runtime with private permissions. Use quoted
absolute paths to keep secrets outside the Nix store. To enable experimental
cloud writes, set `quickconnect.writesEnabled = true`. Both backends can run
together.

The API listens on `127.0.0.1:8787`. If Home Assistant runs on another host,
set `listenAddress` to a trusted LAN IP and `allowRemote = true`. `openFirewall`
is disabled by default; enable it only if the host's network is trusted, or add
a firewall rule restricted to Home Assistant. The API has no authentication.

### Declarative Home Assistant integration

On a NixOS Home Assistant host, install the integration through its existing
Home Assistant service:

```nix
services.home-assistant.customComponents = [
  inputs.gafctl.packages.${pkgs.stdenv.hostPlatform.system}.home-assistant
];
```

Rebuild the host, then add **GAF Attic Vent (via gafctl)** in **Settings → Devices & services**.
This package installs the integration; configure the server separately.

There is no nix-darwin service module. On macOS, run the CLI or server natively
and grant Bluetooth access when prompted.
