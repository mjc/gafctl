from datetime import timedelta

DOMAIN = "updraft"
PLATFORMS = ["sensor", "select", "number", "binary_sensor", "button", "switch"]
DEFAULT_API_URL = "http://updraft:8787"
UPDATE_INTERVAL = timedelta(seconds=30)
CONF_API_URL = "api_url"
CONF_DEVICE_ID = "device_id"
CONF_PROXY_ID = "proxy_id"
