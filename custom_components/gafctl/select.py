"""Fan operating mode control."""

from homeassistant.components.select import SelectEntity
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import MODE_LABELS, entity_keys, legacy_mode
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    if "mode" in entity_keys(coordinator.device).get("select", set()):
        async_add_entities([GafctlControlSelect(coordinator)])


class GafctlControlSelect(GafctlEntity, SelectEntity):
    """Select an operating mode and display confirmed state."""

    _attr_name = "Mode"

    def __init__(self, coordinator: GafctlCoordinator) -> None:
        super().__init__(coordinator, "mode")
        modes = (
            ("automatic", "timer", "off")
            if self._entry.data["backend"] == "legacy_ble"
            else MODE_LABELS
        )
        self._modes = {MODE_LABELS[mode]: mode for mode in modes}
        self._attr_options = list(self._modes)

    @property
    def available(self) -> bool:
        backend = self._entry.data["backend"]
        command = "legacy_mode" if backend == "legacy_ble" else "quick_connect_mode"
        return bool(
            super().available
            and self.coordinator.control_readings(command, backend) is not None
        )

    @property
    def current_option(self) -> str | None:
        state = self.state_values
        if not state:
            return None
        mode = (
            legacy_mode(state)
            if self._entry.data["backend"] == "legacy_ble"
            else state["settings"]["mode"]
        )
        return next(
            (label for label, value in self._modes.items() if value == mode), None
        )

    async def async_select_option(self, option: str) -> None:
        mode = self._modes.get(option)
        if mode is None:
            raise HomeAssistantError("Unsupported fan operating mode")
        with translate_api_errors():
            await self.coordinator.async_set_mode(mode)
