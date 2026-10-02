"""Immutable device data from the Gafctl API."""

from dataclasses import dataclass
from typing import ClassVar, Literal

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
class LegacySettings:
    backend: ClassVar[Literal["legacy_ble"]] = "legacy_ble"
    mode: str | None = None
    controller_fan_on: bool | None = None
    automatic_temperature_tenths_f: int | None = None
    automatic_humidity_tenths_percent: int | None = None
    timer_remaining_minutes: int | None = None
    timer_original_minutes: int | None = None

    @property
    def automatic_temperature_f(self) -> float | None:
        value = self.automatic_temperature_tenths_f
        return value / 10 if value is not None else None

    @property
    def automatic_humidity_percent(self) -> float | None:
        value = self.automatic_humidity_tenths_percent
        return value / 10 if value is not None else None

    @property
    def timer_duration_minutes(self) -> int | None:
        return self.timer_original_minutes


@dataclass(frozen=True, slots=True, kw_only=True)
class QuickConnectSettings:
    backend: ClassVar[Literal["quick_connect"]] = "quick_connect"
    mode: str = "unknown"
    automatic_temperature_f: int | None = None
    automatic_humidity_percent: int | None = None
    timer_duration_minutes: int | None = None
    humidity_monitor: bool | None = None

    def is_mode(self, mode: str) -> bool | None:
        return None if self.mode in {"unknown", "conflicting"} else self.mode == mode


@dataclass(frozen=True, slots=True, kw_only=True)
class Diagnostics:
    firmware_version: str | None = None
    signal_strength_raw: str | None = None
    verified_raw: str | None = None
    ota_in_progress: bool | None = None


@dataclass(frozen=True, slots=True, kw_only=True)
class Readings:
    settings: LegacySettings | QuickConnectSettings
    temperature_f: float | None = None
    humidity_percent: float | None = None
    estimated_running: bool | None = None
    diagnostics: Diagnostics = Diagnostics()


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
