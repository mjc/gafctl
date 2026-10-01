"""Inferred QuickConnect running state."""

from typing import Any

from homeassistant.components.binary_sensor import BinarySensorEntity
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import UpdraftCoordinator
from .client import entity_keys
from .const import DOMAIN


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: UpdraftCoordinator = hass.data[DOMAIN][entry.entry_id]
    if "running_estimate" in entity_keys(coordinator.device).get(
        "binary_sensor", set()
    ):
        async_add_entities([UpdraftRunningEstimate(coordinator, entry)])


class UpdraftRunningEstimate(
    CoordinatorEntity[UpdraftCoordinator], BinarySensorEntity
):
    """Running estimate inferred by the service from current reported state."""

    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_has_entity_name = True
    _attr_name = "Running estimate"

    def __init__(self, coordinator: UpdraftCoordinator, entry: ConfigEntry) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._attr_unique_id = f"{entry.unique_id}_running_estimate"

    @property
    def is_on(self) -> bool | None:
        state = self.coordinator.data.get("state") if self.coordinator.data else None
        value = state.get("running_estimate") if state else None
        return value if isinstance(value, bool) else None

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
    def extra_state_attributes(self) -> dict[str, Any]:
        data = self.coordinator.data or {}
        return {
            "provenance": "inferred",
            "freshness": data.get("freshness"),
            "observed_at_unix_ms": data.get("observed_at_unix_ms"),
            "last_error": data.get("last_error"),
        }

    @property
    def device_info(self) -> dr.DeviceInfo:
        return dr.DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=self.coordinator.device.get("name", "GAF Vent"),
            manufacturer="GAF",
            model="GAF QuickConnect Vent",
        )
