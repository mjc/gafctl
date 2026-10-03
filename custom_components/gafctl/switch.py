"""Mutually exclusive cloud mode controls."""

from homeassistant.components.switch import SwitchEntity
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import MODE_LABELS, entity_keys, mode_is
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("switch", set())
    async_add_entities(
        GafctlModeSwitch(coordinator, mode)
        for mode in MODE_LABELS
        if f"{mode}_mode" in keys
    )


class GafctlModeSwitch(GafctlEntity, SwitchEntity):
    def __init__(self, coordinator: GafctlCoordinator, mode: str) -> None:
        super().__init__(coordinator, f"{mode}_mode")
        self._mode = mode
        self._attr_name = f"{MODE_LABELS[mode]} mode"

    @property
    def is_on(self) -> bool | None:
        state = self.state_values
        mode = state["settings"]["mode"] if state else None
        return mode_is(mode, self._mode)

    @property
    def available(self) -> bool:
        return bool(super().available and self.coordinator.mode_control_available)

    async def async_turn_on(self, **kwargs: object) -> None:
        with translate_api_errors():
            await self.coordinator.async_set_mode(self._mode, only_if_current=None)

    async def async_turn_off(self, **kwargs: object) -> None:
        with translate_api_errors():
            await self.coordinator.async_set_mode("off", only_if_current=self._mode)
