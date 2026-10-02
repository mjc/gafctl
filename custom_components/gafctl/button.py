"""Request current readings through the device's existing service backend."""

from homeassistant.components.button import ButtonEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import GafctlCoordinator
from .client import ApiError, entity_keys
from .const import DOMAIN


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = hass.data[DOMAIN][entry.entry_id]
    if "refresh" in entity_keys(coordinator.device).get("button", set()):
        async_add_entities([GafctlRefreshButton(coordinator, entry)])
    if "all_off" in entity_keys(coordinator.device).get("button", set()):
        async_add_entities([GafctlAllOffButton(coordinator, entry)])


class GafctlRefreshButton(CoordinatorEntity[GafctlCoordinator], ButtonEntity):
    _attr_has_entity_name = True
    _attr_name = "Refresh readings"
    _attr_icon = "mdi:refresh"
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, coordinator: GafctlCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._attr_unique_id = f"{entry.unique_id}_refresh"

    @property
    def available(self) -> bool:
        return bool(
            super().available
            and self.coordinator.http_state_owned
            and self.coordinator.device["state"]
        )

    async def async_press(self) -> None:
        try:
            await self.coordinator.async_refresh_device()
        except ApiError as error:
            raise HomeAssistantError(str(error)) from error

    @property
    def device_info(self) -> dr.DeviceInfo:
        return dr.DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=self.coordinator.device["name"],
            manufacturer="GAF",
            model=("GAF QuickConnect Vent" if self.coordinator.device["backend"] == "quick_connect" else "GAF Wi-Fi Vent"),
        )


class GafctlAllOffButton(CoordinatorEntity[GafctlCoordinator], ButtonEntity):
    _attr_has_entity_name = True
    _attr_name = "All off"
    _attr_icon = "mdi:fan-off"

    def __init__(self, coordinator: GafctlCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._attr_unique_id = f"{entry.unique_id}_all_off"

    @property
    def available(self) -> bool:
        return bool(super().available and self.coordinator.mode_control_available)

    async def async_press(self) -> None:
        try:
            await self.coordinator.async_set_mode("off")
        except ApiError as error:
            raise HomeAssistantError(str(error)) from error

    @property
    def device_info(self) -> dr.DeviceInfo:
        return dr.DeviceInfo(identifiers={(DOMAIN, self._entry.unique_id)},
                             name=self.coordinator.device["name"], manufacturer="GAF", model="GAF QuickConnect Vent")
