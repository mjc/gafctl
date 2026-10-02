"""Selectors for GAF threshold and timer presets and QuickConnect modes."""

from homeassistant.components.select import SelectEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .client import ApiError, entity_keys, timer_control_preset
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity

THRESHOLD_PRESETS = {
    "105.0°F / 30.0%": "automatic105_f30_percent",
    "105.1°F / 30.1%": "automatic105_1_f30_1_percent",
}
TIMER_PRESETS = {"Clear timer": "timer_clear", "1 minute": "timer_one_minute"}


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
        presets: dict[str, str],
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
            and self.coordinator.http_command_owned
            and self.coordinator.supports(command_kind)
            and self.coordinator.data
            and self.coordinator.data.get("available")
            and self.coordinator.data.get("state") is not None
        )

    @property
    def current_option(self) -> str | None:
        state = self.state_values
        if not state:
            return None
        if self._key == "mode":
            return next(
                (
                    label
                    for label, mode in self._presets.items()
                    if mode == state.get("mode")
                ),
                None,
            )
        if self._key == "automatic_thresholds":
            current = (
                state.get("automatic_temperature_threshold_f"),
                state.get("automatic_humidity_threshold_percent"),
            )
            return next(
                (
                    label
                    for label, preset in self._presets.items()
                    if _thresholds_for(preset) == current
                ),
                None,
            )
        current_preset = timer_control_preset(state)
        return next(
            (
                label
                for label, preset in self._presets.items()
                if preset == current_preset
            ),
            None,
        )

    async def async_select_option(self, option: str) -> None:
        value = self._presets.get(option)
        if value is None:
            raise HomeAssistantError("Unsupported fan control option")
        if self._key == "mode":
            try:
                await self.coordinator.async_set_mode(value)
            except ApiError as error:
                raise HomeAssistantError(str(error)) from error
            return
        await self._async_set_preset(value)

    async def _async_set_preset(self, value: str) -> None:
        async with self.coordinator.command_lock:
            if not self.available:
                raise HomeAssistantError(
                    "The selected device cannot accept this control"
                )
            control_error = None
            try:
                await self.coordinator.client.set_control(
                    self.coordinator.device_id, value
                )
            except ApiError as error:
                control_error = error
            try:
                await self.coordinator.async_refresh()
            except Exception as error:
                if control_error is not None:
                    raise HomeAssistantError(
                        f"{control_error}; current state refresh failed"
                    ) from control_error
                raise HomeAssistantError(
                    "Control was confirmed, but Home Assistant could not refresh state"
                ) from error
            state = self.coordinator.data
            if (
                not self.coordinator.last_update_success
                or not state
                or state.get("freshness") != "fresh"
                or state.get("available") is not True
                or state.get("state") is None
            ):
                if control_error is not None:
                    raise HomeAssistantError(
                        f"{control_error}; current state refresh failed"
                    ) from control_error
                raise HomeAssistantError(
                    "Control was confirmed, but Home Assistant could not refresh state"
                )
            if control_error is not None:
                raise HomeAssistantError(str(control_error)) from control_error


def _thresholds_for(preset: str) -> tuple[float, float] | None:
    return {
        "automatic105_f30_percent": (105.0, 30.0),
        "automatic105_1_f30_1_percent": (105.1, 30.1),
    }.get(preset)
