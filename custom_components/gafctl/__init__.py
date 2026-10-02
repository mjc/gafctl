"""Gafctl Home Assistant integration."""

from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .client import ApiClient
from .const import CONF_API_URL, CONF_DEVICE_ID, CONF_PROXY_ID, PLATFORMS
from .controls import entity_keys
from .coordinator import (
    GafctlConfigEntry,
    GafctlCoordinator,
    async_cleanup_registry,
    unavailable_device,
)
from .models import ApiError


async def async_setup_entry(hass: HomeAssistant, entry: GafctlConfigEntry) -> bool:
    session = async_get_clientsession(hass)
    client = ApiClient(entry.data[CONF_API_URL], session)
    try:
        devices = await client.fetch_devices()
    except ApiError as error:
        raise ConfigEntryNotReady from error
    device = next(
        (
            item
            for item in devices
            if item.id == entry.data[CONF_DEVICE_ID]
            and item.proxy_id == entry.data[CONF_PROXY_ID]
        ),
        None,
    )
    if device is None:
        async_cleanup_registry(hass, entry, unavailable_device(entry))
        raise ConfigEntryNotReady("configured device is absent from this proxy")
    coordinator = GafctlCoordinator(hass, client, device, entry)
    await coordinator.async_config_entry_first_refresh()
    coordinator.loaded_entity_keys = entity_keys(coordinator.device)
    async_cleanup_registry(hass, entry, coordinator.device)
    entry.runtime_data = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    coordinator._entities_loaded = True
    return True


async def async_unload_entry(hass: HomeAssistant, entry: GafctlConfigEntry) -> bool:
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
