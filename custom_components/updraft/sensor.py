"""Read-only GAF state and diagnostic sensors."""

from dataclasses import dataclass
from typing import Any

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorEntityDescription,
    SensorStateClass,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory, PERCENTAGE, UnitOfTemperature, UnitOfTime
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import UpdraftCoordinator
from .client import entity_keys
from .const import DOMAIN


@dataclass(frozen=True, kw_only=True)
class UpdraftSensorDescription(SensorEntityDescription):
    value_key: str


SENSORS = (
    UpdraftSensorDescription(
        key="temperature",
        name="Ambient temperature",
        value_key="temperature_f",
        device_class=SensorDeviceClass.TEMPERATURE,
        native_unit_of_measurement=UnitOfTemperature.FAHRENHEIT,
        state_class=SensorStateClass.MEASUREMENT,
    ),
    UpdraftSensorDescription(
        key="humidity",
        name="Relative humidity",
        value_key="humidity_percent",
        device_class=SensorDeviceClass.HUMIDITY,
        native_unit_of_measurement=PERCENTAGE,
        state_class=SensorStateClass.MEASUREMENT,
    ),
    UpdraftSensorDescription(
        key="mode",
        name="Controller mode",
        value_key="mode",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    UpdraftSensorDescription(
        key="controller_fan_flag",
        name="Controller fan flag",
        value_key="controller_fan_flag",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    UpdraftSensorDescription(
        key="firmware_version",
        name="Firmware version",
        value_key="firmware_version",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    UpdraftSensorDescription(
        key="automatic_temperature_threshold",
        name="Automatic temperature threshold",
        value_key="automatic_temperature_threshold_f",
        device_class=SensorDeviceClass.TEMPERATURE,
        native_unit_of_measurement=UnitOfTemperature.FAHRENHEIT,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    UpdraftSensorDescription(
        key="automatic_humidity_threshold",
        name="Automatic humidity threshold",
        value_key="automatic_humidity_threshold_percent",
        native_unit_of_measurement=PERCENTAGE,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    UpdraftSensorDescription(
        key="timer_remaining",
        name="Timer remaining",
        value_key="timer_remaining_minutes",
        native_unit_of_measurement=UnitOfTime.MINUTES,
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
    UpdraftSensorDescription(
        key="humidity_monitor",
        name="Humidity monitoring",
        value_key="humidity_monitor",
        entity_category=EntityCategory.DIAGNOSTIC,
    ),
)

async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: UpdraftCoordinator = hass.data[DOMAIN][entry.entry_id]
    keys = entity_keys(coordinator.device).get("sensor", set())
    async_add_entities(
        UpdraftSensor(coordinator, entry, description)
        for description in SENSORS
        if description.key in keys
    )


class UpdraftSensor(CoordinatorEntity[UpdraftCoordinator], SensorEntity):
    entity_description: UpdraftSensorDescription
    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: UpdraftCoordinator,
        entry: ConfigEntry,
        description: UpdraftSensorDescription,
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = description
        self._entry = entry
        self._attr_unique_id = f"{entry.unique_id}_{description.key}"

    @property
    def native_value(self) -> Any:
        state = self.coordinator.data.get("state") if self.coordinator.data else None
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
    def extra_state_attributes(self) -> dict[str, Any]:
        data = self.coordinator.data or {}
        return {
            "freshness": data.get("freshness"),
            "observed_at_unix_ms": data.get("observed_at_unix_ms"),
            "last_error": data.get("last_error"),
        }

    @property
    def device_info(self) -> dr.DeviceInfo:
        state = self.coordinator.data.get("state") if self.coordinator.data else None
        return dr.DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=self.coordinator.device.get("name", "GAF Vent"),
            manufacturer="GAF",
            model=(
                "GAF QuickConnect Vent"
                if self.coordinator.device["backend"] == "quick_connect"
                else "GAF Wi-Fi Vent"
            ),
            sw_version=state.get("firmware_version") if state else None,
        )
