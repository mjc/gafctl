"""Number controls for settings advertised by the device."""

from homeassistant.components.number import NumberEntity, NumberMode
from homeassistant.const import PERCENTAGE, UnitOfTemperature, UnitOfTime
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import NUMBER_CONTROLS, NumberControl, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity, translate_api_errors

PRESENTATION = {
    "automatic_temperature": ("Target temperature", UnitOfTemperature.FAHRENHEIT),
    "automatic_humidity": ("Target humidity", PERCENTAGE),
    "timer_duration": ("Set timer", UnitOfTime.MINUTES),
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("number", set())
    async_add_entities(
        GafctlNumber(coordinator, control)
        for control in NUMBER_CONTROLS[entry.data["backend"]]
        if control.key in keys
    )


class GafctlNumber(GafctlEntity, NumberEntity):
    _attr_mode = NumberMode.BOX
    _attr_entity_category = None

    def __init__(self, coordinator: GafctlCoordinator, control: NumberControl) -> None:
        super().__init__(coordinator, control.key)
        self._control = control
        self._attr_name, self._attr_native_unit_of_measurement = PRESENTATION[
            control.key
        ]
        self._attr_native_min_value = control.minimum
        self._attr_native_max_value = control.maximum
        self._attr_native_step = control.step

    @property
    def native_value(self) -> float | None:
        state = self.state_values
        if state is None:
            return None
        control = self._control
        value = control.reading(state)
        if control.backend == "legacy_ble" and control.key == "timer_duration":
            return value if control.accepts(value) else None
        return value

    @property
    def available(self) -> bool:
        return super().available and self.coordinator.number_control_available(
            self._control
        )

    async def async_set_native_value(self, value: float) -> None:
        with translate_api_errors():
            await self.coordinator.async_set_number(self._control, value)
