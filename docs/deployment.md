# Run Gafctl as a service

First build Gafctl and confirm a direct state read using the
[README](../README.md). The service computer needs Bluetooth within range of an
original ERV5SMT or EGV5SMT. QuickConnect requires Internet access.

## Listen for Home Assistant

By default, `gafctl server` listens on `127.0.0.1:8787`. That works for clients
on the same computer. For Home Assistant on another computer, use:

```sh
gafctl server --device-id 'PERIPHERAL_ID' \
  --identity-store /absolute/path/to/gafctl-identities.json \
  --bind 0.0.0.0:8787 --allow-remote
```

Replace the peripheral ID with the value from `gafctl ble scan` and choose a
private writable identity-store path. Keep the identity file across restarts.
Restrict port 8787 to your trusted network or Home Assistant host. The API has
no built-in login. An authenticated reverse proxy can provide HTTPS for remote clients.

`--device-id` registers one original Bluetooth fan. An identity store is required
when configuring any fan. The service supports Bluetooth, QuickConnect, or both;
it can also start with no devices configured.

## Linux with systemd

These instructions are for a Linux distribution where you manage service files
manually. On NixOS, manage the package, user, Bluetooth, firewall, and systemd
unit declaratively. This repository builds with devenv and exports no NixOS
package or service module.

Install both executables from the repository root after the release build:

```sh
sudo install -m 0755 target/release/gafctl target/release/gafctl-server /usr/local/bin/
sudo useradd --system --home-dir /var/lib/gafctl --shell /usr/sbin/nologin gafctl
sudo install -d -m 0750 -o root -g gafctl /etc/gafctl
sudo install -m 0640 -o root -g gafctl /dev/null /etc/gafctl/gafctl.env
```

Create the user only if it does not already exist. Edit
`/etc/gafctl/gafctl.env` and add your scan's peripheral ID:

```ini
GAFCTL_DEVICE_ID=PERIPHERAL_ID
GAFCTL_IDENTITY_STORE=/var/lib/gafctl/identities.json
```

Create `/etc/systemd/system/gafctl.service`:

```ini
[Unit]
Description=Gafctl GAF Master Flow attic fan service
Wants=network-online.target
After=network-online.target bluetooth.service

[Service]
User=gafctl
Group=gafctl
StateDirectory=gafctl
StateDirectoryMode=0700
EnvironmentFile=/etc/gafctl/gafctl.env
ExecStart=/usr/local/bin/gafctl-server --bind 0.0.0.0:8787 --allow-remote
Restart=on-failure
RestartSec=5
UMask=0077

[Install]
WantedBy=multi-user.target
```

Install and enable BlueZ using your distribution's tools. The `gafctl` user
must be allowed to access BlueZ through the system D-Bus; check your distribution's
policy if the service gets a permission error. Then start Gafctl:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now gafctl.service
systemctl status gafctl.service
journalctl -u gafctl.service -n 50 --no-pager
```

Allow the required network access in your firewall, then check from the Home
Assistant host:

```sh
curl http://GAFCTL_HOST:8787/health
curl http://GAFCTL_HOST:8787/api/v2/devices
curl http://GAFCTL_HOST:8787/api/v2/devices/configured/state
```

Replace `GAFCTL_HOST` with the service computer's address. `/health` checks the
process. The device state response should have `available: true` and current
readings before you add the [Home Assistant integration](../README.md#add-it-to-home-assistant).

## QuickConnect (experimental)

Use this only for a fan configured in the **GAF Master Flow QuickConnect** app.
Original ERV5SMT and EGV5SMT controllers use Bluetooth. QuickConnect has not been
tested with a live account or fan; see [hardware](hardware.md#quickconnect) and
[API research](quickconnect-contract.md).

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

For the systemd service above, store the password at
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
existing secret manager instead. Set exactly one password source. Never put
passwords in command-line arguments or tracked configuration.

Cloud controls are disabled by default. To opt into experimental mode, target,
and timer-duration writes, set `GAFCTL_QUICKCONNECT_WRITES_ENABLED=true` and
restart. This advertises controls to Home Assistant and the CLI. Cloud writes
have not been tested on a QuickConnect fan. The CLI guide lists the
[commands and limits](cli.md#controls).

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
| Service rejects missing identity store | Set `GAFCTL_IDENTITY_STORE` to a private writable path when configuring a fan. |
| Empty device list | Set `GAFCTL_DEVICE_ID` for Bluetooth, or configure QuickConnect credentials. The service does not automatically register scanned fans. |
| Bluetooth fan unavailable | Check the adapter and BlueZ, service-user permissions, range, and the ID from a scan on this computer. Close the GAF app and retry a direct read. |
| Home Assistant cannot connect | Check the host address, port 8787, listener address, `--allow-remote`, and firewall. `localhost` in Home Assistant refers to Home Assistant's own environment. |
| QuickConnect fails at startup | Supply a username, exactly one password source, and a writable identity-store path. The password file must be a regular file with no group/other access. |
| QuickConnect shows no controls | Writes are disabled by default. Read-only devices expose sensors. |
| Control times out | Read the device state before retrying. The service may still be completing the command. |

For upgrades, stop the service, replace both executables, and restart. Keep the
identity store and local configuration. To roll back, restore the previous
executables and restart. Home Assistant's integration configuration does not need
to be recreated for an upgrade that retains its integration domain and identity.

## Migration from Updraft

The rename changes executable names, the HA integration domain, MQTT namespaces,
environment variables, and shared Nix service, user, credential, and state paths.
Deploy the renamed service, HA integration and broker configuration together.
Existing installations require manual migration.

1. Stop the old service before starting `gafctl-server`.
2. Preserve the existing identity-store contents, including the proxy UUID and
   device mappings. Move the file to the new private state path and set ownership
   for the new service user. Provision credentials at the new configured paths
   and use `GAFCTL_` variables.
3. Back up the HA configuration. Plan the domain/registry migration before
   replacing `custom_components/updraft` with `custom_components/gafctl`; copying
   the directory leaves existing entries under the old domain. Preserve entity
   IDs used by dashboards and automations, and verify proxy/device identity
   after migration.
4. Remove the old retained MQTT discovery configurations before enabling the new
   publisher. Update broker permissions for `gafctl/` and the discovery node
   `homeassistant/+/gafctl/+/config` described in the MQTT guide.
5. Verify state freshness, entity ownership, and control readback, then remove
   the old component and service configuration. Keep the migration backup for
   rollback. Restore the matching HA domain, broker configuration and state
   paths along with the old executable.
