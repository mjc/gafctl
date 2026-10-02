"""The fixed Gafctl HTTP response shapes."""

from typing import Literal, TypedDict

type Backend = Literal["legacy_ble", "quick_connect"]
type EntitySource = Literal["http", "mqtt"]
type JsonValue = (
    None | bool | int | float | str | list[JsonValue] | dict[str, JsonValue]
)
type JsonObject = dict[str, JsonValue]


class Command(TypedDict):
    kind: str


class Capabilities(TypedDict):
    read_state: bool
    commands: list[Command]


class Device(TypedDict):
    proxy_id: str
    id: str
    name: str
    backend: Backend
    capabilities: Capabilities
    state_source: EntitySource
    command_source: EntitySource


class LegacySettings(TypedDict):
    backend: Literal["legacy_ble"]
    mode: str | None
    controller_fan_on: bool | None
    automatic_temperature_tenths_f: int | None
    automatic_humidity_tenths_percent: int | None
    timer_remaining_minutes: int | None
    timer_original_minutes: int | None


class QuickConnectSettings(TypedDict):
    backend: Literal["quick_connect"]
    mode: str
    automatic_temperature_f: int | None
    automatic_humidity_percent: int | None
    timer_duration_minutes: int | None
    humidity_monitor: bool | None


class Diagnostics(TypedDict):
    firmware_version: str | None
    signal_strength_raw: str | None
    verified_raw: str | None
    ota_in_progress: bool | None


class Provenance(TypedDict):
    backend: Backend
    fetched_at_unix_ms: int | None
    observed_at_unix_ms: int | None


class Readings(TypedDict):
    temperature_f: float | None
    humidity_percent: float | None
    settings: LegacySettings | QuickConnectSettings
    estimated_running: bool | None
    diagnostics: Diagnostics | None
    provenance: Provenance


class DeviceState(TypedDict):
    id: str
    backend: Backend
    available: bool
    inventory_status: Literal["unknown", "present", "missing", "unavailable"]
    last_error: str | None
    state: Readings | None


def device_identity(device: Device) -> tuple[str, str, Backend]:
    return device["proxy_id"], device["id"], device["backend"]


class ApiError(Exception):
    """Gafctl API error with a message suitable for display."""


class ControlOutcomeUnknown(ApiError):
    def __init__(self, request_id: str) -> None:
        self.request_id = request_id
        super().__init__(
            f"Control outcome unknown for request {request_id}; read current state before sending another command"
        )
