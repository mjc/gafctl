# Configure Gafctl

Original ERV5SMT/EGV5SMT controllers use Bluetooth. Enter their peripheral ID in
`device_id`. The host needs an enabled Bluetooth adapter and the fan must be
within range. Close the manufacturer's app before starting this service.

For a scan on Home Assistant OS, use its debug SSH shell and locate the running
container with `docker ps --format '{{.Names}}'`. Its name starts with `addon_`
and ends with `_gafctl`. Then run:

```sh
docker exec CONTAINER_NAME gafctl ble scan
```

This scan discovers devices without opening a fan connection. Copy the reported
ID into the app configuration and restart it. Do not use a Bluetooth ID from
a different host without verifying it on this host.

For QuickConnect controllers, set `quickconnect_username`,
`quickconnect_password`, and `quickconnect_role`. This backend is experimental;
leave `quickconnect_writes_enabled` disabled until readings are verified.
QuickConnect requires Internet access. You can configure both backends together.

MQTT is optional. Set `mqtt_host`, `mqtt_username`, and `mqtt_password` together.
For Home Assistant's Mosquitto app, the host is `core-mosquitto`. Use a dedicated
MQTT login. Enable `mqtt_discovery` if you want entities through MQTT.

For HTTP, install the separate **Gafctl GAF Vent** integration through the HACS
custom repository `https://github.com/mjc/gafctl`, or copy the repository's
`custom_components/gafctl` directory into your Home Assistant configuration.
Restart Home Assistant and add the integration with
`http://HOME_ASSISTANT_HOST:8787`. Do not add both HTTP and MQTT entities for the
same fan; select one entity source for each device.

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
