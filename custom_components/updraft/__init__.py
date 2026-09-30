"""Updraft Home Assistant integration."""

import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ApiClient, ApiError
from .const import CONF_API_URL, CONF_DEVICE_ID, DOMAIN, PLATFORMS, UPDATE_INTERVAL

LOGGER = logging.getLogger(__name__)


class UpdraftCoordinator(DataUpdateCoordinator[dict]):
    def __init__(self, hass: HomeAssistant, client: ApiClient, device_id: str) -> None:
        self.client = client
        self.device_id = device_id
        super().__init__(
            hass,
            logger=LOGGER,
            name="Updraft GAF Vent",
            update_interval=UPDATE_INTERVAL,
        )

    async def _async_update_data(self) -> dict:
        try:
            return await self.client.fetch_state(self.device_id)
        except ApiError as error:
            raise UpdateFailed(str(error)) from error


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    session = async_get_clientsession(hass)
    client = ApiClient(entry.data[CONF_API_URL], session)
    coordinator = UpdraftCoordinator(hass, client, entry.data[CONF_DEVICE_ID])
    await coordinator.async_config_entry_first_refresh()
    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        hass.data[DOMAIN].pop(entry.entry_id)
    return unloaded
