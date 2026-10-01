"""Configuration flow for an existing local Updraft proxy."""

from uuid import uuid4

import voluptuous as vol
from homeassistant import config_entries
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .client import ApiClient, ApiError, normalize_api_url
from .const import CONF_API_URL, CONF_DEVICE_ID, DEFAULT_API_URL, DOMAIN


class UpdraftConfigFlow(config_entries.ConfigFlow, domain=DOMAIN):
    VERSION = 1

    async def async_step_user(self, user_input=None):
        errors = {}
        if user_input is not None:
            try:
                api_url = normalize_api_url(user_input[CONF_API_URL])
            except ApiError:
                errors["base"] = "invalid_url"
            else:
                client = ApiClient(api_url, async_get_clientsession(self.hass))
                try:
                    devices = await client.fetch_devices()
                    device = next(
                        (
                            item
                            for item in devices
                            if item["state"]
                            and item["backend"] == "legacy_ble"
                            and any(
                                command.get("kind") == "legacy_preset"
                                for command in item["commands"]
                                if isinstance(command, dict)
                            )
                        ),
                        None,
                    )
                    if device is None:
                        errors["base"] = "no_device"
                    else:
                        await self.async_set_unique_id(uuid4().hex)
                        return self.async_create_entry(
                            title="Updraft GAF Vent",
                            data={
                                CONF_API_URL: api_url,
                                CONF_DEVICE_ID: device["id"],
                            },
                        )
                except ApiError:
                    errors["base"] = "cannot_connect"

        schema = vol.Schema(
            {vol.Required(CONF_API_URL, default=DEFAULT_API_URL): str}
        )
        return self.async_show_form(step_id="user", data_schema=schema, errors=errors)
