# Run Updraft as a service

First build Updraft and confirm a direct state read using the
[README](../README.md). The service computer needs Bluetooth within range of an
original ERV5SMT or EGV5SMT. QuickConnect uses the Internet instead of Bluetooth.

## Listen for Home Assistant

By default, `updraft serve` listens on `127.0.0.1:8787`. That works for clients
on the same computer. For Home Assistant on another computer, use:

```sh
updraft serve --device-id 'PERIPHERAL_ID' \
  --identity-store /absolute/path/to/updraft-identities.json \
  --bind 0.0.0.0:8787 --allow-remote
```

Replace the peripheral ID with the value from `updraft ble scan` and choose a
private writable identity-store path. Keep the identity file across restarts.
Restrict port
8787 to your trusted network or Home Assistant host. The API has no built-in
login. An authenticated reverse proxy can provide HTTPS for remote clients.

`--device-id` configures one original Bluetooth fan. Without it, the service
still starts, but no Bluetooth device is registered. Configuring any fan requires
an identity store. QuickConnect can run alone
or alongside that fan.

## Linux with systemd

These instructions are for a Linux distribution where you manage service files
manually. On NixOS, manage the package, user, Bluetooth, firewall, and systemd
unit declaratively. This repository does not yet export a NixOS package or
service module; the current build workflow is devenv. Repository packaging and the
module are tracked in [UPD-50](https://lific.mjc.lol/UPD/issues/UPD-50).

Install the executable from the repository root:

```sh
sudo install -m 0755 target/release/updraft /usr/local/bin/updraft
sudo useradd --system --home-dir /var/lib/updraft --shell /usr/sbin/nologin updraft
sudo install -d -m 0750 -o root -g updraft /etc/updraft
sudo install -m 0640 -o root -g updraft /dev/null /etc/updraft/updraft.env
```

Create the user only if it does not already exist. Edit
`/etc/updraft/updraft.env` and add your scan's peripheral ID:

```ini
UPDRAFT_DEVICE_ID=PERIPHERAL_ID
UPDRAFT_IDENTITY_STORE=/var/lib/updraft/identities.json
```

Create `/etc/systemd/system/updraft.service`:

```ini
[Unit]
Description=Updraft GAF Master Flow attic fan service
Wants=network-online.target
After=network-online.target bluetooth.service

[Service]
User=updraft
Group=updraft
StateDirectory=updraft
StateDirectoryMode=0700
EnvironmentFile=/etc/updraft/updraft.env
ExecStart=/usr/local/bin/updraft serve --bind 0.0.0.0:8787 --allow-remote
Restart=on-failure
RestartSec=5
UMask=0077

[Install]
WantedBy=multi-user.target
```

Install and enable BlueZ using your distribution's tools. The `updraft` user
must be allowed to access BlueZ through the system D-Bus; check your distribution's
policy if the service gets a permission error. Then start Updraft:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now updraft.service
systemctl status updraft.service
journalctl -u updraft.service -n 50 --no-pager
```

Allow the required network access in your firewall, then check from the Home
Assistant host:

```sh
curl http://UPDRAFT_HOST:8787/health
curl http://UPDRAFT_HOST:8787/api/v2/devices
curl http://UPDRAFT_HOST:8787/api/v2/devices/configured/state
```

Replace `UPDRAFT_HOST` with the service computer's address. `/health` checks the
process. The device state response should have `available: true` and current
readings before you add the [Home Assistant integration](../README.md#add-it-to-home-assistant).

## QuickConnect (experimental)

Use this only for a fan configured in the **GAF Master Flow QuickConnect** app.
It does not connect an original ERV5SMT or EGV5SMT to the cloud. Live compatibility
has not been verified; see [hardware](hardware.md#quickconnect) and
[API research](quickconnect-contract.md).

The required settings are:

| Variable | Value |
| --- | --- |
| `UPDRAFT_QUICKCONNECT_USERNAME` | Your QuickConnect account login |
| `UPDRAFT_QUICKCONNECT_PASSWORD_FILE` | Absolute path to a private file containing the account password |
| `UPDRAFT_QUICKCONNECT_ROLE` | `contractor` (default) or `consumer`, matching your account |
| `UPDRAFT_IDENTITY_STORE` | Writable file path for persistent device identities |

For a foreground run, create the password file outside the checkout using your
editor, restrict it with `chmod 600`, then run:

```sh
export UPDRAFT_QUICKCONNECT_USERNAME='YOUR_ACCOUNT_LOGIN'
export UPDRAFT_QUICKCONNECT_PASSWORD_FILE='/absolute/path/to/quickconnect-password'
export UPDRAFT_QUICKCONNECT_ROLE=consumer
export UPDRAFT_IDENTITY_STORE='/absolute/path/to/updraft-identities.json'
updraft serve
```

Choose the role that matches your account. The identity file's location
must be writable. Updraft creates its directory and a new file with owner-only
permissions. Keep this file across upgrades and restarts: it preserves the IDs
Home Assistant uses for cloud devices.

For the systemd service above, store the password at
`/etc/updraft/quickconnect-password` with root-only permissions. Add a credential
to its `[Service]` section:

```ini
LoadCredential=quickconnect-password:/etc/updraft/quickconnect-password
Environment=UPDRAFT_QUICKCONNECT_PASSWORD_FILE=%d/quickconnect-password
```

Add the username, role, and identity path to `/etc/updraft/updraft.env`:

```ini
UPDRAFT_QUICKCONNECT_USERNAME=YOUR_ACCOUNT_LOGIN
UPDRAFT_QUICKCONNECT_ROLE=consumer
UPDRAFT_IDENTITY_STORE=/var/lib/updraft/identities.json
```

Remove `UPDRAFT_DEVICE_ID` for cloud-only operation. After editing the unit, run
`sudo systemctl daemon-reload` and `sudo systemctl restart updraft.service`.

`UPDRAFT_QUICKCONNECT_PASSWORD` can supply the password directly through an
existing secret manager instead. Set exactly one password source. Never put
passwords in command-line arguments or tracked configuration.

Cloud controls are disabled by default. To opt into experimental mode, target,
and timer-duration writes, set `UPDRAFT_QUICKCONNECT_WRITES_ENABLED=true` and
restart. This advertises controls to Home Assistant and the CLI; enabling it
does not establish that your model's cloud writes work. The CLI guide lists the
[commands and limits](cli.md#quickconnect-controls).

Cloud polling runs separately from Bluetooth polling. A login or Internet failure
leaves the service running and cloud devices unavailable; it retries on the next
poll. Check the login, account role, and password when authentication fails. Keep
the identity file when changing a password or recovering account access.

## MQTT

See [Home Assistant and MQTT](home-assistant-entities.md#mqtt-setup) for broker
settings, discovery, topics, and access rules. MQTT is optional; the HTTP
integration does not need a broker.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Service rejects missing identity store | Set `UPDRAFT_IDENTITY_STORE` to a private writable path when configuring a fan. |
| Empty device list | Set `UPDRAFT_DEVICE_ID` for Bluetooth, or configure QuickConnect credentials. The service does not automatically register scanned fans. |
| Bluetooth fan unavailable | Check the adapter and BlueZ, service-user permissions, range, and the ID from a scan on this computer. Close the GAF app and retry a direct read. |
| Home Assistant cannot connect | Check the host address, port 8787, listener address, `--allow-remote`, and firewall. `localhost` in Home Assistant refers to Home Assistant's own environment. |
| QuickConnect fails at startup | Supply a username, exactly one password source, and a writable identity-store path. The password file must be a regular file with no group/other access. |
| QuickConnect shows no controls | Writes are disabled by default. Read-only devices expose sensors. |
| Control times out | Read the device state before retrying. The service may still be completing the command. |

For upgrades, stop the service, replace the executable, and restart. Keep the
identity store and local configuration. To roll back, restore the previous
executable and restart. Home Assistant's integration configuration does not need
to be recreated for a normal upgrade.
