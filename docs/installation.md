# Install Gafctl

Install the Gafctl server and its Home Assistant integration separately. The
integration requires Home Assistant **2026.9.4 or newer**. HACS installs the
integration.

| Server host | Install method |
| --- | --- |
| Home Assistant OS | [Home Assistant app](#home-assistant-os) |
| Ubuntu 24.04+, Debian 12+ | [Debian package or source build](#ubuntu-and-debian) |
| Other Linux distributions | [Binary archive](#linux-binary-archive), [source build](#source-build), or [Docker Compose](#docker-compose) |
| NixOS / nix-darwin | [Nix package and NixOS module](#nix) |
| macOS | [Nix package](#nix); Bluetooth access runs natively |

Original ERV5SMT and EGV5SMT controllers need a Linux Bluetooth adapter and BlueZ
for an always-on service. Containers use the host's BlueZ over D-Bus. Keep one
server connected to the fan. QuickConnect needs Internet access and a GAF
account; it is experimental, with writes disabled by default.

The commands below install version **0.1.0** from its
[GitHub release](https://github.com/mjc/gafctl/releases/tag/v0.1.0) or
`ghcr.io/mjc/gafctl:0.1.0`. Downloads become available when that version is
published. For an unreleased revision, use a [source build](#source-build) or
[build the container locally](#build-the-container-from-source).

## Ubuntu and Debian

Install the download tools:

```sh
sudo apt update
sudo apt install ca-certificates curl
```

Download the package for your architecture and verify its checksum:

```sh
version=0.1.0
arch=$(dpkg --print-architecture)
case "$arch" in amd64|arm64) ;; *) echo "Unsupported architecture: $arch"; exit 1 ;; esac
mkdir -p "gafctl-$version-$arch"
cd "gafctl-$version-$arch"
release="https://github.com/mjc/gafctl/releases/download/v$version"
curl --fail --location --remote-name "$release/gafctl_${version}_${arch}.deb"
curl --fail --location --remote-name "$release/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
```

Continue only if the package checksum reports `OK`:

```sh
sudo apt install "./gafctl_${version}_${arch}.deb"
```

The release provides `amd64` for x86-64 and `arm64` for 64-bit ARM, targeting
Debian 12 / Ubuntu 24.04 and newer. On Ubuntu 22.04, build from source or use
the container.

The package installs both executables, a systemd unit, a private configuration
directory, and a D-Bus policy allowing the service account to use BlueZ. It
creates the `gafctl` user and preserves device identity across reinstallations.
Configure and start the service as shown below.

For an original controller, scan and enter the ID in the configuration file:

```sh
sudo apt install bluez
sudo systemctl enable --now bluetooth
sudo gafctl ble scan
sudoedit /etc/gafctl/gafctl.env
```

Uncomment `GAFCTL_DEVICE_ID` and replace its value with your scan's ID. For
QuickConnect, follow [credential setup](#quickconnect).
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

## Linux binary archive

The release archives contain both executables and their license notices. They
require glibc 2.36 or newer, the D-Bus runtime library, and CA certificates. On
Debian/Ubuntu, install the runtime dependencies and download tools with
`sudo apt install libdbus-1-3 ca-certificates curl`. Original controllers also
need BlueZ. Use your distribution's equivalent packages on other Linux systems.

Download and verify the archive:

```sh
version=0.1.0
case "$(uname -m)" in
  x86_64) arch=amd64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "Unsupported architecture"; exit 1 ;;
esac
mkdir -p "gafctl-$version-$arch"
cd "gafctl-$version-$arch"
release="https://github.com/mjc/gafctl/releases/download/v$version"
curl --fail --location --remote-name "$release/gafctl_${version}_linux_${arch}.tar.gz"
curl --fail --location --remote-name "$release/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
```

Continue only if the archive checksum reports `OK`, then extract and install:

```sh
tar -xzf "gafctl_${version}_linux_${arch}.tar.gz"
sudo install -m 755 gafctl gafctl-server /usr/local/bin/
sudo install -d /usr/local/share/licenses/gafctl
sudo install -m 644 LICENSE LICENSE-QUICKCONNECT-REFERENCE.txt \
  THIRD-PARTY-NOTICES.txt LICENSE-RUST-STDLIB.html /usr/local/share/licenses/gafctl/
gafctl --version
gafctl server --help
```

Keep both executables together; `gafctl server` launches its sibling. Follow
[service setup](#background-service) for persistent operation. Archives do not install
a systemd unit or BlueZ D-Bus policy; use the Debian package or NixOS module
for those. Repeat these steps with the new version to upgrade both executables.

## Docker Compose

Install Docker Engine and Compose 2.24 or newer on Linux, then clone this repo:

```sh
git clone --branch v0.1.0 --depth 1 https://github.com/mjc/gafctl.git
cd gafctl
cp packaging/gafctl.env gafctl.env
export GAFCTL_IMAGE=ghcr.io/mjc/gafctl:0.1.0
docker compose pull gafctl
```

The registry image supports AMD64 and ARM64 Linux; Docker selects the native
architecture. Keep `GAFCTL_IMAGE` set for every Compose command, or save
`GAFCTL_IMAGE=ghcr.io/mjc/gafctl:0.1.0` in Compose's `.env` file beside
`compose.yaml`. The separate `gafctl.env` configures the server inside the
container.

Edit `gafctl.env` to set the backend and optional MQTT credentials. Keep the
configuration private with `chmod 600 gafctl.env`. The named volume `gafctl-data`
stores identities and settings; keep it across upgrades. Do not use
`docker compose down --volumes` unless you intend to discard them.

By default, Compose exposes the API only on loopback. To let a separate Home
Assistant host connect, set `GAFCTL_LISTEN_IP` to this server's trusted LAN IP.
The HTTP API has no login. Docker-published ports need Docker-aware firewall
rules; do not assume an ordinary UFW rule limits them.

### Original Bluetooth controller

Start BlueZ on the Linux host and scan through the host bus:

```sh
sudo systemctl enable --now bluetooth
docker compose run --rm --no-deps \
  -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket:ro \
  --entrypoint gafctl gafctl ble scan
export GAFCTL_DEVICE_ID='PERIPHERAL_ID'
export GAFCTL_LISTEN_IP='SERVER_LAN_IP'
docker compose -f compose.yaml -f compose.bluetooth.yaml up -d --no-build
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
docker compose -f compose.yaml -f compose.quickconnect.yaml up -d --no-build
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

The CLI is inside the container. To use it against the running server:

```sh
docker compose exec gafctl gafctl devices
docker compose exec gafctl gafctl state configured --format json
```

To upgrade a release installation, check out the new release tag and update
`GAFCTL_IMAGE` to its version. Run `docker compose pull gafctl`, then repeat
your original `up -d --no-build` command with the same overlays, environment,
and named volume.

### Build the container from source

Clone the revision you want to build, copy `packaging/gafctl.env` to `gafctl.env`,
and configure it as above. Select the local image and build:

```sh
export GAFCTL_IMAGE=gafctl:local
docker compose build gafctl
```

Use the same scan and start commands above with this image. To update a source
installation, pull the repository, rebuild, and repeat the original start
command. If `.env` sets `GAFCTL_IMAGE`, keep it aligned with your chosen image.

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

For MQTT ownership, follow [MQTT setup](#mqtt), using
`http://HOME_ASSISTANT_HOST:8787` as the service address.

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

Follow the [README steps](../README.md#add-it-to-home-assistant) for HACS or
manual installation. HACS installs from GitHub releases and updates the Python
integration separately from the server; keep their versions aligned. For manual
upgrades, replace `custom_components/gafctl` and restart Home Assistant. Preserve
HA configuration and the server identity store. Choose one [entity source](#mqtt)
per fan.

## Source build

Install Rust **1.98.1** through [rustup](https://rustup.rs/). The checked-in
`rust-toolchain.toml` selects that version. Linux also needs a C/C++ toolchain,
CMake, pkg-config, and D-Bus development headers. On Debian/Ubuntu, install
them with `sudo apt install build-essential cmake pkg-config libdbus-1-dev git`.
Then:

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

Follow [service setup](#background-service) for persistent operation. On other Linux
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

## Background service

The [README](../README.md#start-the-service) shows a foreground server.
`--device-id` configures one original Bluetooth fan. An identity store is required
for either backend; the server can also start empty. Keep that private file
across upgrades: it stores proxy/device identity, timer duration, and pending
mode restoration. Bluetooth must be in range; only one server should connect.

For automatic startup, use the Debian package, Compose, Home Assistant app,
or NixOS module above.

For a manual installation on another systemd distribution, build both binaries
and install the repository's service files:

```sh
sudo install -m 0755 target/release/gafctl target/release/gafctl-server /usr/bin/
sudo useradd --system --home-dir /var/lib/gafctl --shell /usr/sbin/nologin gafctl
sudo install -d -m 0750 -o root -g gafctl /etc/gafctl
sudo install -m 0640 -o root -g gafctl packaging/gafctl.env /etc/gafctl/gafctl.env
sudo install -m 0644 packaging/systemd/gafctl.service /etc/systemd/system/
sudo install -m 0644 packaging/dbus/gafctl.conf /etc/dbus-1/system.d/
```

Skip `useradd` if the account already exists. Edit `/etc/gafctl/gafctl.env` with the
Bluetooth device ID or QuickConnect credentials. Start BlueZ for an original
controller and reload the D-Bus configuration using your distribution's tools.
The unit creates a private `/var/lib/gafctl` state directory and sets the
identity-store path. Follow the [package listener override](#ubuntu-and-debian) if Home
Assistant runs elsewhere, then start the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now gafctl.service
systemctl status gafctl.service
journalctl -u gafctl.service -n 50 --no-pager
```

Check `/health` for process health and `/api/v2/devices/DEVICE_ID/state` for
fan availability and current readings. Restrict port 8787 to trusted clients;
the HTTP API has no login. For remote access, use an authenticated reverse proxy
or private network. `localhost` in Home Assistant refers to HA's own environment.

## QuickConnect

Use this for any [QuickConnect model or retrofit controller](../README.md)
configured in the **GAF Master Flow QuickConnect** app.
Original ERV5SMT and EGV5SMT controllers use Bluetooth. QuickConnect has not been
tested with a live account or fan. See the [QuickConnect API](reference.md#quickconnect-api).

The required settings are:

| Variable | Value |
| --- | --- |
| `GAFCTL_QUICKCONNECT_USERNAME` | Your QuickConnect account login |
| `GAFCTL_QUICKCONNECT_PASSWORD_FILE` | Absolute path to a private file containing the account password |
| `GAFCTL_QUICKCONNECT_ROLE` | `contractor` (default) or `consumer`, matching your account |
| `GAFCTL_IDENTITY_STORE` | Writable file path for persistent device identities |

For a foreground run, create the password file outside the checkout using your
editor, restrict it with `chmod 600`, then run:

```sh
export GAFCTL_QUICKCONNECT_USERNAME='YOUR_ACCOUNT_LOGIN'
export GAFCTL_QUICKCONNECT_PASSWORD_FILE='/absolute/path/to/quickconnect-password'
export GAFCTL_QUICKCONNECT_ROLE=consumer
export GAFCTL_IDENTITY_STORE='/absolute/path/to/gafctl-identities.json'
gafctl server
```

Choose the role that matches your account. The identity file's location
must be writable. Gafctl creates its directory and a new file with owner-only
permissions. Keep this file across upgrades and restarts: it preserves the IDs
Home Assistant uses for cloud devices.

For a systemd service, store the password at
`/etc/gafctl/quickconnect-password` with root-only permissions. Add a credential
to its `[Service]` section:

```ini
LoadCredential=quickconnect-password:/etc/gafctl/quickconnect-password
Environment=GAFCTL_QUICKCONNECT_PASSWORD_FILE=%d/quickconnect-password
```

Add the username, role, and identity path to `/etc/gafctl/gafctl.env`:

```ini
GAFCTL_QUICKCONNECT_USERNAME=YOUR_ACCOUNT_LOGIN
GAFCTL_QUICKCONNECT_ROLE=consumer
GAFCTL_IDENTITY_STORE=/var/lib/gafctl/identities.json
```

Remove `GAFCTL_DEVICE_ID` for cloud-only operation. After editing the unit, run
`sudo systemctl daemon-reload` and `sudo systemctl restart gafctl.service`.

`GAFCTL_QUICKCONNECT_PASSWORD` can supply the password directly through an
existing secret manager. Set one password source and keep it out of command-line
arguments and tracked configuration.

To enable experimental mode, target and timer-duration writes, set
`GAFCTL_QUICKCONNECT_WRITES_ENABLED=true` and restart. Controls then appear in
Home Assistant and the CLI. See [commands and limits](reference.md#controls).

Cloud polling runs separately from Bluetooth polling. A login or Internet failure
leaves the service running and cloud devices unavailable; it retries on the next
poll. Check the login, account role, and password when authentication fails. Keep
the identity file when changing a password or recovering account access.

## MQTT

Configure Home Assistant's MQTT integration and a broker supporting **MQTT 5**.
Set these variables in the service environment, then restart gafctl:

```ini
GAFCTL_MQTT_HOST=BROKER_HOST
GAFCTL_MQTT_PORT=1883
GAFCTL_MQTT_USERNAME=GAFCTL_BROKER_USER
GAFCTL_MQTT_PASSWORD=YOUR_BROKER_PASSWORD
GAFCTL_MQTT_DISCOVERY=true
```

Keep the environment file private. MQTT requires a password; there is no MQTT
password-file option. The NixOS module loads it through systemd credentials.
Gafctl uses plain TCP on every port; use a trusted network or a local TLS tunnel.

Read inventory to find the local device ID, then assign both HA sources to MQTT:

```sh
curl http://GAFCTL_HOST:8787/api/v2/devices
curl --fail -X PUT http://GAFCTL_HOST:8787/api/v2/devices/DEVICE_ID/sources \
  -H 'Content-Type: application/json' \
  -d '{"state_source":"mqtt","command_source":"mqtt"}'
curl http://GAFCTL_HOST:8787/api/v2/devices
```

Replace the host and device ID; the original Bluetooth fan is `configured`.
Both sources default to `http` and must match. MQTT requires a configured broker
and discovery before switching; ownership persists across restarts/outages.
Set both values to `http` to switch back. Administrative commands remain usable
on either transport. Disable discovery to publish state only, after returning
any MQTT-owned devices to HTTP.

Handoff removes the old owner's entities, including after restart. Polling can
briefly expose both owners. HA assigns separate device/entity IDs to each
integration; update automations referring to the old owner. MQTT ownership in
an enabled backend requires broker/discovery configuration at startup.
See the [reference](reference.md#mqtt) for topics, broker permissions, and results.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Service rejects missing identity store | Set `GAFCTL_IDENTITY_STORE` to a private writable path when configuring a fan. |
| Empty device list | Set `GAFCTL_DEVICE_ID` for Bluetooth, or configure QuickConnect credentials. The service does not automatically register scanned fans. |
| Bluetooth fan unavailable | Check the adapter and BlueZ, service-user permissions, range, and the ID from a scan on this computer. Close the GAF app and retry a direct read. |
| Home Assistant cannot connect | Check the host address, port 8787, listener address, `--allow-remote`, and firewall. `localhost` in Home Assistant refers to Home Assistant's own environment. |
| QuickConnect fails at startup | Supply a username, exactly one password source, and a writable identity-store path. The password file must be a regular file with no group/other access. |
| QuickConnect shows no controls | Writes are disabled by default. Read-only devices expose sensors. |
| Control times out | Read the device state before retrying. The service may still be completing the command. |

For upgrades, stop the service, replace both executables, and restart. Keep the
identity store and local configuration. To roll back, restore the previous
executables and restart. Keeping the integration domain and identity preserves
Home Assistant's configuration.
