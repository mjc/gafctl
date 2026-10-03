"""Readings and diagnostics reported by the controller."""

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorStateClass,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys, tenths
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlReadingEntity
from .readings import SENSORS


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("sensor", set())
    async_add_entities(
        GafctlSensor(coordinator, key)
        for key in SENSORS[entry.data["backend"]]
        if key in keys
    )


class GafctlSensor(GafctlReadingEntity, SensorEntity):
    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator, key)
        reading = SENSORS[coordinator.entry.data["backend"]][key]
        self._attr_name = reading.name
        self._path = reading.path
        self._tenths = reading.tenths
        self._attr_native_unit_of_measurement = reading.unit
        self._attr_device_class = (
            SensorDeviceClass(reading.device_class) if reading.device_class else None
        )
        self._attr_state_class = (
            SensorStateClass.MEASUREMENT if reading.measurement else None
        )
        self._attr_entity_category = (
            None if reading.measurement else EntityCategory.DIAGNOSTIC
        )

    @property
    def native_value(self) -> float | int | str | None:
        value = self.reading_value(self._path)
        return tenths(value) if self._tenths else value
