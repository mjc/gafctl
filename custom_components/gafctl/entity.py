"""Identity and device metadata shared by Gafctl entities."""

from collections.abc import Iterator
from contextlib import contextmanager

from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import GafctlCoordinator
from .models import ApiError, JsonObject, JsonValue, Readings


@contextmanager
def translate_api_errors() -> Iterator[None]:
    """Translate API errors into Home Assistant errors."""
    try:
        yield
    except ApiError as error:
        raise HomeAssistantError(str(error)) from error


class GafctlEntity(CoordinatorEntity[GafctlCoordinator]):
    _attr_has_entity_name = True

    def __init__(self, coordinator: GafctlCoordinator, key: str) -> None:
        super().__init__(coordinator)
        self._entry = coordinator.entry
        self._key = key
        self._attr_unique_id = f"{coordinator.entry.unique_id}_{key}"

    @property
    def state_values(self) -> Readings | None:
        data = self.coordinator.data
        return data["state"] if data else None

    @property
    def reading_attributes(self) -> JsonObject:
        data = self.coordinator.data
        return {
            "freshness": (
                "fresh"
                if data["available"]
                else "unknown"
                if data["inventory_status"] == "unknown"
                else "stale"
            )
            if data
            else None,
            "observed_at_unix_ms": data["state"]["provenance"]["observed_at_unix_ms"]
            if data and data["state"]
            else None,
            "last_error": data["last_error"] if data else None,
        }

    @property
    def device_info(self) -> DeviceInfo:
        device = self.coordinator.device
        readings = self.state_values
        diagnostics = readings["diagnostics"] if readings else None
        return DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=device["name"] if device else self._entry.title,
            manufacturer="GAF",
            model="GAF Wi-Fi Vent"
            if self._entry.data["backend"] == "legacy_ble"
            else "GAF QuickConnect Vent",
            sw_version=diagnostics["firmware_version"] if diagnostics else None,
        )


class GafctlReadingEntity(GafctlEntity):
    def reading_value(self, path: tuple[str, ...]) -> JsonValue:
        readings = self.state_values
        if readings is None:
            return None
        if len(path) == 1:
            return readings[path[0]]
        section, field = path
        values = readings[section]
        return values.get(field) if values is not None else None

    @property
    def available(self) -> bool:
        return super().available and self.coordinator.current_readings is not None

    @property
    def extra_state_attributes(self) -> JsonObject:
        return self.reading_attributes
