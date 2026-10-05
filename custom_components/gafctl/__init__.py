"""Gafctl Home Assistant integration."""

from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .client import ApiClient
from .const import CONF_API_URL, PLATFORMS
from .controls import entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator


async def async_setup_entry(hass: HomeAssistant, entry: GafctlConfigEntry) -> bool:
    session = async_get_clientsession(hass)
    client = ApiClient(entry.data[CONF_API_URL], session)
    coordinator = GafctlCoordinator(hass, client, entry)
    await coordinator.async_config_entry_first_refresh()
    coordinator.loaded_entity_keys = entity_keys(coordinator.device)
    entry.runtime_data = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    coordinator._entities_loaded = True
    entry.async_on_unload(
        coordinator.async_add_listener(coordinator._reload_changed_entities)
    )
    return True


async def async_unload_entry(hass: HomeAssistant, entry: GafctlConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        await entry.runtime_data.async_unload()
    return unloaded
