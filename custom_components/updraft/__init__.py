"""Updraft Home Assistant integration."""

import asyncio
import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers import device_registry as dr, entity_registry as er
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ApiClient, ApiError, entity_keys
from .const import CONF_API_URL, CONF_DEVICE_ID, CONF_PROXY_ID, DOMAIN, PLATFORMS, UPDATE_INTERVAL

LOGGER = logging.getLogger(__name__)


class UpdraftCoordinator(DataUpdateCoordinator[dict]):
    def __init__(
        self,
        hass: HomeAssistant,
        client: ApiClient,
        device: dict,
        entry: ConfigEntry,
    ) -> None:
        self.client = client
        self.device = device
        self.device_id = device["id"]
        self.entry = entry
        self.loaded_entity_keys = entity_keys(device)
        self.command_lock = asyncio.Lock()
        self._reload_scheduled = False
        self._entities_loaded = False
        super().__init__(
            hass,
            config_entry=entry,
            logger=LOGGER,
            name=f"Updraft {device['name']}",
            update_interval=UPDATE_INTERVAL,
        )

    async def _async_update_data(self) -> dict:
        try:
            devices = await self.client.fetch_devices()
            current = next(
                (device for device in devices
                 if device["id"] == self.device_id
                 and device["proxy_id"] == self.entry.data[CONF_PROXY_ID]),
                None,
            )
            self.device = current or unavailable_device(self.entry)
            self._reload_changed_entities()
            if current is None:
                raise UpdateFailed("configured device is absent from this proxy")
            return await self.client.fetch_state(self.device_id)
        except ApiError as error:
            raise UpdateFailed(str(error)) from error

    def _reload_changed_entities(self) -> None:
        if (
            self._entities_loaded
            and entity_keys(self.device) != self.loaded_entity_keys
            and not self._reload_scheduled
        ):
            self._reload_scheduled = True
            self.hass.async_create_task(self._reload_entry())

    async def _reload_entry(self) -> None:
        try:
            async_cleanup_registry(self.hass, self.entry, self.device)
            reloaded = await self.hass.config_entries.async_reload(self.entry.entry_id)
            if not reloaded:
                self._reload_scheduled = False
        except Exception:
            self._reload_scheduled = False
            LOGGER.exception("Could not reload changed Updraft entities")

    def supports(self, command_kind: str) -> bool:
        return (
            self.device["command_source"] == "http"
            and any(
                command.get("kind") == command_kind
                for command in self.device.get("commands", [])
                if isinstance(command, dict)
            )
        )

    @property
    def http_state_owned(self) -> bool:
        return self.device["state_source"] == "http"

    @property
    def http_command_owned(self) -> bool:
        return self.device["command_source"] == "http"


def unavailable_device(entry: ConfigEntry) -> dict:
    return {
        "proxy_id": entry.data[CONF_PROXY_ID],
        "id": entry.data[CONF_DEVICE_ID],
        "name": entry.title,
        "backend": entry.data["backend"],
        "state": False,
        "commands": [],
        "state_source": "http",
        "command_source": "http",
    }


@callback
def async_cleanup_registry(hass: HomeAssistant, entry: ConfigEntry, device: dict) -> None:
    expected = {
        (platform, f"{entry.unique_id}_{key}")
        for platform, keys in entity_keys(device).items()
        for key in keys
    }
    entities = er.async_get(hass)
    for entity in er.async_entries_for_config_entry(entities, entry.entry_id):
        if (entity.domain, entity.unique_id) not in expected:
            entities.async_remove(entity.entity_id)
    devices = dr.async_get(hass)
    for registered in dr.async_entries_for_config_entry(devices, entry.entry_id):
        if not er.async_entries_for_device(entities, registered.id):
            devices.async_remove_device(registered.id)


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    session = async_get_clientsession(hass)
    client = ApiClient(entry.data[CONF_API_URL], session)
    try:
        devices = await client.fetch_devices()
    except ApiError as error:
        raise ConfigEntryNotReady from error
    device = next(
        (item for item in devices if item["id"] == entry.data[CONF_DEVICE_ID]
         and item["proxy_id"] == entry.data[CONF_PROXY_ID]),
        None,
    )
    if device is None:
        async_cleanup_registry(hass, entry, unavailable_device(entry))
        raise ConfigEntryNotReady("configured device is absent from this proxy")
    coordinator = UpdraftCoordinator(hass, client, device, entry)
    await coordinator.async_config_entry_first_refresh()
    coordinator.loaded_entity_keys = entity_keys(coordinator.device)
    async_cleanup_registry(hass, entry, coordinator.device)
    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    coordinator._entities_loaded = True
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        hass.data[DOMAIN].pop(entry.entry_id)
    return unloaded
