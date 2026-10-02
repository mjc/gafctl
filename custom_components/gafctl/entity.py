"""Identity and device metadata shared by Gafctl entities."""

from collections.abc import Iterator
from contextlib import contextmanager

from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .const import DOMAIN
from .coordinator import GafctlConfigEntry, GafctlCoordinator
from .models import ApiError, JsonObject, Readings


@contextmanager
def translate_api_errors() -> Iterator[None]:
    """Translate API errors into Home Assistant errors."""
    try:
        yield
    except ApiError as error:
        raise HomeAssistantError(str(error)) from error


class GafctlEntity(CoordinatorEntity[GafctlCoordinator]):
    _attr_has_entity_name = True

    def __init__(
        self, coordinator: GafctlCoordinator, entry: GafctlConfigEntry, key: str
    ) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._attr_unique_id = f"{entry.unique_id}_{key}"

    @property
    def state_values(self) -> Readings | None:
        data = self.coordinator.data
        return data.state if data else None

    @property
    def reading_attributes(self) -> JsonObject:
        data = self.coordinator.data
        return {
            "freshness": data.freshness if data else None,
            "observed_at_unix_ms": data.observed_at_unix_ms if data else None,
            "last_error": data.last_error if data else None,
        }

    @property
    def device_info(self) -> DeviceInfo:
        device = self.coordinator.device
        return DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=device.name,
            manufacturer="GAF",
            model="GAF Wi-Fi Vent"
            if device.backend == "legacy_ble"
            else "GAF QuickConnect Vent",
            sw_version=self.state_values.firmware_version
            if self.state_values
            else None,
        )
