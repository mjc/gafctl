"""Read-only GAF state and diagnostic sensors."""

from collections.abc import Callable
from dataclasses import dataclass

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorEntityDescription,
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

from .controls import NUMBER_CONTROLS, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlReadingEntity
from .models import Readings


@dataclass(frozen=True, kw_only=True)
class GafctlSensorDescription(SensorEntityDescription):
    value: Callable[[Readings], float | int | str | None]
    entity_category: EntityCategory | None = EntityCategory.DIAGNOSTIC


SENSORS = (
    GafctlSensorDescription(
        key="temperature",
        name="Ambient temperature",
        value=lambda readings: readings["temperature_f"],
        device_class=SensorDeviceClass.TEMPERATURE,
        native_unit_of_measurement=UnitOfTemperature.FAHRENHEIT,
        state_class=SensorStateClass.MEASUREMENT,
        entity_category=None,
    ),
    GafctlSensorDescription(
        key="humidity",
        name="Relative humidity",
        value=lambda readings: readings["humidity_percent"],
        device_class=SensorDeviceClass.HUMIDITY,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
        entity_category=None,
    ),
    GafctlSensorDescription(
        key="mode",
        name="Controller mode",
        value=lambda readings: readings["settings"]["mode"],
    ),
    GafctlSensorDescription(
        key="firmware_version",
        name="Firmware version",
        value=lambda readings: (
            readings["diagnostics"]["firmware_version"]
            if readings["diagnostics"]
            else None
        ),
    ),
    GafctlSensorDescription(
        key="automatic_temperature_threshold",
        name="Automatic temperature threshold",
        value=NUMBER_CONTROLS["legacy_ble"][0].reading,
        device_class=SensorDeviceClass.TEMPERATURE,
        native_unit_of_measurement=UnitOfTemperature.FAHRENHEIT,
    ),
    GafctlSensorDescription(
        key="automatic_humidity_threshold",
        name="Automatic humidity threshold",
        value=NUMBER_CONTROLS["legacy_ble"][1].reading,
        native_unit_of_measurement=PERCENTAGE,
    ),
    GafctlSensorDescription(
        key="timer_remaining",
        name="Timer remaining",
        value=lambda readings: (
            readings["settings"]["timer_remaining_minutes"]
            if readings["settings"]["backend"] == "legacy_ble"
            else None
        ),
        native_unit_of_measurement=UnitOfTime.MINUTES,
    ),
    GafctlSensorDescription(
        key="timer_original",
        name="Original timer setting",
        value=NUMBER_CONTROLS["legacy_ble"][2].reading,
        native_unit_of_measurement=UnitOfTime.MINUTES,
    ),
    GafctlSensorDescription(
        key="signal_strength_raw",
        name="Signal strength (reported)",
        value=lambda readings: (
            readings["diagnostics"]["signal_strength_raw"]
            if readings["diagnostics"]
            else None
        ),
    ),
    GafctlSensorDescription(
        key="verified_raw",
        name="Verification (reported)",
        value=lambda readings: (
            readings["diagnostics"]["verified_raw"] if readings["diagnostics"] else None
        ),
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: GafctlCoordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("sensor", set())
    async_add_entities(
        GafctlSensor(coordinator, description)
        for description in SENSORS
        if description.key in keys
    )


class GafctlSensor(GafctlReadingEntity, SensorEntity):
    entity_description: GafctlSensorDescription

    @property
    def native_value(self) -> float | int | str | None:
        state = self.state_values
        return self.entity_description.value(state) if state else None
