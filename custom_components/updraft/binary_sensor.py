"""Reported controller flags and the cloud running estimate."""

from dataclasses import dataclass
from typing import Any

from homeassistant.components.binary_sensor import BinarySensorEntity, BinarySensorEntityDescription
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import UpdraftCoordinator
from .client import entity_keys
from .const import DOMAIN


@dataclass(frozen=True, kw_only=True)
class UpdraftBinaryDescription(BinarySensorEntityDescription):
    provenance: str = "reported"


DESCRIPTIONS = (
    UpdraftBinaryDescription(key="controller_fan_flag", name="Controller fan flag", provenance="controller"),
    UpdraftBinaryDescription(key="running_estimate", name="Running estimate", provenance="inferred"),
    UpdraftBinaryDescription(key="ota_in_progress", name="OTA in progress"),
    UpdraftBinaryDescription(key="automatic_mode", name="Automatic mode"),
    UpdraftBinaryDescription(key="timer_mode", name="Timer mode"),
    UpdraftBinaryDescription(key="manual_mode", name="Manual mode"),
    UpdraftBinaryDescription(key="humidity_monitor", name="Humidity monitoring"),
)


async def async_setup_entry(
    hass: HomeAssistant, entry: ConfigEntry, async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: UpdraftCoordinator = hass.data[DOMAIN][entry.entry_id]
    keys = entity_keys(coordinator.device).get("binary_sensor", set())
    async_add_entities(
        UpdraftBinarySensor(coordinator, entry, description)
        for description in DESCRIPTIONS if description.key in keys
    )


class UpdraftBinarySensor(CoordinatorEntity[UpdraftCoordinator], BinarySensorEntity):
    """A reported boolean; unknown values stay unknown."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_has_entity_name = True
    entity_description: UpdraftBinaryDescription

    def __init__(self, coordinator: UpdraftCoordinator, entry: ConfigEntry, description: UpdraftBinaryDescription) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self.entity_description = description
        self._attr_unique_id = f"{entry.unique_id}_{description.key}"

    @property
    def is_on(self) -> bool | None:
        state = (self.coordinator.data or {}).get("state") or {}
        value = state.get(self.entity_description.key)
        return value if type(value) is bool else None

    @property
    def available(self) -> bool:
        data = self.coordinator.data or {}
        return bool(
            super().available and self.coordinator.http_state_owned
            and data.get("available") is True and data.get("freshness") == "fresh"
            and data.get("state") is not None
        )

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        data = self.coordinator.data or {}
        return {
            "provenance": self.entity_description.provenance,
            "freshness": data.get("freshness"),
            "observed_at_unix_ms": data.get("observed_at_unix_ms"),
            "last_error": data.get("last_error"),
        }

    @property
    def device_info(self) -> dr.DeviceInfo:
        return dr.DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=self.coordinator.device.get("name", "GAF Vent"), manufacturer="GAF",
            model="GAF Wi-Fi Vent" if self.coordinator.device["backend"] == "legacy_ble" else "GAF QuickConnect Vent",
        )
