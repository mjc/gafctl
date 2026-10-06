"""Selectors for GAF modes and automatic threshold presets."""

from homeassistant.components.select import SelectEntity
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import (
    MODE_LABELS,
    THRESHOLDS,
    entity_keys,
    legacy_mode,
    threshold_control_preset,
)
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors

THRESHOLD_OPTIONS = {
    f"{temperature:.1f}°F / {humidity:.1f}%": preset
    for preset, (temperature, humidity) in THRESHOLDS.items()
}
MODE_OPTIONS = {label: mode for mode, label in MODE_LABELS.items()}
SELECTS = {
    "automatic_thresholds": ("Automatic thresholds", THRESHOLD_OPTIONS),
    "mode": ("Mode", MODE_OPTIONS),
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: GafctlCoordinator = entry.runtime_data
    control_keys = entity_keys(coordinator.device).get("select", set())
    async_add_entities(
        GafctlControlSelect(coordinator, key) for key in SELECTS if key in control_keys
    )


class GafctlControlSelect(GafctlEntity, SelectEntity):
    """A mode or preset selector with confirmed state readback."""

    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator, key)
        self._attr_name, self._presets = SELECTS[key]
        if key == "mode" and self._entry.data["backend"] == "legacy_ble":
            self._presets = {
                MODE_LABELS[mode]: mode for mode in ("automatic", "timer", "off")
            }
        self._attr_options = list(self._presets)

    @property
    def available(self) -> bool:
        backend = self._entry.data["backend"]
        command_kind = (
            ("legacy_mode" if backend == "legacy_ble" else "quick_connect_mode")
            if self._key == "mode"
            else "legacy_preset"
        )
        return bool(
            super().available
            and self.coordinator.control_readings(
                command_kind, self._entry.data["backend"]
            )
            is not None
        )

    @property
    def current_option(self) -> str | None:
        state = self.state_values
        if not state:
            return None
        current = (
            (
                legacy_mode(state)
                if self._entry.data["backend"] == "legacy_ble"
                else state["settings"]["mode"]
            )
            if self._key == "mode"
            else threshold_control_preset(state)
        )
        return next(
            (label for label, preset in self._presets.items() if preset == current),
            None,
        )

    async def async_select_option(self, option: str) -> None:
        value = self._presets.get(option)
        if value is None:
            raise HomeAssistantError("Unsupported fan control option")
        with translate_api_errors():
            if self._key == "mode":
                await self.coordinator.async_set_mode(value)
            else:
                await self.coordinator.async_set_preset(value)
