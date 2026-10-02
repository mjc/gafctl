#!/bin/sh
set -eu
umask 077
if [ -f /data/options.json ]; then
    setting() {
        value=$(jq -r --arg key "$2" '.[$key] // empty | tostring' /data/options.json)
        if [ -n "$value" ]; then export "$1=$value"; else unset "$1"; fi
    }
    setting GAFCTL_DEVICE_ID device_id
    setting GAFCTL_QUICKCONNECT_USERNAME quickconnect_username
    setting GAFCTL_QUICKCONNECT_ROLE quickconnect_role
    setting GAFCTL_QUICKCONNECT_WRITES_ENABLED quickconnect_writes_enabled
    setting GAFCTL_MQTT_HOST mqtt_host
    setting GAFCTL_MQTT_PORT mqtt_port
    setting GAFCTL_MQTT_USERNAME mqtt_username
    setting GAFCTL_MQTT_PASSWORD mqtt_password
    setting GAFCTL_MQTT_DISCOVERY mqtt_discovery
    if jq -e '.quickconnect_password | length > 0' /data/options.json >/dev/null; then
        touch /data/quickconnect-password
        chmod 600 /data/quickconnect-password
        jq -r '.quickconnect_password' /data/options.json > /data/quickconnect-password
        export GAFCTL_QUICKCONNECT_PASSWORD_FILE=/data/quickconnect-password
    fi
fi
exec "$@"
