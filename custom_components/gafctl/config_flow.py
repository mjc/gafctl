"""Select devices whose Home Assistant source is HTTP."""

import voluptuous as vol
from homeassistant import config_entries
from homeassistant.config_entries import ConfigFlowResult
from homeassistant.helpers import selector
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .client import ApiClient, normalize_api_url
from .const import CONF_API_URL, CONF_DEVICE_ID, CONF_PROXY_ID, DEFAULT_API_URL, DOMAIN
from .controls import entity_keys, select_device
from .models import ApiError, Device, JsonObject, JsonValue, device_identity

CONF_DEVICE_IDS = "device_ids"


class GafctlConfigFlow(config_entries.ConfigFlow, domain=DOMAIN):
    VERSION = 2
    _api_url: str
    _devices: dict[str, Device]

    async def _fetch_devices(self, api_url: str) -> list[Device]:
        return await ApiClient(
            api_url, async_get_clientsession(self.hass)
        ).fetch_devices()

    async def async_step_user(
        self, user_input: JsonObject | None = None
    ) -> ConfigFlowResult:
        if user_input is None:
            return self._address_form("user", DEFAULT_API_URL, {})
        try:
            self._api_url = normalize_api_url(user_input[CONF_API_URL])
        except ApiError:
            return self._address_form("user", DEFAULT_API_URL, {"base": "invalid_url"})
        try:
            devices = await self._fetch_devices(self._api_url)
        except ApiError:
            return self._address_form(
                "user", DEFAULT_API_URL, {"base": "cannot_connect"}
            )
        configured = {
            (entry.data[CONF_PROXY_ID], entry.data[CONF_DEVICE_ID])
            for entry in self._async_current_entries()
        }
        self._devices = {
            device["id"]: device
            for device in devices
            if (device["proxy_id"], device["id"]) not in configured
            and entity_keys(device)
        }
        if not self._devices:
            return self._address_form("user", DEFAULT_API_URL, {"base": "no_device"})
        return await self.async_step_device()

    async def async_step_device(
        self, user_input: JsonObject | None = None
    ) -> ConfigFlowResult:
        errors = {}
        if user_input is not None:
            try:
                devices = await self._selected_devices(user_input[CONF_DEVICE_IDS])
                for device in devices[1:]:
                    result = await self.hass.config_entries.flow.async_init(
                        DOMAIN,
                        context={"source": config_entries.SOURCE_IMPORT},
                        data=device_data(self._api_url, device),
                    )
                    if (
                        result["type"] != "create_entry"
                        and result.get("reason") != "already_configured"
                    ):
                        raise ApiError("selected device could not be configured")
                return await self._create_device_entry(devices[0])
            except ApiError, KeyError:
                errors["base"] = "device_unavailable"
        options = [
            {"value": device_id, "label": f"{device['name']} ({device_id})"}
            for device_id, device in self._devices.items()
        ]
        schema = vol.Schema(
            {
                vol.Required(CONF_DEVICE_IDS): selector.SelectSelector(
                    selector.SelectSelectorConfig(options=options, multiple=True)
                )
            }
        )
        return self.async_show_form(step_id="device", data_schema=schema, errors=errors)

    async def _selected_devices(self, selected: JsonValue) -> list[Device]:
        if (
            not isinstance(selected, list)
            or not selected
            or any(
                not isinstance(device_id, str) or device_id not in self._devices
                for device_id in selected
            )
        ):
            raise ApiError("invalid selection")
        current = await self._fetch_devices(self._api_url)
        devices = [
            select_device(current, device_id) for device_id in dict.fromkeys(selected)
        ]
        if any(
            device_identity(self._devices[device["id"]]) != device_identity(device)
            for device in devices
        ):
            raise ApiError("identity changed")
        return devices

    async def async_step_import(self, user_input: JsonObject) -> ConfigFlowResult:
        try:
            self._api_url = normalize_api_url(user_input[CONF_API_URL])
            device = select_device(
                await self._fetch_devices(self._api_url), user_input[CONF_DEVICE_ID]
            )
            if device_identity(device) != (
                user_input[CONF_PROXY_ID],
                user_input[CONF_DEVICE_ID],
                user_input["backend"],
            ):
                raise ApiError("identity changed")
        except ApiError, KeyError:
            return self.async_abort(reason="device_unavailable")
        return await self._create_device_entry(device)

    async def _create_device_entry(self, device: Device) -> ConfigFlowResult:
        await self.async_set_unique_id(f"gafctl_{device['proxy_id']}_{device['id']}")
        self._abort_if_unique_id_configured()
        return self.async_create_entry(
            title=device["name"], data=device_data(self._api_url, device)
        )

    async def async_step_reconfigure(
        self, user_input: JsonObject | None = None
    ) -> ConfigFlowResult:
        entry = self._get_reconfigure_entry()
        default = entry.data[CONF_API_URL]
        if user_input is None:
            return self._address_form("reconfigure", default, {})
        try:
            api_url = normalize_api_url(user_input[CONF_API_URL])
        except ApiError:
            return self._address_form("reconfigure", default, {"base": "invalid_url"})
        try:
            device = select_device(
                await self._fetch_devices(api_url), entry.data[CONF_DEVICE_ID]
            )
        except ApiError:
            return self._address_form(
                "reconfigure", default, {"base": "cannot_connect"}
            )
        identity = (
            entry.data[CONF_PROXY_ID],
            entry.data[CONF_DEVICE_ID],
            entry.data["backend"],
        )
        if device_identity(device) != identity:
            return self._address_form("reconfigure", default, {"base": "wrong_device"})
        return self.async_update_reload_and_abort(
            entry, data_updates={CONF_API_URL: api_url}, reason="reconfigure_successful"
        )

    def _address_form(
        self, step_id: str, default: str, errors: dict[str, str]
    ) -> ConfigFlowResult:
        schema = vol.Schema({vol.Required(CONF_API_URL, default=default): str})
        return self.async_show_form(step_id=step_id, data_schema=schema, errors=errors)


def device_data(api_url: str, device: Device) -> dict[str, object]:
    return {
        CONF_API_URL: api_url,
        CONF_DEVICE_ID: device["id"],
        CONF_PROXY_ID: device["proxy_id"],
        "backend": device["backend"],
    }
