"""Number controls for settings advertised by the device."""

from dataclasses import dataclass
from types import MappingProxyType

from homeassistant.components.number import (
    NumberEntity,
    NumberEntityDescription,
    NumberMode,
)
from homeassistant.const import PERCENTAGE, UnitOfTemperature, UnitOfTime
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import NUMBER_CONTROLS, NumberControl, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors


@dataclass(frozen=True, kw_only=True)
class GafctlNumberDescription(NumberEntityDescription):
    control: NumberControl


PRESENTATION = MappingProxyType(
    {
        "automatic_temperature": ("Target temperature", UnitOfTemperature.FAHRENHEIT),
        "automatic_humidity": ("Target humidity", PERCENTAGE),
        "timer_duration": ("Timer duration", UnitOfTime.MINUTES),
    }
)
DESCRIPTIONS = MappingProxyType(
    {
        backend: tuple(
            GafctlNumberDescription(
                key=control.key,
                name=PRESENTATION[control.key][0],
                native_unit_of_measurement=PRESENTATION[control.key][1],
                control=control,
            )
            for control in controls
        )
        for backend, controls in NUMBER_CONTROLS.items()
    }
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("number", set())
    async_add_entities(
        GafctlNumber(coordinator, entry, description)
        for description in DESCRIPTIONS[coordinator.device.backend]
        if description.key in keys
    )


class GafctlNumber(GafctlEntity, NumberEntity):
    entity_description: GafctlNumberDescription
    _attr_mode = NumberMode.BOX
    _attr_entity_category = None

    def __init__(
        self,
        coordinator: GafctlCoordinator,
        entry: GafctlConfigEntry,
        description: GafctlNumberDescription,
    ) -> None:
        super().__init__(coordinator, entry, description.key)
        self.entity_description = description
        control = description.control
        self._attr_native_min_value = control.minimum
        self._attr_native_max_value = control.maximum
        self._attr_native_step = control.step

    @property
    def native_value(self) -> float | None:
        state = self.state_values
        if state is None:
            return None
        control = self.entity_description.control
        value = control.reading(state)
        if control.backend == "legacy_ble" and control.key == "timer_duration":
            return value if control.accepts(value) else None
        return value

    @property
    def available(self) -> bool:
        return super().available and self.coordinator.number_control_available(
            self.entity_description.control
        )

    async def async_set_native_value(self, value: float) -> None:
        with translate_api_errors():
            await self.coordinator.async_set_number(
                self.entity_description.control, value
            )
