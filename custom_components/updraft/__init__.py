"""Updraft Home Assistant integration."""

import asyncio
import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ApiClient, ApiError, entity_keys
from .const import CONF_API_URL, CONF_DEVICE_ID, DOMAIN, PLATFORMS, UPDATE_INTERVAL

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
        super().__init__(
            hass,
            logger=LOGGER,
            name=f"Updraft {device['name']}",
            update_interval=UPDATE_INTERVAL,
        )

    async def _async_update_data(self) -> dict:
        try:
            devices = await self.client.fetch_devices()
        except ApiError:
            devices = None
        try:
            if devices is not None:
                current = next(
                    (device for device in devices if device["id"] == self.device_id),
                    None,
                )
                if current is not None:
                    self.device = current
                    if (
                        entity_keys(current) != self.loaded_entity_keys
                        and not self._reload_scheduled
                    ):
                        self._reload_scheduled = True
                        self.hass.async_create_task(self._reload_entry())
            return await self.client.fetch_state(self.device_id)
        except ApiError as error:
            raise UpdateFailed(str(error)) from error

    async def _reload_entry(self) -> None:
        try:
            reloaded = await self.hass.config_entries.async_reload(self.entry.entry_id)
            if not reloaded:
                self._reload_scheduled = False
        except Exception:
            self._reload_scheduled = False
            LOGGER.exception("Could not reload changed Updraft entities")

    def supports(self, command_kind: str) -> bool:
        return (
            self.device.get("command_source", "http") == "http"
            and any(
                command.get("kind") == command_kind
                for command in self.device.get("commands", [])
                if isinstance(command, dict)
            )
        )

    @property
    def http_state_owned(self) -> bool:
        return self.device.get("state_source", "http") == "http"

    @property
    def http_command_owned(self) -> bool:
        return self.device.get("command_source", "http") == "http"


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    session = async_get_clientsession(hass)
    client = ApiClient(entry.data[CONF_API_URL], session)
    try:
        devices = await client.fetch_devices()
    except ApiError as error:
        raise ConfigEntryNotReady from error
    device = next(
        (item for item in devices if item["id"] == entry.data[CONF_DEVICE_ID]),
        None,
    )
    if device is None:
        device = {
            "id": entry.data[CONF_DEVICE_ID],
            "name": entry.data.get("device_name", "GAF Vent"),
            "backend": entry.data.get("backend", "legacy_ble"),
            "state": True,
            "capabilities": entry.data.get("capabilities", {}),
            "commands": entry.data.get("capabilities", {}).get("commands", []),
            "state_source": entry.data.get("state_source", "http"),
            "command_source": entry.data.get("command_source", "http"),
        }
    coordinator = UpdraftCoordinator(hass, client, device, entry)
    await coordinator.async_config_entry_first_refresh()
    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    unloaded = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unloaded:
        hass.data[DOMAIN].pop(entry.entry_id)
    return unloaded


async def async_migrate_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Keep existing entry and entity identifiers while adding per-device entries."""
    if entry.version == 1 and entry.data.get(CONF_DEVICE_ID):
        hass.config_entries.async_update_entry(entry, version=2)
        return True
    return entry.version == 2
