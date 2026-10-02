"""Reported controller flags and the cloud running estimate."""

from collections.abc import Callable
from dataclasses import dataclass

from homeassistant.components.binary_sensor import (
    BinarySensorEntity,
    BinarySensorEntityDescription,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity
from .models import JsonObject, Readings


@dataclass(frozen=True, kw_only=True)
class GafctlBinaryDescription(BinarySensorEntityDescription):
    value: Callable[[Readings], bool | None]
    provenance: str = "reported"


DESCRIPTIONS = (
    GafctlBinaryDescription(
        key="controller_fan_flag",
        value=lambda readings: readings.controller_fan_flag,
        name="Controller fan flag",
        provenance="controller",
    ),
    GafctlBinaryDescription(
        key="running_estimate",
        value=lambda readings: readings.running_estimate,
        name="Running estimate",
        provenance="inferred",
    ),
    GafctlBinaryDescription(
        key="ota_in_progress",
        value=lambda readings: readings.ota_in_progress,
        name="OTA in progress",
    ),
    GafctlBinaryDescription(
        key="automatic_mode",
        value=lambda readings: readings.automatic_mode,
        name="Automatic mode",
    ),
    GafctlBinaryDescription(
        key="timer_mode", value=lambda readings: readings.timer_mode, name="Timer mode"
    ),
    GafctlBinaryDescription(
        key="manual_mode",
        value=lambda readings: readings.manual_mode,
        name="Manual mode",
    ),
    GafctlBinaryDescription(
        key="humidity_monitor",
        value=lambda readings: readings.humidity_monitor,
        name="Humidity monitoring",
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: GafctlCoordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("binary_sensor", set())
    async_add_entities(
        GafctlBinarySensor(coordinator, entry, description)
        for description in DESCRIPTIONS
        if description.key in keys
    )


class GafctlBinarySensor(GafctlEntity, BinarySensorEntity):
    """A reported boolean; unknown values stay unknown."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC
    entity_description: GafctlBinaryDescription

    def __init__(
        self,
        coordinator: GafctlCoordinator,
        entry: GafctlConfigEntry,
        description: GafctlBinaryDescription,
    ) -> None:
        super().__init__(coordinator, entry, description.key)
        self.entity_description = description

    @property
    def is_on(self) -> bool | None:
        state = self.state_values
        value = self.entity_description.value(state) if state else None
        return value if type(value) is bool else None

    @property
    def available(self) -> bool:
        data = self.coordinator.data
        return bool(
            super().available
            and self.coordinator.http_state_owned
            and data is not None
            and data.available is True
            and data.freshness == "fresh"
            and data.state is not None
        )

    @property
    def extra_state_attributes(self) -> JsonObject:
        return {
            "provenance": self.entity_description.provenance
        } | self.reading_attributes
