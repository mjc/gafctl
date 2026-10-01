from datetime import timedelta

DOMAIN = "updraft"
PLATFORMS = ["sensor", "select", "number", "binary_sensor"]
DEFAULT_API_URL = "http://tali.local:8787"
UPDATE_INTERVAL = timedelta(seconds=30)
CONF_API_URL = "api_url"
CONF_DEVICE_ID = "device_id"
