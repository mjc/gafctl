"""Reported controller flags and the running estimate."""

from homeassistant.components.binary_sensor import BinarySensorEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers.entity_platform import AddEntitiesCallback

from .controls import entity_keys, mode_is
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .entity import GafctlReadingEntity
from .models import JsonObject
from .readings import BINARY_FIELDS


async def async_setup_entry(
    hass: HomeAssistant,
    entry: GafctlConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator = entry.runtime_data
    keys = entity_keys(coordinator.device).get("binary_sensor", set())
    async_add_entities(
        GafctlBinarySensor(coordinator, key)
        for key in BINARY_FIELDS[entry.data["backend"]]
        if key in keys
    )


class GafctlBinarySensor(GafctlReadingEntity, BinarySensorEntity):
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator, key)
        self._attr_name, self._path, self._provenance = BINARY_FIELDS[
            coordinator.entry.data["backend"]
        ][key]

    @property
    def is_on(self) -> bool | None:
        value = self.reading_value(self._path)
        if self._key.endswith("_mode"):
            return mode_is(value, self._key.removesuffix("_mode"))
        return value

    @property
    def extra_state_attributes(self) -> JsonObject:
        return {"provenance": self._provenance} | self.reading_attributes
