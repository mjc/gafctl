"""Selectors for GAF threshold and timer presets and QuickConnect modes."""

from collections.abc import Mapping

from homeassistant.components.select import SelectEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import (
    MODE_LABELS,
    THRESHOLDS,
    TIMER_PRESETS,
    entity_keys,
    threshold_control_preset,
    timer_control_preset,
)
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors

THRESHOLD_OPTIONS = {
    f"{temperature:.1f}°F / {humidity:.1f}%": preset
    for preset, (temperature, humidity) in THRESHOLDS.items()
}
TIMER_OPTIONS = {label: preset for preset, (label, _) in TIMER_PRESETS.items()}
MODE_OPTIONS = {label: mode for mode, label in MODE_LABELS.items()}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: GafctlCoordinator = entry.runtime_data
    control_keys = entity_keys(coordinator.device).get("select", set())
    async_add_entities(
        (
            GafctlControlSelect(coordinator, key, name, options)
            for key, name, options in (
                ("automatic_thresholds", "Automatic thresholds", THRESHOLD_OPTIONS),
                ("timer", "Fan timer", TIMER_OPTIONS),
                ("mode", "Mode", MODE_OPTIONS),
            )
            if key in control_keys
        )
    )


class GafctlControlSelect(GafctlEntity, SelectEntity):
    """A preset selector that checks BLE acknowledgement and readback."""

    _attr_entity_category = EntityCategory.CONFIG

    def __init__(
        self,
        coordinator: GafctlCoordinator,
        key: str,
        name: str,
        presets: Mapping[str, str],
    ) -> None:
        super().__init__(coordinator, key)
        self._presets = presets
        self._attr_name = name
        self._attr_options = list(presets)

    @property
    def available(self) -> bool:
        command_kind = "quick_connect_mode" if self._key == "mode" else "legacy_preset"
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
            state["settings"]["mode"]
            if self._key == "mode"
            else threshold_control_preset(state)
            if self._key == "automatic_thresholds"
            else timer_control_preset(state)
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
