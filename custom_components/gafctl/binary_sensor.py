"""Reported controller flags and the cloud running estimate."""

from collections.abc import Callable
from dataclasses import dataclass
from functools import partial

from homeassistant.components.binary_sensor import (
    BinarySensorEntity,
    BinarySensorEntityDescription,
)
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import MODE_LABELS, entity_keys
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlReadingEntity
from .models import JsonObject, Readings


@dataclass(frozen=True, kw_only=True)
class GafctlBinaryDescription(BinarySensorEntityDescription):
    value: Callable[[Readings], bool | None]
    provenance: str = "reported"


def mode_value(readings: Readings, *, mode: str) -> bool | None:
    settings = readings["settings"]
    return settings["mode"] == mode if settings["mode"] in MODE_LABELS else None


DESCRIPTIONS = (
    GafctlBinaryDescription(
        key="controller_fan_flag",
        value=lambda readings: (
            readings["settings"]["controller_fan_on"]
            if readings["settings"]["backend"] == "legacy_ble"
            else None
        ),
        name="Controller fan flag",
        provenance="controller",
    ),
    GafctlBinaryDescription(
        key="running_estimate",
        value=lambda readings: readings["estimated_running"],
        name="Running estimate",
        provenance="inferred",
    ),
    GafctlBinaryDescription(
        key="ota_in_progress",
        value=lambda readings: (
            readings["diagnostics"]["ota_in_progress"]
            if readings["diagnostics"]
            else None
        ),
        name="OTA in progress",
    ),
    *(
        GafctlBinaryDescription(
            key=f"{mode}_mode",
            name=f"{label} mode",
            value=partial(mode_value, mode=mode),
        )
        for mode, label in MODE_LABELS.items()
        if mode != "off"
    ),
    GafctlBinaryDescription(
        key="humidity_monitor",
        value=lambda readings: (
            readings["settings"]["humidity_monitor"]
            if readings["settings"]["backend"] == "quick_connect"
            else None
        ),
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
        GafctlBinarySensor(coordinator, description)
        for description in DESCRIPTIONS
        if description.key in keys
    )


class GafctlBinarySensor(GafctlReadingEntity, BinarySensorEntity):
    """A reported boolean; unknown values stay unknown."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC
    entity_description: GafctlBinaryDescription

    @property
    def is_on(self) -> bool | None:
        state = self.state_values
        return self.entity_description.value(state) if state else None

    @property
    def extra_state_attributes(self) -> JsonObject:
        return {
            "provenance": self.entity_description.provenance
        } | self.reading_attributes
