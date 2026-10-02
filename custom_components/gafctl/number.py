"""Number controls for settings advertised by the device."""

from dataclasses import dataclass

from homeassistant.components.number import (
    NumberEntity,
    NumberEntityDescription,
    NumberMode,
)
from homeassistant.const import PERCENTAGE, UnitOfTemperature, UnitOfTime
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import NumberControl, entity_keys, number_controls
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity
from .models import ApiError


@dataclass(frozen=True, kw_only=True)
class GafctlNumberDescription(NumberEntityDescription):
    control: NumberControl


PRESENTATION = (
    ("Target temperature", UnitOfTemperature.FAHRENHEIT),
    ("Target humidity", PERCENTAGE),
    ("Timer duration", UnitOfTime.MINUTES),
)


def descriptions(
    controls: tuple[NumberControl, ...],
) -> tuple[GafctlNumberDescription, ...]:
    return tuple(
        GafctlNumberDescription(
            key=control.key,
            name=name,
            native_unit_of_measurement=unit,
            control=control,
        )
        for control, (name, unit) in zip(controls, PRESENTATION, strict=True)
    )


CONTROLS = descriptions(number_controls("quick_connect"))
LEGACY_CONTROLS = descriptions(number_controls("legacy_ble"))


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("number", set())
    controls = (
        LEGACY_CONTROLS if coordinator.device.backend == "legacy_ble" else CONTROLS
    )
    async_add_entities(
        GafctlNumber(coordinator, entry, description)
        for description in controls
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
            return value if value is not None and 0 <= value <= 360 else None
        return value

    @property
    def available(self) -> bool:
        return super().available and self.coordinator.number_control_available(
            self.entity_description.control
        )

    async def async_set_native_value(self, value: float) -> None:
        try:
            await self.coordinator.async_set_number(
                self.entity_description.control, value
            )
        except ApiError as error:
            raise HomeAssistantError(str(error)) from error
