"""Readings and diagnostics reported by the controller."""

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorStateClass,
)
from homeassistant.const import (
    PERCENTAGE,
    EntityCategory,
    UnitOfTemperature,
    UnitOfTime,
)
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlReadingEntity

SENSORS = {
    "temperature": (
        "Ambient temperature",
        ("temperature_f",),
        UnitOfTemperature.FAHRENHEIT,
        SensorDeviceClass.TEMPERATURE,
    ),
    "humidity": (
        "Relative humidity",
        ("humidity_percent",),
        PERCENTAGE,
        SensorDeviceClass.HUMIDITY,
    ),
    "mode": ("Controller mode", ("settings", "mode"), None, None),
    "firmware_version": (
        "Firmware version",
        ("diagnostics", "firmware_version"),
        None,
        None,
    ),
    "automatic_temperature_threshold": (
        "Automatic temperature threshold",
        ("settings", "automatic_temperature_tenths_f"),
        UnitOfTemperature.FAHRENHEIT,
        SensorDeviceClass.TEMPERATURE,
    ),
    "automatic_humidity_threshold": (
        "Automatic humidity threshold",
        ("settings", "automatic_humidity_tenths_percent"),
        PERCENTAGE,
        None,
    ),
    "timer_remaining": (
        "Timer remaining",
        ("settings", "timer_remaining_minutes"),
        UnitOfTime.MINUTES,
        None,
    ),
    "timer_original": (
        "Original timer setting",
        ("settings", "timer_original_minutes"),
        UnitOfTime.MINUTES,
        None,
    ),
    "signal_strength_raw": (
        "Signal strength (reported)",
        ("diagnostics", "signal_strength_raw"),
        None,
        None,
    ),
    "verified_raw": (
        "Verification (reported)",
        ("diagnostics", "verified_raw"),
        None,
        None,
    ),
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("sensor", set())
    async_add_entities(GafctlSensor(coordinator, key) for key in SENSORS if key in keys)


class GafctlSensor(GafctlReadingEntity, SensorEntity):
    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator, key)
        (
            self._attr_name,
            self._path,
            self._attr_native_unit_of_measurement,
            self._attr_device_class,
        ) = SENSORS[key]
        measurement = key in {"temperature", "humidity"}
        self._attr_state_class = SensorStateClass.MEASUREMENT if measurement else None
        self._attr_entity_category = None if measurement else EntityCategory.DIAGNOSTIC

    @property
    def native_value(self) -> float | int | str | None:
        value = self.reading_value(self._path)
        if value is not None and self._key in {
            "automatic_temperature_threshold",
            "automatic_humidity_threshold",
        }:
            return value / 10
        return value
