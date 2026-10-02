"""Reported controller flags and the cloud running estimate."""

from dataclasses import dataclass

from homeassistant.components.binary_sensor import (
    BinarySensorEntity,
    BinarySensorEntityDescription,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .client import JsonObject, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlEntity


@dataclass(frozen=True, kw_only=True)
class GafctlBinaryDescription(BinarySensorEntityDescription):
    provenance: str = "reported"


DESCRIPTIONS = (
    GafctlBinaryDescription(
        key="controller_fan_flag", name="Controller fan flag", provenance="controller"
    ),
    GafctlBinaryDescription(
        key="running_estimate", name="Running estimate", provenance="inferred"
    ),
    GafctlBinaryDescription(key="ota_in_progress", name="OTA in progress"),
    GafctlBinaryDescription(key="automatic_mode", name="Automatic mode"),
    GafctlBinaryDescription(key="timer_mode", name="Timer mode"),
    GafctlBinaryDescription(key="manual_mode", name="Manual mode"),
    GafctlBinaryDescription(key="humidity_monitor", name="Humidity monitoring"),
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
        value = state.get(self.entity_description.key)
        return value if type(value) is bool else None

    @property
    def available(self) -> bool:
        data = self.coordinator.data or {}
        return bool(
            super().available
            and self.coordinator.http_state_owned
            and data.get("available") is True
            and data.get("freshness") == "fresh"
            and data.get("state") is not None
        )

    @property
    def extra_state_attributes(self) -> JsonObject:
        return {
            "provenance": self.entity_description.provenance
        } | self.reading_attributes
