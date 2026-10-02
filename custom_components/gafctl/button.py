"""Request readings through the device's service backend."""

from homeassistant.components.button import ButtonEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("button", set())
    async_add_entities(
        entity(coordinator, entry)
        for key, entity in (
            ("refresh", GafctlRefreshButton),
            ("all_off", GafctlAllOffButton),
        )
        if key in keys
    )


class GafctlRefreshButton(GafctlEntity, ButtonEntity):
    _attr_name = "Refresh readings"
    _attr_icon = "mdi:refresh"
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(
        self, coordinator: GafctlCoordinator, entry: GafctlConfigEntry
    ) -> None:
        super().__init__(coordinator, entry, "refresh")

    @property
    def available(self) -> bool:
        return bool(
            super().available
            and self.coordinator.http_owned
            and self.coordinator.device.read_state
        )

    async def async_press(self) -> None:
        with translate_api_errors():
            await self.coordinator.async_refresh_device()


class GafctlAllOffButton(GafctlEntity, ButtonEntity):
    _attr_name = "All off"
    _attr_icon = "mdi:fan-off"

    def __init__(
        self, coordinator: GafctlCoordinator, entry: GafctlConfigEntry
    ) -> None:
        super().__init__(coordinator, entry, "all_off")

    @property
    def available(self) -> bool:
        return bool(super().available and self.coordinator.mode_control_available)

    async def async_press(self) -> None:
        with translate_api_errors():
            await self.coordinator.async_set_mode("off")
