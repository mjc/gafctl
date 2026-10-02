"""Refresh readings and turn off cloud modes."""

from homeassistant.components.button import ButtonEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors

BUTTONS = {
    "refresh": ("Refresh readings", "mdi:refresh", EntityCategory.DIAGNOSTIC),
    "all_off": ("All off", "mdi:fan-off", None),
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("button", set())
    async_add_entities(GafctlButton(coordinator, key) for key in BUTTONS if key in keys)


class GafctlButton(GafctlEntity, ButtonEntity):
    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator, key)
        self._attr_name, self._attr_icon, self._attr_entity_category = BUTTONS[key]

    @property
    def available(self) -> bool:
        allowed = (
            self.coordinator.device is not None
            and self.coordinator.http_owned
            and self.coordinator.device["capabilities"]["read_state"]
            if self._key == "refresh"
            else self.coordinator.mode_control_available
        )
        return super().available and allowed

    async def async_press(self) -> None:
        with translate_api_errors():
            if self._key == "refresh":
                await self.coordinator.async_refresh_device()
            else:
                await self.coordinator.async_set_mode("off")
