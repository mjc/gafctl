"""Selectors for GAF threshold and timer presets and QuickConnect modes."""

from collections.abc import Mapping
from types import MappingProxyType

from homeassistant.components.select import SelectEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys, threshold_control_preset, timer_control_preset
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity
from .models import ApiError

THRESHOLD_PRESETS = MappingProxyType(
    {
        "105.0°F / 30.0%": "automatic105_f30_percent",
        "105.1°F / 30.1%": "automatic105_1_f30_1_percent",
    }
)
TIMER_PRESETS = MappingProxyType(
    {"Clear timer": "timer_clear", "1 minute": "timer_one_minute"}
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: GafctlCoordinator = entry.runtime_data
    control_keys = entity_keys(coordinator.device).get("select", set())
    entities = []
    if "automatic_thresholds" in control_keys:
        entities.extend(
            (
                GafctlControlSelect(
                    coordinator,
                    entry,
                    "automatic_thresholds",
                    "Automatic thresholds",
                    THRESHOLD_PRESETS,
                ),
                GafctlControlSelect(
                    coordinator, entry, "timer", "Fan timer", TIMER_PRESETS
                ),
            )
        )
    if "mode" in control_keys:
        entities.append(
            GafctlControlSelect(
                coordinator,
                entry,
                "mode",
                "Mode",
                {
                    "Off": "off",
                    "Automatic": "automatic",
                    "Timer": "timer",
                    "Manual": "manual",
                },
            )
        )
    async_add_entities(entities)


class GafctlControlSelect(GafctlEntity, SelectEntity):
    """A preset selector that checks BLE acknowledgement and readback."""

    _attr_entity_category = EntityCategory.CONFIG

    def __init__(
        self,
        coordinator: GafctlCoordinator,
        entry: GafctlConfigEntry,
        key: str,
        name: str,
        presets: Mapping[str, str],
    ) -> None:
        super().__init__(coordinator, entry, key)
        self._key = key
        self._presets = presets
        self._attr_name = name
        self._attr_options = list(presets)

    @property
    def available(self) -> bool:
        command_kind = "quick_connect_mode" if self._key == "mode" else "legacy_preset"
        return bool(
            super().available
            and self.coordinator.fresh_readings
            and self.coordinator.supports(command_kind)
        )

    @property
    def current_option(self) -> str | None:
        state = self.state_values
        if not state:
            return None
        current = (
            state.mode
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
        try:
            if self._key == "mode":
                await self.coordinator.async_set_mode(value)
            else:
                await self.coordinator.async_set_preset(value)
        except ApiError as error:
            raise HomeAssistantError(str(error)) from error
