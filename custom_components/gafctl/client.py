"""HTTP client for the Gafctl API."""

from __future__ import annotations

import time
from collections.abc import Mapping
from contextlib import AbstractAsyncContextManager
from dataclasses import replace
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
    EntitySource,
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
        try:
            selected = [_decode_device(device) for device in devices]
        except ApiError as error:
            raise ApiError("proxy returned invalid device data") from error
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


def _optional_uint(value: object, maximum: int = 65535) -> int | None:
    if value is None:
        return None
    if type(value) is not int or not 0 <= value <= maximum:
        raise ApiError("proxy returned invalid unsigned integer")
    return value


def _optional_number(value: object) -> float | int | None:
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


def _decode_device(value: object) -> Device:
    device = _mapping(value)
    capabilities = _mapping(device.get("capabilities"))
    commands = capabilities.get("commands")
    if not isinstance(commands, list):
        raise ApiError("proxy returned invalid commands")
    device_id = _string(device.get("id"))
    if not _valid_identifier(device_id):
        raise ApiError("proxy returned invalid device identifier")
    owner = _owner(device.get("state_source"))
    if _owner(device.get("command_source")) != owner:
        raise ApiError("proxy returned mixed device ownership")
    return Device(
        proxy_id=_proxy_id(device.get("proxy_id")),
        id=device_id,
        name=_string(device.get("name")),
        read_state=_boolean(capabilities.get("read_state")),
        backend=_backend(device.get("backend")),
        commands=frozenset(
            _string(_mapping(command).get("kind")) for command in commands
        ),
        owner=owner,
    )


def _owner(value: object) -> EntitySource:
    if value == "http":
        return "http"
    if value == "mqtt":
        return "mqtt"
    raise ApiError("proxy returned invalid device ownership")


def _control_error(payload: object, status: int) -> str:
    if isinstance(payload, Mapping):
        message = payload.get("message")
        if isinstance(message, str):
            return f"control was not confirmed: {message}"
        outcome = payload.get("status")
        if isinstance(outcome, str):
            return f"control was not confirmed: {outcome}"
    return f"proxy returned HTTP {status} for control"


def _proxy_id(value: object) -> str:
    identifier = _string(value)
    try:
        parsed = UUID(identifier)
    except ValueError as error:
        raise ApiError("proxy returned invalid proxy identity") from error
    if parsed.version != 4 or parsed.variant != RFC_4122 or str(parsed) != identifier:
        raise ApiError("proxy returned invalid proxy identity")
    return identifier


def _valid_identifier(value: str) -> bool:
    return (
        bool(value)
        and len(value) <= 64
        and all(
            character.isascii() and (character.isalnum() or character in "_-")
            for character in value
        )
    )


def _decode_readings(state: Mapping[str, object], backend: Backend) -> Readings:
    settings = _mapping(state.get("settings"))
    if settings.get("backend") != backend:
        raise ApiError("proxy returned settings for a different backend")
    mode = _mode(settings.get("mode"), backend)
    raw_diagnostics = state.get("diagnostics")
    diagnostics = {} if raw_diagnostics is None else _mapping(raw_diagnostics)
    common = Readings(
        temperature_f=_optional_number(state.get("temperature_f")),
        humidity_percent=_optional_number(state.get("humidity_percent")),
        firmware_version=_optional_string(diagnostics.get("firmware_version")),
        mode=mode,
    )
    signal_strength = _optional_string(diagnostics.get("signal_strength_raw"))
    verified = _optional_string(diagnostics.get("verified_raw"))
    ota = _optional_boolean(diagnostics.get("ota_in_progress"))
    running = _optional_boolean(state.get("estimated_running"))
    if backend == "legacy_ble":
        return replace(
            common,
            controller_fan_flag=_optional_boolean(settings.get("controller_fan_on")),
            automatic_temperature_threshold_f=_tenths(
                _optional_uint(settings.get("automatic_temperature_tenths_f"))
            ),
            automatic_humidity_threshold_percent=_tenths(
                _optional_uint(settings.get("automatic_humidity_tenths_percent"))
            ),
            timer_remaining_minutes=_optional_uint(
                settings.get("timer_remaining_minutes")
            ),
            timer_original_minutes=_optional_uint(
                settings.get("timer_original_minutes")
            ),
        )
    return replace(
        common,
        automatic_temperature_f=_optional_uint(settings.get("automatic_temperature_f")),
        automatic_humidity_percent=_optional_uint(
            settings.get("automatic_humidity_percent")
        ),
        timer_duration_minutes=_optional_uint(settings.get("timer_duration_minutes")),
        humidity_monitor=_optional_boolean(settings.get("humidity_monitor")),
        automatic_mode=_mode_flag(mode, "automatic"),
        timer_mode=_mode_flag(mode, "timer"),
        manual_mode=_mode_flag(mode, "manual"),
        running_estimate=running,
        running_estimate_provenance="inferred" if running is not None else None,
        signal_strength_raw=signal_strength,
        verified_raw=verified,
        ota_in_progress=ota,
    )


def _mode(value: object, backend: Backend) -> str | None:
    mode = _optional_string(value)
    allowed = (
        {None, "automatic", "timer", "ota"}
        if backend == "legacy_ble"
        else QUICKCONNECT_MODES | {"unknown", "conflicting"}
    )
    if mode not in allowed:
        raise ApiError("proxy returned invalid device mode")
    return mode


def _observed_at(value: object, backend: Backend) -> int | None:
    provenance = _mapping(value)
    if provenance.get("backend") != backend:
        raise ApiError("proxy returned provenance for a different backend")
    _optional_uint(provenance.get("fetched_at_unix_ms"), 2**64 - 1)
    return _optional_uint(provenance.get("observed_at_unix_ms"), 2**64 - 1)


def _mode_flag(mode: str | None, expected: str) -> bool | None:
    return mode == expected if mode in QUICKCONNECT_MODES else None


def _tenths(value: int | None) -> float | None:
    return value / 10 if value is not None else None


def _state_response(payload: object, device_id: str) -> DeviceState:
    payload = _mapping(payload)
    backend = _backend(payload.get("backend"))
    inventory_status = payload.get("inventory_status")
    if not isinstance(inventory_status, str) or inventory_status not in {
        "unknown",
        "present",
        "missing",
        "unavailable",
    }:
        raise ApiError("proxy returned invalid device state")
    available = _boolean(payload.get("available"))
    last_error = _optional_string(payload.get("last_error"))
    state = payload.get("state")
    if payload.get("id") != device_id:
        raise ApiError("proxy returned mismatched device state")
    if available != (state is not None):
        raise ApiError("proxy returned inconsistent device state")
    observed_at = None
    values = None
    if state is not None:
        state = _mapping(state)
        try:
            observed_at = _observed_at(state.get("provenance"), backend)
            values = _decode_readings(state, backend)
        except ApiError as error:
            raise ApiError("proxy returned invalid device values") from error
    freshness: Literal["fresh", "unknown", "stale"] = (
        "fresh"
        if available
        else "unknown"
        if inventory_status == "unknown"
        else "stale"
    )
    return DeviceState(
        device_id=device_id,
        backend=backend,
        available=available,
        freshness=freshness,
        observed_at_unix_ms=observed_at,
        last_error=last_error,
        state=values,
    )
