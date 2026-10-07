"""Advertised entities, bounded commands and control readback."""

import math
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Literal, TypeGuard

from .models import ApiError, Backend, Device, DeviceState, JsonObject, Readings
from .readings import BINARY_FIELDS, MODE_LABELS, SENSORS

CONTROL_HTTP_STATUSES = {
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
QUICKCONNECT_MODES = frozenset(MODE_LABELS)


@dataclass(frozen=True, slots=True)
class NumberControl:
    key: Literal["automatic_temperature", "automatic_humidity", "timer_duration"]
    backend: Backend
    minimum: int
    maximum: int
    step: int = 1

    @property
    def command_kind(self) -> str:
        prefix = "legacy" if self.backend == "legacy_ble" else "quick_connect"
        return f"{prefix}_{self.key}"

    @property
    def command_field(self) -> str:
        return {
            "automatic_temperature": "temperature_f",
            "automatic_humidity": "humidity_percent",
            "timer_duration": "minutes",
        }[self.key]

    def reading(self, state: DeviceState | None) -> float | int | None:
        if state is None or state["state"] is None:
            return None
        settings = state["state"]["settings"]
        match self.key:
            case "automatic_temperature":
                return (
                    tenths(settings["automatic_temperature_tenths_f"])
                    if settings["backend"] == "legacy_ble"
                    else settings["automatic_temperature_f"]
                )
            case "automatic_humidity":
                return (
                    tenths(settings["automatic_humidity_tenths_percent"])
                    if settings["backend"] == "legacy_ble"
                    else settings["automatic_humidity_percent"]
                )
            case "timer_duration":
                return (
                    state["timer_duration_minutes"]
                    if settings["backend"] == "legacy_ble"
                    else settings["timer_duration_minutes"]
                )

    @property
    def capability(self) -> str:
        if self.backend == "quick_connect" and self.key != "timer_duration":
            return "quick_connect_targets"
        return self.command_kind

    def accepts(self, value: object) -> TypeGuard[int]:
        return (
            type(value) is int
            and self.minimum <= value <= self.maximum
            and ((value - self.minimum) % self.step == 0)
        )

    def validate(self, value: object) -> int:
        if (
            not _is_finite_number(value)
            or value != int(value)
            or (not self.accepts(int(value)))
        ):
            raise ApiError("value is outside the supported device range")
        return int(value)

    def current_supported(self, state: DeviceState | None) -> bool:
        value = self.reading(state)
        if not _is_finite_number(value):
            return False
        if self.backend == "legacy_ble" and self.key == "timer_duration":
            return self.accepts(value)
        if self.backend == "legacy_ble":
            return self.minimum <= value <= self.maximum or (
                self.key == "automatic_humidity" and value == 100
            )
        return self.accepts(value) and (
            self.capability != "quick_connect_targets"
            or all(
                target.accepts(target.reading(state))
                for target in NUMBER_CONTROLS[self.backend]
                if target.capability == self.capability
            )
        )

    def command(self, value: int) -> JsonObject:
        return {"kind": self.command_kind, self.command_field: value}


BACKENDS: tuple[Backend, ...] = ("legacy_ble", "quick_connect")
NUMBER_CONTROLS: Mapping[Backend, tuple[NumberControl, ...]] = {
    backend: (
        NumberControl("automatic_temperature", backend, 90, 120),
        NumberControl("automatic_humidity", backend, 30, 80),
        NumberControl("timer_duration", backend, 0, 360)
        if backend == "legacy_ble"
        else NumberControl("timer_duration", backend, 30, 360, 30),
    )
    for backend in BACKENDS
}


def select_device(devices: list[Device], device_id: str) -> Device:
    """Find the selected device and check its HTTP ownership."""
    device = next((item for item in devices if item["id"] == device_id), None)
    if device is None or not entity_keys(device):
        raise ApiError("selected device is unavailable")
    return device


def entity_keys(device: Device | None) -> dict[str, set[str]]:
    """Return entity keys for the device's ownership and capabilities."""
    if device is None or device["state_source"] != "http":
        return {}
    commands = command_kinds(device)
    backend = device["backend"]
    entities: dict[str, set[str]] = {}
    if device["capabilities"]["read_state"]:
        entities = {
            "sensor": set(SENSORS[backend]),
            "binary_sensor": set(BINARY_FIELDS[backend]),
        }
    if backend == "legacy_ble" and "legacy_mode" in commands:
        entities.setdefault("select", set()).add("mode")
    if backend == "quick_connect" and "quick_connect_mode" in commands:
        entities["select"] = {"mode"}
        entities["switch"] = {f"{mode}_mode" for mode in MODE_LABELS if mode != "off"}
        entities.setdefault("button", set()).add("all_off")
    number_keys: set[str] = {
        control.key
        for control in NUMBER_CONTROLS[backend]
        if control.capability in commands
    }
    if number_keys:
        entities["number"] = number_keys
    return entities


def _is_finite_number(value: object) -> TypeGuard[int | float]:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def command_kinds(device: Device) -> set[str]:
    return {command["kind"] for command in device["capabilities"]["commands"]}


def mode_is(reported: object, expected: str) -> bool | None:
    return (
        reported == expected
        if isinstance(reported, str) and reported in QUICKCONNECT_MODES
        else None
    )


def legacy_mode(readings: Readings) -> str | None:
    settings = readings["settings"]
    if settings["backend"] != "legacy_ble":
        return None
    if settings["mode"] == "automatic":
        return "automatic"
    if settings["mode"] != "timer":
        return None
    fan = settings["controller_fan_on"]
    return "timer" if fan is True else "off" if fan is False else None


def tenths(value: int | None) -> float | None:
    return value / 10 if value is not None else None
