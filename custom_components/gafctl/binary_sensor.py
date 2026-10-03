"""Reported controller flags and the running estimate."""

from homeassistant.components.binary_sensor import BinarySensorEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import MODE_LABELS, entity_keys, mode_is
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlReadingEntity
from .models import JsonObject

BINARY_FIELDS = {
    "controller_fan_flag": (
        "Controller fan flag",
        ("settings", "controller_fan_on"),
        "controller",
    ),
    "running_estimate": ("Running estimate", ("estimated_running",), "inferred"),
    "ota_in_progress": (
        "OTA in progress",
        ("diagnostics", "ota_in_progress"),
        "reported",
    ),
    **{
        f"{mode}_mode": (f"{label} mode", ("settings", "mode"), "reported")
        for mode, label in MODE_LABELS.items()
        if mode != "off"
    },
    "humidity_monitor": (
        "Humidity monitoring",
        ("settings", "humidity_monitor"),
        "reported",
    ),
}


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("binary_sensor", set())
    async_add_entities(
        GafctlBinarySensor(coordinator, key) for key in BINARY_FIELDS if key in keys
    )


class GafctlBinarySensor(GafctlReadingEntity, BinarySensorEntity):
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator, key)
        self._attr_name, self._path, self._provenance = BINARY_FIELDS[key]

    @property
    def is_on(self) -> bool | None:
        value = self.reading_value(self._path)
        if self._key.endswith("_mode"):
            return mode_is(value, self._key.removesuffix("_mode"))
        return value

    @property
    def extra_state_attributes(self) -> JsonObject:
        return {"provenance": self._provenance} | self.reading_attributes
