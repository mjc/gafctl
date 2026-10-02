"""Immutable data validated at the HTTP boundary."""

from dataclasses import dataclass
from typing import Literal

type JsonValue = (
    None | bool | int | float | str | list[JsonValue] | dict[str, JsonValue]
)


type JsonObject = dict[str, JsonValue]


type Backend = Literal["legacy_ble", "quick_connect"]


type EntitySource = Literal["http", "mqtt"]


@dataclass(frozen=True, slots=True, kw_only=True)
class Device:
    proxy_id: str
    id: str
    name: str
    backend: Backend
    read_state: bool
    commands: frozenset[str]
    owner: EntitySource


@dataclass(frozen=True, slots=True, kw_only=True)
class Readings:
    temperature_f: float | int | None = None
    humidity_percent: float | int | None = None
    mode: str | None = None
    firmware_version: str | None = None
    controller_fan_flag: bool | None = None
    automatic_temperature_threshold_f: float | None = None
    automatic_humidity_threshold_percent: float | None = None
    timer_remaining_minutes: int | None = None
    timer_original_minutes: int | None = None
    automatic_temperature_f: int | None = None
    automatic_humidity_percent: int | None = None
    timer_duration_minutes: int | None = None
    humidity_monitor: bool | None = None
    automatic_mode: bool | None = None
    timer_mode: bool | None = None
    manual_mode: bool | None = None
    running_estimate: bool | None = None
    running_estimate_provenance: str | None = None
    signal_strength_raw: str | None = None
    verified_raw: str | None = None
    ota_in_progress: bool | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class DeviceState:
    device_id: str
    backend: Backend
    available: bool
    freshness: Literal["fresh", "unknown", "stale"]
    observed_at_unix_ms: int | None
    last_error: str | None
    state: Readings | None


class ApiError(Exception):
    """Gafctl API error with a message suitable for display."""


class ControlOutcomeUnknown(ApiError):
    """The submitted command could not be confirmed."""

    def __init__(self, request_id: str) -> None:
        self.request_id = request_id
        super().__init__(
            f"Control outcome unknown for request {request_id}; read current state before sending another command"
        )
