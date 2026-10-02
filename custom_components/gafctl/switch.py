"""Mutually exclusive cloud mode controls."""

from types import MappingProxyType

from homeassistant.components.switch import SwitchEntity
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import QUICKCONNECT_MODES, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity
from .models import ApiError

MODES = MappingProxyType(
    {"automatic": "Automatic mode", "timer": "Timer mode", "manual": "Manual mode"}
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("switch", set())
    async_add_entities(
        GafctlModeSwitch(coordinator, entry, mode, name)
        for mode, name in MODES.items()
        if f"{mode}_mode" in keys
    )


class GafctlModeSwitch(GafctlEntity, SwitchEntity):
    def __init__(
        self,
        coordinator: GafctlCoordinator,
        entry: GafctlConfigEntry,
        mode: str,
        name: str,
    ) -> None:
        super().__init__(coordinator, entry, f"{mode}_mode")
        self._mode = mode
        self._attr_name = name

    @property
    def is_on(self) -> bool | None:
        state = self.state_values
        mode = state.mode if state else None
        return mode == self._mode if mode in QUICKCONNECT_MODES else None

    @property
    def available(self) -> bool:
        return bool(super().available and self.coordinator.mode_control_available)

    async def async_turn_on(self, **kwargs: object) -> None:
        await self._set_mode(self._mode)

    async def async_turn_off(self, **kwargs: object) -> None:
        await self._set_mode("off", only_if_current=self._mode)

    async def _set_mode(self, mode: str, *, only_if_current: str | None = None) -> None:
        try:
            await self.coordinator.async_set_mode(mode, only_if_current=only_if_current)
        except ApiError as error:
            raise HomeAssistantError(str(error)) from error
