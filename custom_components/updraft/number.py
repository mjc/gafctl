"""Capability-driven device setting controls."""

import math
from typing import Any

from homeassistant.components.number import NumberEntity, NumberMode
from homeassistant.config_entries import ConfigEntry
from homeassistant.const import PERCENTAGE, UnitOfTemperature, UnitOfTime
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import UpdraftCoordinator
from .client import ApiError, LEGACY_NUMBER_COMMANDS, LEGACY_NUMBER_RANGES, QUICKCONNECT_NUMBER_RANGES, entity_keys
from .const import DOMAIN


CONTROLS = (
    {
        "capability": "quick_connect_targets",
        "key": "automatic_temperature",
        "name": "Target temperature",
        "state_key": "automatic_temperature_f",
        "minimum": QUICKCONNECT_NUMBER_RANGES["automatic_temperature"][0],
        "maximum": QUICKCONNECT_NUMBER_RANGES["automatic_temperature"][1],
        "step": QUICKCONNECT_NUMBER_RANGES["automatic_temperature"][2],
        "unit": UnitOfTemperature.FAHRENHEIT,
    },
    {
        "capability": "quick_connect_targets",
        "key": "automatic_humidity",
        "name": "Target humidity",
        "state_key": "automatic_humidity_percent",
        "minimum": QUICKCONNECT_NUMBER_RANGES["automatic_humidity"][0],
        "maximum": QUICKCONNECT_NUMBER_RANGES["automatic_humidity"][1],
        "step": QUICKCONNECT_NUMBER_RANGES["automatic_humidity"][2],
        "unit": PERCENTAGE,
    },
    {
        "capability": "quick_connect_timer_duration",
        "key": "timer_duration",
        "name": "Timer duration",
        "state_key": "timer_duration_minutes",
        "minimum": QUICKCONNECT_NUMBER_RANGES["timer_duration"][0],
        "maximum": QUICKCONNECT_NUMBER_RANGES["timer_duration"][1],
        "step": QUICKCONNECT_NUMBER_RANGES["timer_duration"][2],
        "unit": UnitOfTime.MINUTES,
    },
)
LEGACY_STATE_KEYS = {
    "automatic_temperature": "automatic_temperature_threshold_f",
    "automatic_humidity": "automatic_humidity_threshold_percent",
    "timer_duration": "timer_original_minutes",
}
LEGACY_CONTROLS = tuple(
    control | {
        "capability": LEGACY_NUMBER_COMMANDS[control["key"]][0],
        "state_key": LEGACY_STATE_KEYS[control["key"]],
        "minimum": LEGACY_NUMBER_RANGES[control["key"]][0],
        "maximum": LEGACY_NUMBER_RANGES[control["key"]][1],
        "step": LEGACY_NUMBER_RANGES[control["key"]][2],
    }
    for control in CONTROLS
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    coordinator: UpdraftCoordinator = hass.data[DOMAIN][entry.entry_id]
    async_add_entities(
        UpdraftNumber(coordinator, entry, control)
        for control in (LEGACY_CONTROLS if coordinator.device["backend"] == "legacy_ble" else CONTROLS)
        if control["key"] in entity_keys(coordinator.device).get("number", set())
    )


class UpdraftNumber(CoordinatorEntity[UpdraftCoordinator], NumberEntity):
    """One setting advertised by the selected device."""

    _attr_mode = NumberMode.BOX
    _attr_has_entity_name = True
    _attr_entity_category = None

    def __init__(
        self,
        coordinator: UpdraftCoordinator,
        entry: ConfigEntry,
        control: dict[str, Any],
    ) -> None:
        super().__init__(coordinator)
        self._entry = entry
        self._control = control
        self._attr_name = control["name"]
        self._attr_unique_id = f"{entry.unique_id}_{control['key']}"
        self._attr_native_min_value = control["minimum"]
        self._attr_native_max_value = control["maximum"]
        self._attr_native_step = control["step"]
        self._attr_native_unit_of_measurement = control["unit"]

    @property
    def native_value(self) -> float | None:
        state = self.coordinator.data.get("state") if self.coordinator.data else None
        value = state.get(self._control["state_key"]) if state else None
        if (
            self.coordinator.device["backend"] == "legacy_ble"
            and self._control["key"] == "timer_duration"
            and not _valid_value(value, 0, 360, 1)
        ):
            return None
        return value if _finite_number(value) else None

    @property
    def available(self) -> bool:
        data = self.coordinator.data or {}
        state = data.get("state") or {}
        value = state.get(self._control["state_key"])
        current_targets_valid = self._control["capability"] != "quick_connect_targets" or all(
            _valid_value(
                state.get(key),
                90 if key == "automatic_temperature_f" else 30,
                120 if key == "automatic_temperature_f" else 80,
                1,
            )
            for key in ("automatic_temperature_f", "automatic_humidity_percent")
        )
        return bool(
            super().available
            and self.coordinator.http_command_owned
            and self.coordinator.supports(self._control["capability"])
            and data.get("available") is True
            and data.get("freshness") == "fresh"
            and self._current_value_supported(value)
            and current_targets_valid
        )

    def _current_value_supported(self, value: Any) -> bool:
        control = self._control
        if self.coordinator.device["backend"] == "legacy_ble":
            return _finite_number(value) and (
                control["minimum"] <= value <= control["maximum"]
                or control["key"] == "automatic_humidity" and value == 100
                or control["key"] == "timer_duration" and value == 600
            )
        return _valid_value(value, control["minimum"], control["maximum"], control["step"])

    async def async_set_native_value(self, value: float) -> None:
        control = self._control
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not control["minimum"] <= value <= control["maximum"]
            or not float(value).is_integer()
            or (value - control["minimum"]) % control["step"]
        ):
            raise HomeAssistantError("Value is outside the supported device range")
        async with self.coordinator.command_lock:
            try:
                await self.coordinator.async_refresh()
            except Exception as error:
                raise HomeAssistantError("Could not refresh device state") from error
            data = self.coordinator.data or {}
            state = data.get("state")
            if (
                not self.coordinator.last_update_success
                or data.get("available") is not True
                or data.get("freshness") != "fresh"
                or not isinstance(state, dict)
                or not self.available
            ):
                raise HomeAssistantError("The selected device has no fresh state")
            try:
                command = self._command(int(value))
                await self.coordinator.client.set_control(
                    self.coordinator.device_id, command
                )
            except ApiError as error:
                raise HomeAssistantError(str(error)) from error
            try:
                await self.coordinator.async_refresh()
            except Exception as error:
                raise HomeAssistantError(
                    "Control was confirmed but state refresh failed"
                ) from error
            data = self.coordinator.data or {}
            state = data.get("state") or {}
            if (
                not self.coordinator.last_update_success
                or data.get("available") is not True
                or data.get("freshness") != "fresh"
                or state.get(control["state_key"]) != int(value)
            ):
                raise HomeAssistantError(
                    "Control was confirmed but state refresh failed"
                )

    def _command(self, value: int) -> dict[str, Any]:
        key = self._control["key"]
        if self.coordinator.device["backend"] == "legacy_ble":
            kind, field = LEGACY_NUMBER_COMMANDS[key]
        else:
            kind, field = {
                "automatic_temperature": ("quick_connect_automatic_temperature", "temperature_f"),
                "automatic_humidity": ("quick_connect_automatic_humidity", "humidity_percent"),
                "timer_duration": ("quick_connect_timer_duration", "minutes"),
            }[key]
        return {"kind": kind, field: value}

    @property
    def device_info(self) -> dr.DeviceInfo:
        return dr.DeviceInfo(
            identifiers={(DOMAIN, self._entry.unique_id)},
            name=self.coordinator.device.get("name", "GAF Vent"),
            manufacturer="GAF",
            model="GAF Wi-Fi Vent" if self.coordinator.device["backend"] == "legacy_ble" else "GAF QuickConnect Vent",
        )


def _valid_value(value: Any, minimum: int, maximum: int, step: int) -> bool:
    return (
        type(value) is int
        and minimum <= value <= maximum
        and (value - minimum) % step == 0
    )


def _finite_number(value: Any) -> bool:
    return type(value) in (int, float) and math.isfinite(value)
