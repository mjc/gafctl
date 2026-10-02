"""HTTP client for the Gafctl API."""

from __future__ import annotations

import time
from collections.abc import Mapping
from contextlib import AbstractAsyncContextManager
from typing import Literal, Protocol
from urllib.parse import urljoin, urlparse
from uuid import RFC_4122, UUID, uuid4

from .controls import (
    CONTROL_HTTP_STATUSES,
    QUICKCONNECT_MODES,
    _control_command,
    _is_finite_number,
)
from .models import (
    ApiError,
    Backend,
    ControlOutcomeUnknown,
    Device,
    DeviceState,
    JsonObject,
    JsonValue,
    Readings,
)


class Response(Protocol):
    @property
    def status(self) -> int: ...

    async def json(self) -> object: ...


class Session(Protocol):
    def get(
        self, url: str, *, timeout: int
    ) -> AbstractAsyncContextManager[Response]: ...

    def post(
        self, url: str, *, timeout: int, json: JsonObject = ...
    ) -> AbstractAsyncContextManager[Response]: ...


def normalize_api_url(value: str) -> str:
    """Validate a proxy URL and return it without a trailing slash."""
    try:
        parsed = urlparse(value.strip())
        valid = (
            parsed.scheme in {"http", "https"}
            and parsed.hostname is not None
            and parsed.username is None
            and parsed.password is None
            and not parsed.query
            and not parsed.fragment
        )
    except ValueError:
        valid = False
    if not valid:
        raise ApiError("enter a valid HTTP or HTTPS address without credentials")
    return value.strip().rstrip("/")


class ApiClient:
    def __init__(self, base_url: str, session: Session) -> None:
        self._base_url = base_url.rstrip("/") + "/"
        self._session = session

    async def fetch_devices(self) -> list[Device]:
        payload = await self._get_json("api/v2/devices")
        devices = payload.get("devices") if isinstance(payload, Mapping) else None
        if not isinstance(devices, list):
            raise ApiError("proxy returned no devices")
        if not all(_valid_device(device) for device in devices):
            raise ApiError("proxy returned invalid device data")
        selected = [_device_values(_mapping(device)) for device in devices]
        identifiers = [device.id for device in selected]
        if len({device.proxy_id for device in selected}) > 1:
            raise ApiError("proxy returned inconsistent proxy identities")
        if len(identifiers) != len(set(identifiers)):
            raise ApiError("proxy returned duplicate device identifiers")
        return selected

    async def fetch_state(self, device_id: str) -> DeviceState:
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        payload = await self._get_json(f"api/v2/devices/{device_id}/state")
        return _state_response(payload, device_id)

    async def refresh(self, device_id: str, backend: str) -> DeviceState:
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        url = urljoin(self._base_url, f"api/v2/devices/{device_id}/refresh")
        try:
            async with self._session.post(url, timeout=300) as response:
                payload = await response.json()
                if not isinstance(payload, Mapping):
                    raise ApiError("proxy returned invalid refresh outcome")
                status = payload.get("status")
                if response.status != 200 or status != "fresh":
                    if status == "superseded":
                        raise ApiError(
                            "device refresh was superseded by another operation"
                        )
                    raise ApiError("device refresh did not complete")
                result = _state_response(payload, device_id)
                if payload["backend"] != backend:
                    raise ApiError("proxy returned a refresh for a different backend")
                if not result.available or payload["inventory_status"] != "present":
                    raise ApiError("proxy returned a refresh without current readings")
                return result
        except ApiError:
            raise
        except Exception as error:
            raise ApiError("cannot refresh readings from the local proxy") from error

    async def set_control(
        self, device_id: str, command: str | Mapping[str, JsonValue]
    ) -> None:
        """Send exactly once and require a matching confirmed response."""
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        command = _control_command(command)
        request_id = uuid4().hex
        url = urljoin(self._base_url, f"api/v2/devices/{device_id}/control")
        try:
            async with self._session.post(
                url,
                json={
                    "request_id": request_id,
                    "issued_at_unix_ms": time.time_ns() // 1_000_000,
                    "command": command,
                },
                timeout=300,
            ) as response:
                payload = await response.json()
                http_status = response.status
        except Exception as error:
            raise ControlOutcomeUnknown(request_id) from error
        if not isinstance(payload, Mapping):
            raise ControlOutcomeUnknown(request_id)
        status = payload.get("status")
        if (
            payload.get("request_id") != request_id
            or not isinstance(status, str)
            or CONTROL_HTTP_STATUSES.get(status) != http_status
        ):
            raise ControlOutcomeUnknown(request_id)
        if http_status != 200 or payload["status"] != "confirmed":
            raise ApiError(
                f"{_control_error(payload, http_status)} (request {request_id})"
            )

    async def _get_json(self, path: str) -> object:
        url = urljoin(self._base_url, path)
        try:
            async with self._session.get(url, timeout=10) as response:
                if response.status != 200:
                    raise ApiError(f"proxy returned HTTP {response.status}")
                return await response.json()
        except ApiError:
            raise
        except Exception as error:
            raise ApiError("cannot connect to the local proxy") from error


def _valid_device(device: object) -> bool:
    if not isinstance(device, Mapping):
        return False
    capabilities = device.get("capabilities")
    if not isinstance(capabilities, Mapping):
        return False
    commands = capabilities.get("commands")
    return (
        _valid_proxy_id(device.get("proxy_id"))
        and isinstance(device.get("id"), str)
        and _valid_identifier(device["id"])
        and isinstance(device.get("name"), str)
        and isinstance(device.get("backend"), str)
        and device["backend"] in {"legacy_ble", "quick_connect"}
        and isinstance(capabilities.get("read_state"), bool)
        and isinstance(commands, list)
        and all(
            isinstance(command, Mapping) and isinstance(command.get("kind"), str)
            for command in commands
        )
        and all(
            isinstance(device.get(source), str) and device[source] in {"http", "mqtt"}
            for source in ("state_source", "command_source")
        )
        and device["state_source"] == device["command_source"]
    )


def _mapping(value: object) -> Mapping[str, object]:
    if not isinstance(value, Mapping) or not all(isinstance(key, str) for key in value):
        raise ApiError("proxy returned invalid object data")
    return value


def _string(value: object) -> str:
    if not isinstance(value, str):
        raise ApiError("proxy returned invalid text")
    return value


def _optional_string(value: object) -> str | None:
    return None if value is None else _string(value)


def _boolean(value: object) -> bool:
    if not isinstance(value, bool):
        raise ApiError("proxy returned invalid flag")
    return value


def _optional_boolean(value: object) -> bool | None:
    return None if value is None else _boolean(value)


def _integer(value: object) -> int | None:
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool):
        raise ApiError("proxy returned invalid integer")
    return value


def _number(value: object) -> float | int | None:
    if value is None:
        return None
    if not _is_finite_number(value):
        raise ApiError("proxy returned invalid number")
    return value


def _backend(value: object) -> Backend:
    if value == "legacy_ble":
        return "legacy_ble"
    if value == "quick_connect":
        return "quick_connect"
    raise ApiError("proxy returned invalid backend")


def _device_values(device: Mapping[str, object]) -> Device:
    capabilities = _mapping(device["capabilities"])
    commands = capabilities["commands"]
    if not isinstance(commands, list):
        raise ApiError("proxy returned invalid commands")
    return Device(
        proxy_id=_string(device["proxy_id"]),
        id=_string(device["id"]),
        name=_string(device["name"]),
        read_state=_boolean(capabilities["read_state"]),
        backend=_backend(device["backend"]),
        commands=frozenset(_string(_mapping(command)["kind"]) for command in commands),
        owner="http" if device["state_source"] == "http" else "mqtt",
    )


def _control_error(payload: object, status: int) -> str:
    if isinstance(payload, Mapping):
        message = payload.get("message")
        if isinstance(message, str):
            return f"control was not confirmed: {message}"
        outcome = payload.get("status")
        if isinstance(outcome, str):
            return f"control was not confirmed: {outcome}"
    return f"proxy returned HTTP {status} for control"


def _valid_proxy_id(value: object) -> bool:
    if not isinstance(value, str):
        return False
    try:
        parsed = UUID(value)
    except ValueError:
        return False
    return parsed.version == 4 and parsed.variant == RFC_4122 and str(parsed) == value


def _valid_identifier(value: str) -> bool:
    return (
        bool(value)
        and len(value) <= 64
        and all(
            character.isascii() and (character.isalnum() or character in "_-")
            for character in value
        )
    )


def _valid_state(state: Mapping[str, object], backend: str) -> bool:
    settings = state.get("settings")
    provenance = state.get("provenance")
    return (
        isinstance(settings, Mapping)
        and _valid_settings(settings, backend)
        and isinstance(provenance, Mapping)
        and provenance.get("backend") == backend
        and all(
            _valid_timestamp(provenance.get(key))
            for key in ("fetched_at_unix_ms", "observed_at_unix_ms")
        )
        and _valid_diagnostics(state.get("diagnostics"))
        and (
            state.get("estimated_running") is None
            or isinstance(state["estimated_running"], bool)
        )
        and all(
            _optional_finite_number(state.get(key))
            for key in ("temperature_f", "humidity_percent")
        )
    )


def _valid_diagnostics(diagnostics: object) -> bool:
    if diagnostics is None:
        return True
    return (
        isinstance(diagnostics, Mapping)
        and all(
            diagnostics.get(key) is None or isinstance(diagnostics[key], str)
            for key in ("firmware_version", "signal_strength_raw", "verified_raw")
        )
        and (
            diagnostics.get("ota_in_progress") is None
            or isinstance(diagnostics["ota_in_progress"], bool)
        )
    )


def _valid_settings(settings: Mapping[str, object], backend: str) -> bool:
    if settings.get("backend") != backend:
        return False
    mode = settings.get("mode")
    valid_modes = (
        {"automatic", "timer", "ota"}
        if backend == "legacy_ble"
        else {"off", "automatic", "timer", "manual", "unknown", "conflicting"}
    )
    if mode is None:
        if backend == "quick_connect":
            return False
    elif not isinstance(mode, str) or mode not in valid_modes:
        return False
    keys: tuple[str, ...]
    if backend == "quick_connect":
        keys = (
            "automatic_temperature_f",
            "automatic_humidity_percent",
            "timer_duration_minutes",
        )
        flag = settings.get("humidity_monitor")
    else:
        keys = (
            "automatic_temperature_tenths_f",
            "automatic_humidity_tenths_percent",
            "timer_remaining_minutes",
            "timer_original_minutes",
        )
        flag = settings.get("controller_fan_on")
    return (flag is None or isinstance(flag, bool)) and all(
        _optional_integer(settings.get(key), 65535) for key in keys
    )


def _home_assistant_values(state: Mapping[str, object], backend: str) -> Readings:
    diagnostics = _mapping(state.get("diagnostics") or {})
    settings = _mapping(state["settings"])
    common = {
        "temperature_f": _number(state.get("temperature_f")),
        "humidity_percent": _number(state.get("humidity_percent")),
    }
    if backend == "legacy_ble":
        return Readings(
            temperature_f=common["temperature_f"],
            humidity_percent=common["humidity_percent"],
            firmware_version=_optional_string(diagnostics.get("firmware_version")),
            mode=_optional_string(settings.get("mode")),
            controller_fan_flag=_optional_boolean(settings.get("controller_fan_on")),
            automatic_temperature_threshold_f=_tenths(
                _integer(settings.get("automatic_temperature_tenths_f"))
            ),
            automatic_humidity_threshold_percent=_tenths(
                _integer(settings.get("automatic_humidity_tenths_percent"))
            ),
            timer_remaining_minutes=_integer(settings.get("timer_remaining_minutes")),
            timer_original_minutes=_integer(settings.get("timer_original_minutes")),
        )
    mode = _string(settings["mode"])
    running = _optional_boolean(state.get("estimated_running"))
    return Readings(
        temperature_f=common["temperature_f"],
        humidity_percent=common["humidity_percent"],
        mode=mode,
        automatic_temperature_f=_integer(settings.get("automatic_temperature_f")),
        automatic_humidity_percent=_integer(settings.get("automatic_humidity_percent")),
        timer_duration_minutes=_integer(settings.get("timer_duration_minutes")),
        humidity_monitor=_optional_boolean(settings.get("humidity_monitor")),
        automatic_mode=_mode_flag(mode, "automatic"),
        timer_mode=_mode_flag(mode, "timer"),
        manual_mode=_mode_flag(mode, "manual"),
        running_estimate=running,
        running_estimate_provenance="inferred" if running is not None else None,
        firmware_version=_optional_string(diagnostics.get("firmware_version")),
        signal_strength_raw=_optional_string(diagnostics.get("signal_strength_raw")),
        verified_raw=_optional_string(diagnostics.get("verified_raw")),
        ota_in_progress=_optional_boolean(diagnostics.get("ota_in_progress")),
    )


def _mode_flag(mode: str, expected: str) -> bool | None:
    return mode == expected if mode in QUICKCONNECT_MODES else None


def _optional_finite_number(value: object) -> bool:
    return value is None or _is_finite_number(value)


def _optional_integer(value: object, maximum: int) -> bool:
    return value is None or (
        isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= maximum
    )


def _valid_timestamp(value: object) -> bool:
    return _optional_integer(value, 2**64 - 1)


def _tenths(value: int | None) -> float | None:
    return value / 10 if value is not None else None


def _state_response(payload: object, device_id: str) -> DeviceState:
    if not isinstance(payload, Mapping):
        raise ApiError("proxy returned invalid device state")
    payload = _mapping(payload)
    available = payload.get("available")
    state = payload.get("state")
    last_error = payload.get("last_error")
    backend = payload.get("backend")
    inventory_status = payload.get("inventory_status")
    if not isinstance(backend, str) or backend not in {
        "legacy_ble",
        "quick_connect",
    }:
        raise ApiError("proxy returned invalid device state")
    if not isinstance(inventory_status, str) or inventory_status not in {
        "unknown",
        "present",
        "missing",
        "unavailable",
    }:
        raise ApiError("proxy returned invalid device state")
    if not isinstance(available, bool):
        raise ApiError("proxy returned invalid device state")
    if state is not None and not isinstance(state, Mapping):
        raise ApiError("proxy returned invalid device state")
    if last_error is not None and not isinstance(last_error, str):
        raise ApiError("proxy returned invalid device state")
    if payload.get("id") != device_id:
        raise ApiError("proxy returned mismatched device state")
    if available != (state is not None):
        raise ApiError("proxy returned inconsistent device state")
    observed_at = None
    values = None
    if state is not None:
        state = _mapping(state)
        if not _valid_state(state, backend):
            raise ApiError("proxy returned invalid device values")
        observed_at = _integer(_mapping(state["provenance"]).get("observed_at_unix_ms"))
        values = _home_assistant_values(state, backend)
    freshness: Literal["fresh", "unknown", "stale"] = (
        "fresh"
        if available
        else ("unknown" if inventory_status == "unknown" else "stale")
    )
    return DeviceState(
        device_id=device_id,
        backend=_backend(backend),
        available=available,
        freshness=freshness,
        observed_at_unix_ms=observed_at,
        last_error=last_error,
        state=values,
    )
