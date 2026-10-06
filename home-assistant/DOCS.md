# Configure Gafctl

Original ERV5SMT/EGV5SMT controllers use Bluetooth. Enter their peripheral ID in
`device_id`. The host needs an enabled Bluetooth adapter and the fan must be
within range. Close the manufacturer's app before starting this service.

For a scan on Home Assistant OS, use its debug SSH shell and locate the running
container with `docker ps --format '{{.Names}}'`. Its name starts with `app_`
and ends with `_gafctl`. Then run:

```sh
docker exec CONTAINER_NAME gafctl ble scan
```

The scan reads advertisements. Copy the host-local ID into the app
configuration and restart it.

For QuickConnect controllers, set `quickconnect_username`,
`quickconnect_password`, and `quickconnect_role`. This backend is experimental;
leave `quickconnect_writes_enabled` disabled until readings are verified.
QuickConnect requires Internet access. You can configure both backends together.

MQTT is optional. Set `mqtt_host`, `mqtt_username`, and `mqtt_password` together.
For Home Assistant's Mosquitto app, the host is `core-mosquitto`. Use a dedicated
MQTT login. Enable `mqtt_discovery` if you want entities through MQTT.

Assign each device to MQTT after enabling discovery. Read
`http://HOME_ASSISTANT_HOST:8787/api/v2/devices` to get its actual local device ID,
then assign both sources to MQTT:

```sh
curl --fail -X PUT http://HOME_ASSISTANT_HOST:8787/api/v2/devices/DEVICE_ID/sources \
  -H 'Content-Type: application/json' \
  -d '{"state_source":"mqtt","command_source":"mqtt"}'
```

Replace `DEVICE_ID` with the inventory ID and reread inventory to confirm both
sources. MQTT discovery must be enabled for this request to succeed. To use the
HTTP integration, assign both sources to `http`. Ownership persists across app
restarts.

For HTTP, install the separate **Gafctl GAF Vent** integration through the HACS
custom repository `https://github.com/mjc/gafctl`, or copy the repository's
`custom_components/gafctl` directory into your Home Assistant configuration.
The integration requires Home Assistant 2026.9.4 or newer. Restart Home Assistant
and add it with `http://HOME_ASSISTANT_HOST:8787`. Select one entity source per
fan.

Port 8787 is exposed to the local network and has no login. Keep it on your
trusted network. The health watchdog checks the server process; inspect device
state to check whether a fan is available.

Identities and settings persist in `/data` across app updates and are included
in backups. Restoring that data preserves device identities. Passwords are also
stored in the app's private options and backups. Do not share them.

The first installation builds the server from a pinned source revision. It
needs Internet access, time, and free disk space. See the repository's
[installation guide](https://github.com/mjc/gafctl/blob/main/docs/installation.md)
for other server hosts and troubleshooting.
