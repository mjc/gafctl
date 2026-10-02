"""Read-only GAF state and diagnostic sensors."""

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

from .client import JsonObject, JsonValue, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity


@dataclass(frozen=True, kw_only=True)
class GafctlSensorDescription(SensorEntityDescription):
    value_key: str


SENSORS = (
    GafctlSensorDescription(
        key="temperature",
        name="Ambient temperature",
        value_key="temperature_f",
        device_class=SensorDeviceClass.TEMPERATURE,
        native_unit_of_measurement=UnitOfTemperature.FAHRENHEIT,
        state_class=SensorStateClass.MEASUREMENT,
    ),
    GafctlSensorDescription(
        key="humidity",
        name="Relative humidity",
        value_key="humidity_percent",
        device_class=SensorDeviceClass.HUMIDITY,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
    ),
    GafctlSensorDescription(
        key="mode",
        name="Controller mode",
        value_key="mode",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="firmware_version",
        name="Firmware version",
        value_key="firmware_version",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="automatic_temperature_threshold",
        name="Automatic temperature threshold",
        value_key="automatic_temperature_threshold_f",
        device_class=SensorDeviceClass.TEMPERATURE,
        native_unit_of_measurement=UnitOfTemperature.FAHRENHEIT,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="automatic_humidity_threshold",
        name="Automatic humidity threshold",
        value_key="automatic_humidity_threshold_percent",
        native_unit_of_measurement=PERCENTAGE,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="timer_remaining",
        name="Timer remaining",
        value_key="timer_remaining_minutes",
        native_unit_of_measurement=UnitOfTime.MINUTES,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="timer_original",
        name="Original timer setting",
        value_key="timer_original_minutes",
        native_unit_of_measurement=UnitOfTime.MINUTES,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="signal_strength_raw",
        name="Signal strength (reported)",
        value_key="signal_strength_raw",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    GafctlSensorDescription(
        key="verified_raw",
        name="Verification (reported)",
        value_key="verified_raw",
        entity_category=EntityCategory.DIAGNOSTIC,
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
        GafctlSensor(coordinator, entry, description)
        for description in SENSORS
        if description.key in keys
    )


class GafctlSensor(GafctlEntity, SensorEntity):
    entity_description: GafctlSensorDescription

    def __init__(
        self,
        coordinator: GafctlCoordinator,
        entry: GafctlConfigEntry,
        description: GafctlSensorDescription,
    ) -> None:
        super().__init__(coordinator, entry, description.key)
        self.entity_description = description

    @property
    def native_value(self) -> JsonValue:
        state = self.state_values
        return state.get(self.entity_description.value_key) if state else None

    @property
    def available(self) -> bool:
        return bool(
            super().available
            and self.coordinator.http_state_owned
            and self.coordinator.data
            and self.coordinator.data.get("available")
            and self.coordinator.data.get("state") is not None
        )

    @property
    def extra_state_attributes(self) -> JsonObject:
        return self.reading_attributes
