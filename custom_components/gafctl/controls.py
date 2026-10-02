"""Advertised entities, bounded commands and control readback."""

import math
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from types import MappingProxyType
from typing import TypeGuard

from .models import ApiError, Backend, Device, JsonObject, JsonValue, Readings

CONTROL_HTTP_STATUSES = MappingProxyType(
    {
        "confirmed": 200,
        "unconfirmed": 502,
        "submitted_unconfirmed": 502,
        "readback_mismatch": 502,
        "readback_unavailable": 502,
        "rejected": 422,
        "unsupported_command": 422,
        "stale_request": 422,
        "request_id_reused": 422,
        "unknown_device": 404,
        "device_unavailable": 404,
        "backend_unavailable": 503,
        "busy": 429,
        "control_failed": 500,
        "invalid_request_id": 400,
    }
)


MODE_LABELS = MappingProxyType(
    {"off": "Off", "automatic": "Automatic", "timer": "Timer", "manual": "Manual"}
)
QUICKCONNECT_MODES = frozenset(MODE_LABELS)

QUICKCONNECT_SENSOR_KEYS = frozenset(
    {
        "temperature",
        "humidity",
        "mode",
        "firmware_version",
        "signal_strength_raw",
        "verified_raw",
    }
)


LEGACY_SENSOR_KEYS = frozenset(
    {
        "temperature",
        "humidity",
        "mode",
        "firmware_version",
        "automatic_temperature_threshold",
        "automatic_humidity_threshold",
        "timer_remaining",
        "timer_original",
    }
)


@dataclass(frozen=True, slots=True, kw_only=True)
class NumberControl:
    key: str
    backend: Backend
    command_kind: str
    command_field: str
    reading: Callable[[Readings], float | int | None]
    minimum: int
    maximum: int
    step: int

    @property
    def capability(self) -> str:
        if self.backend == "quick_connect" and self.key != "timer_duration":
            return "quick_connect_targets"
        return self.command_kind

    def accepts(self, value: object) -> TypeGuard[int]:
        return (
            type(value) is int
            and self.minimum <= value <= self.maximum
            and (value - self.minimum) % self.step == 0
        )

    def validate(self, value: object) -> int:
        if (
            not _is_finite_number(value)
            or value != int(value)
            or not self.accepts(int(value))
        ):
            raise ApiError("value is outside the supported device range")
        return int(value)

    def current_supported(self, readings: Readings) -> bool:
        value = self.reading(readings)
        if not _is_finite_number(value):
            return False
        if self.backend == "legacy_ble":
            return (
                self.minimum <= value <= self.maximum
                or self.key == "automatic_humidity"
                and value == 100
                or self.key == "timer_duration"
                and value == 600
            )
        return self.accepts(value) and (
            self.capability != "quick_connect_targets"
            or all(
                target.accepts(target.reading(readings))
                for target in NUMBER_CONTROLS[self.backend]
                if target.capability == self.capability
            )
        )

    def command(self, value: int) -> JsonObject:
        return {"kind": self.command_kind, self.command_field: value}


NUMBER_CONTROLS: Mapping[Backend, tuple[NumberControl, ...]] = MappingProxyType(
    {
        "legacy_ble": (
            NumberControl(
                key="automatic_temperature",
                backend="legacy_ble",
                command_kind="legacy_automatic_temperature",
                command_field="temperature_f",
                reading=lambda readings: readings.automatic_temperature_threshold_f,
                minimum=90,
                maximum=120,
                step=1,
            ),
            NumberControl(
                key="automatic_humidity",
                backend="legacy_ble",
                command_kind="legacy_automatic_humidity",
                command_field="humidity_percent",
                reading=lambda readings: readings.automatic_humidity_threshold_percent,
                minimum=30,
                maximum=80,
                step=1,
            ),
            NumberControl(
                key="timer_duration",
                backend="legacy_ble",
                command_kind="legacy_timer",
                command_field="minutes",
                reading=lambda readings: readings.timer_original_minutes,
                minimum=0,
                maximum=360,
                step=1,
            ),
        ),
        "quick_connect": (
            NumberControl(
                key="automatic_temperature",
                backend="quick_connect",
                command_kind="quick_connect_automatic_temperature",
                command_field="temperature_f",
                reading=lambda readings: readings.automatic_temperature_f,
                minimum=90,
                maximum=120,
                step=1,
            ),
            NumberControl(
                key="automatic_humidity",
                backend="quick_connect",
                command_kind="quick_connect_automatic_humidity",
                command_field="humidity_percent",
                reading=lambda readings: readings.automatic_humidity_percent,
                minimum=30,
                maximum=80,
                step=1,
            ),
            NumberControl(
                key="timer_duration",
                backend="quick_connect",
                command_kind="quick_connect_timer_duration",
                command_field="minutes",
                reading=lambda readings: readings.timer_duration_minutes,
                minimum=30,
                maximum=360,
                step=30,
            ),
        ),
    }
)

THRESHOLDS = MappingProxyType(
    {
        "automatic105_f30_percent": (105.0, 30.0),
        "automatic105_1_f30_1_percent": (105.1, 30.1),
    }
)


TIMER_PRESETS = MappingProxyType(
    {"timer_clear": ("Clear timer", 0), "timer_one_minute": ("1 minute", 1)}
)
CONTROL_PRESETS = frozenset((*THRESHOLDS, *TIMER_PRESETS))


def threshold_control_preset(readings: Readings) -> str | None:
    current = (
        readings.automatic_temperature_threshold_f,
        readings.automatic_humidity_threshold_percent,
    )
    return next(
        (preset for preset, thresholds in THRESHOLDS.items() if current == thresholds),
        None,
    )


def preset_matches(readings: Readings, preset: str) -> bool:
    if preset in THRESHOLDS:
        return (
            readings.mode == "automatic"
            and threshold_control_preset(readings) == preset
        )
    return (
        readings.mode == "timer"
        and timer_control_preset(readings) == preset
        and (preset != "timer_clear" or readings.controller_fan_flag is False)
    )


def select_device(devices: list[Device], device_id: str) -> Device:
    """Find the selected device and check its HTTP ownership."""
    device = next((item for item in devices if item.id == device_id), None)
    if device is None or not entity_platforms(device):
        raise ApiError("selected device is unavailable")
    return device


def entity_platforms(device: Device) -> set[str]:
    """Return HA platforms owned by this adapter for the device capabilities."""
    return set(entity_keys(device))


def entity_keys(device: Device) -> dict[str, set[str]]:
    """Return entity keys for the device's ownership and capabilities."""
    if device.owner != "http":
        return {}
    command_kinds = device.commands
    backend = device.backend
    entities: dict[str, set[str]] = {}
    if device.read_state:
        entities["button"] = {"refresh"}
        entities["sensor"] = (
            set(LEGACY_SENSOR_KEYS)
            if backend == "legacy_ble"
            else set(QUICKCONNECT_SENSOR_KEYS)
        )
        if backend == "quick_connect":
            entities["binary_sensor"] = {
                "running_estimate",
                "ota_in_progress",
                "automatic_mode",
                "timer_mode",
                "manual_mode",
                "humidity_monitor",
            }
        else:
            entities["binary_sensor"] = {"controller_fan_flag"}
    if backend == "legacy_ble" and "legacy_preset" in command_kinds:
        entities["select"] = {"automatic_thresholds", "timer"}
    if backend == "quick_connect" and "quick_connect_mode" in command_kinds:
        entities["select"] = {"mode"}
        entities["switch"] = {f"{mode}_mode" for mode in MODE_LABELS if mode != "off"}
        entities.setdefault("button", set()).add("all_off")
    number_keys = {
        control.key
        for control in NUMBER_CONTROLS[backend]
        if control.capability in command_kinds
    }
    if number_keys:
        entities["number"] = number_keys
    return entities


def _control_command(command: str | Mapping[str, JsonValue]) -> JsonObject:
    if isinstance(command, str):
        if command not in CONTROL_PRESETS:
            raise ApiError("unsupported control preset")
        return {"kind": "legacy_preset", "preset": command}
    if not isinstance(command, Mapping):
        raise ApiError("unsupported control command")
    kind = command.get("kind")
    if not isinstance(kind, str):
        raise ApiError("invalid device control command")
    mode_fields = {
        "quick_connect_mode": "mode",
        "quick_connect_conditional_off": "only_if_current",
    }
    if field := mode_fields.get(kind):
        value = command.get(field)
        if (
            set(command) == {"kind", field}
            and isinstance(value, str)
            and value in QUICKCONNECT_MODES
        ):
            return dict(command)
    for controls in NUMBER_CONTROLS.values():
        for control in controls:
            if (
                kind == control.command_kind
                and set(command) == {"kind", control.command_field}
                and control.accepts(command[control.command_field])
            ):
                return dict(command)
    targets = tuple(
        control
        for control in NUMBER_CONTROLS["quick_connect"]
        if control.capability == "quick_connect_targets"
    )
    if (
        kind == "quick_connect_targets"
        and set(command) == {"kind", *(control.command_field for control in targets)}
        and all(control.accepts(command[control.command_field]) for control in targets)
    ):
        return dict(command)
    raise ApiError("invalid device control command")


def timer_control_preset(state: Readings) -> str | None:
    current = (state.timer_remaining_minutes, state.timer_original_minutes)
    return next(
        (
            preset
            for preset, (_, duration) in TIMER_PRESETS.items()
            if current == (duration, duration)
        ),
        None,
    )


def _is_finite_number(value: object) -> TypeGuard[int | float]:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False
