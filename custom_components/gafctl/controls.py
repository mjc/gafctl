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


CONTROL_PRESETS = frozenset(
    {
        "automatic105_f30_percent",
        "automatic105_1_f30_1_percent",
        "timer_clear",
        "timer_one_minute",
    }
)


QUICKCONNECT_MODES = frozenset({"off", "automatic", "timer", "manual"})


QUICKCONNECT_NUMBER_RANGES = MappingProxyType(
    {
        "automatic_temperature": (90, 120, 1),
        "automatic_humidity": (30, 80, 1),
        "timer_duration": (30, 360, 30),
    }
)


LEGACY_NUMBER_RANGES = MappingProxyType(
    {
        "automatic_temperature": (90, 120, 1),
        "automatic_humidity": (30, 80, 1),
        "timer_duration": (0, 360, 1),
    }
)


LEGACY_NUMBER_COMMANDS = MappingProxyType(
    {
        "automatic_temperature": ("legacy_automatic_temperature", "temperature_f"),
        "automatic_humidity": ("legacy_automatic_humidity", "humidity_percent"),
        "timer_duration": ("legacy_timer", "minutes"),
    }
)


QUICKCONNECT_NUMBER_COMMANDS = MappingProxyType(
    {
        "automatic_temperature": (
            "quick_connect_automatic_temperature",
            "temperature_f",
        ),
        "automatic_humidity": ("quick_connect_automatic_humidity", "humidity_percent"),
        "timer_duration": ("quick_connect_timer_duration", "minutes"),
    }
)


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
    capability: str
    command_kind: str
    command_field: str
    reading: Callable[[Readings], float | int | None]
    minimum: int
    maximum: int
    step: int

    def validate(self, value: object) -> int:
        if (
            not _is_finite_number(value)
            or not self.minimum <= value <= self.maximum
            or value != int(value)
            or (value - self.minimum) % self.step != 0
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
        if self.capability == "quick_connect_targets" and not (
            _integer_in_range(readings.automatic_temperature_f, 90, 120)
            and _integer_in_range(readings.automatic_humidity_percent, 30, 80)
        ):
            return False
        return (
            _integer_in_range(value, self.minimum, self.maximum)
            and (value - self.minimum) % self.step == 0
        )

    def command(self, value: int) -> JsonObject:
        return {"kind": self.command_kind, self.command_field: value}


NUMBER_READINGS: Mapping[
    Backend, Mapping[str, Callable[[Readings], float | int | None]]
] = MappingProxyType(
    {
        "legacy_ble": MappingProxyType(
            {
                "automatic_temperature": lambda readings: (
                    readings.automatic_temperature_threshold_f
                ),
                "automatic_humidity": lambda readings: (
                    readings.automatic_humidity_threshold_percent
                ),
                "timer_duration": lambda readings: readings.timer_original_minutes,
            }
        ),
        "quick_connect": MappingProxyType(
            {
                "automatic_temperature": lambda readings: (
                    readings.automatic_temperature_f
                ),
                "automatic_humidity": lambda readings: (
                    readings.automatic_humidity_percent
                ),
                "timer_duration": lambda readings: readings.timer_duration_minutes,
            }
        ),
    }
)


def number_controls(backend: Backend) -> tuple[NumberControl, ...]:
    commands, ranges = (
        (LEGACY_NUMBER_COMMANDS, LEGACY_NUMBER_RANGES)
        if backend == "legacy_ble"
        else (QUICKCONNECT_NUMBER_COMMANDS, QUICKCONNECT_NUMBER_RANGES)
    )
    return tuple(
        NumberControl(
            key=key,
            backend=backend,
            capability="quick_connect_targets"
            if backend == "quick_connect" and key != "timer_duration"
            else kind,
            command_kind=kind,
            command_field=field,
            reading=NUMBER_READINGS[backend][key],
            minimum=ranges[key][0],
            maximum=ranges[key][1],
            step=ranges[key][2],
        )
        for key, (kind, field) in commands.items()
    )


THRESHOLDS = MappingProxyType(
    {
        "automatic105_f30_percent": (105.0, 30.0),
        "automatic105_1_f30_1_percent": (105.1, 30.1),
    }
)


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
    if backend == "legacy_ble":
        number_keys = {
            key
            for key, (capability, _) in LEGACY_NUMBER_COMMANDS.items()
            if capability in command_kinds
        }
        if number_keys:
            entities["number"] = number_keys
    if backend == "quick_connect":
        if "quick_connect_mode" in command_kinds:
            entities["select"] = {"mode"}
            entities["switch"] = {"automatic_mode", "timer_mode", "manual_mode"}
            entities.setdefault("button", set()).add("all_off")
        number_keys = {
            key
            for key, capability in (
                ("automatic_temperature", "quick_connect_targets"),
                ("automatic_humidity", "quick_connect_targets"),
                ("timer_duration", "quick_connect_timer_duration"),
            )
            if capability in command_kinds
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
    for commands, ranges in (
        (LEGACY_NUMBER_COMMANDS, LEGACY_NUMBER_RANGES),
        (QUICKCONNECT_NUMBER_COMMANDS, QUICKCONNECT_NUMBER_RANGES),
    ):
        for key, (capability, field) in commands.items():
            if kind != capability or set(command) != {"kind", field}:
                continue
            minimum, maximum, step = ranges[key]
            value = command[field]
            if (
                _integer_in_range(value, minimum, maximum)
                and (value - minimum) % step == 0
            ):
                return dict(command)
    if (
        kind == "quick_connect_targets"
        and set(command)
        == {
            "kind",
            "temperature_f",
            "humidity_percent",
        }
        and all(
            _integer_in_range(command[field], *QUICKCONNECT_NUMBER_RANGES[key][:2])
            for key, field in (
                ("automatic_temperature", "temperature_f"),
                ("automatic_humidity", "humidity_percent"),
            )
        )
    ):
        return dict(command)
    raise ApiError("invalid device control command")


def _integer_in_range(value: object, minimum: int, maximum: int) -> TypeGuard[int]:
    return type(value) is int and minimum <= value <= maximum


def timer_control_preset(state: Readings) -> str | None:
    presets: Mapping[tuple[int | None, int | None], str] = {
        (0, 0): "timer_clear",
        (1, 1): "timer_one_minute",
    }
    return presets.get((state.timer_remaining_minutes, state.timer_original_minutes))


def _is_finite_number(value: object) -> TypeGuard[int | float]:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False
