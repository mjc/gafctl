"""Configuration flow for selecting a device from a local Updraft proxy."""

import voluptuous as vol
from homeassistant import config_entries
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .client import (
    ApiClient,
    ApiError,
    entity_platforms,
    normalize_api_url,
    select_device,
)
from .const import CONF_API_URL, CONF_DEVICE_ID, CONF_PROXY_ID, DEFAULT_API_URL, DOMAIN


class UpdraftConfigFlow(config_entries.ConfigFlow, domain=DOMAIN):
    VERSION = 2

    async def async_step_user(self, user_input=None):
        errors = {}
        if user_input is not None:
            try:
                api_url = normalize_api_url(user_input[CONF_API_URL])
            except ApiError:
                errors["base"] = "invalid_url"
            else:
                self._api_url = api_url
                try:
                    devices = await ApiClient(
                        api_url, async_get_clientsession(self.hass)
                    ).fetch_devices()
                    configured_ids = {
                        (entry.data[CONF_PROXY_ID], entry.data[CONF_DEVICE_ID])
                        for entry in self._async_current_entries()
                    }
                    self._devices = {
                        device["id"]: device
                        for device in devices
                        if (device["proxy_id"], device["id"]) not in configured_ids
                        and entity_platforms(device)
                    }
                    if not self._devices:
                        errors["base"] = "no_device"
                    else:
                        return await self.async_step_device()
                except ApiError:
                    errors["base"] = "cannot_connect"

        schema = vol.Schema(
            {vol.Required(CONF_API_URL, default=DEFAULT_API_URL): str}
        )
        return self.async_show_form(step_id="user", data_schema=schema, errors=errors)

    async def async_step_device(self, user_input=None):
        if user_input is not None:
            try:
                device = select_device(
                    list(self._devices.values()), user_input[CONF_DEVICE_ID]
                )
            except (ApiError, KeyError):
                return self.async_abort(reason="device_unavailable")
            await self.async_set_unique_id(f"updraft_{device['proxy_id']}_{device['id']}")
            self._abort_if_unique_id_configured()
            return self.async_create_entry(
                title=device["name"],
                data={
                    CONF_API_URL: self._api_url,
                    CONF_DEVICE_ID: device["id"],
                    CONF_PROXY_ID: device["proxy_id"],
                    "backend": device["backend"],
                    "device_name": device["name"],
                    "capabilities": device["capabilities"],
                    "state_source": device["state_source"],
                    "command_source": device["command_source"],
                },
            )

        options = {
            device_id: f"{device['name']} ({device_id})"
            for device_id, device in self._devices.items()
        }
        schema = vol.Schema(
            {vol.Required(CONF_DEVICE_ID): vol.In(options)}
        )
        return self.async_show_form(step_id="device", data_schema=schema)
