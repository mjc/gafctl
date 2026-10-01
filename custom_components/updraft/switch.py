"""Mutually exclusive cloud mode controls."""

from typing import Any

from homeassistant.components.switch import SwitchEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import UpdraftCoordinator
from .client import ApiError, QUICKCONNECT_MODES, entity_keys
from .const import DOMAIN


MODES = {"automatic": "Automatic mode", "timer": "Timer mode", "manual": "Manual mode"}


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback) -> None:
    coordinator = hass.data[DOMAIN][entry.entry_id]
    keys = entity_keys(coordinator.device).get("switch", set())
    async_add_entities(
        UpdraftModeSwitch(coordinator, entry, mode, name)
        for mode, name in MODES.items() if f"{mode}_mode" in keys
    )


class UpdraftModeSwitch(CoordinatorEntity[UpdraftCoordinator], SwitchEntity):
    _attr_has_entity_name = True

    def __init__(self, coordinator: UpdraftCoordinator, entry: ConfigEntry, mode: str, name: str) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._mode = mode
        self._attr_name = name
        self._attr_unique_id = f"{entry.unique_id}_{mode}_mode"

    @property
    def is_on(self) -> bool | None:
        mode = ((self.coordinator.data or {}).get("state") or {}).get("mode")
        return mode == self._mode if mode in QUICKCONNECT_MODES else None

    @property
    def available(self) -> bool:
        return bool(super().available and self.coordinator.mode_control_available)

    async def async_turn_on(self, **kwargs: Any) -> None:
        await self._set_mode(self._mode)

    async def async_turn_off(self, **kwargs: Any) -> None:
        await self._set_mode("off", only_if_current=self._mode)

    async def _set_mode(self, mode: str, *, only_if_current: str | None = None) -> None:
        try:
            await self.coordinator.async_set_mode(mode, only_if_current=only_if_current)
        except ApiError as error:
            raise HomeAssistantError(str(error)) from error

    @property
    def device_info(self) -> dr.DeviceInfo:
        return dr.DeviceInfo(identifiers={(DOMAIN, self._entry.unique_id)},
                             name=self.coordinator.device["name"], manufacturer="GAF", model="GAF QuickConnect Vent")
