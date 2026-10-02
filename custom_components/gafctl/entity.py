"""Identity and device metadata shared by Gafctl entities."""

from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .client import JsonObject
from .const import DOMAIN
from .coordinator import GafctlConfigEntry, GafctlCoordinator


class GafctlEntity(CoordinatorEntity[GafctlCoordinator]):
    _attr_has_entity_name = True

    def __init__(
        self, coordinator: GafctlCoordinator, entry: GafctlConfigEntry, key: str
    ) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._attr_unique_id = f"{entry.unique_id}_{key}"

    @property
    def state_values(self) -> JsonObject:
        return (self.coordinator.data or {}).get("state") or {}

    @property
    def reading_attributes(self) -> JsonObject:
        data = self.coordinator.data or {}
        return {
            key: data.get(key)
            for key in ("freshness", "observed_at_unix_ms", "last_error")
        }

    @property
    def device_info(self) -> DeviceInfo:
        device = self.coordinator.device
        return DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=device.get("name", "GAF Vent"),
            manufacturer="GAF",
            model="GAF Wi-Fi Vent"
            if device["backend"] == "legacy_ble"
            else "GAF QuickConnect Vent",
            sw_version=self.state_values.get("firmware_version"),
        )
