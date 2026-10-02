"""HTTP client for the Gafctl API."""

from __future__ import annotations

import time
from collections.abc import Mapping
from contextlib import AbstractAsyncContextManager
from typing import Any, Literal, Protocol
from urllib.parse import urljoin, urlparse
from uuid import RFC_4122, UUID, uuid4

from .controls import (
    CONTROL_HTTP_STATUSES,
    _control_command,
)
from .models import (
    ApiError,
    Backend,
    ControlOutcomeUnknown,
    Device,
    DeviceState,
    Diagnostics,
    EntitySource,
    JsonObject,
    JsonValue,
    LegacySettings,
    QuickConnectSettings,
    Readings,
)


class Response(Protocol):
    @property
    def status(self) -> int: ...

    async def json(self) -> Any: ...


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

    async def _get_json(self, path: str) -> Any:
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


def _backend(value: object) -> Backend:
    if value == "legacy_ble":
        return "legacy_ble"
    if value == "quick_connect":
        return "quick_connect"
    raise ApiError("proxy returned invalid backend")


def _decode_device(device: Any) -> Device:
    try:
        device_id = device["id"]
        if not _valid_identifier(device_id):
            raise ApiError("proxy returned invalid device identifier")
        owner = _owner(device["state_source"])
        if _owner(device["command_source"]) != owner:
            raise ApiError("proxy returned mixed device ownership")
        capabilities = device["capabilities"]
        return Device(
            proxy_id=_proxy_id(device["proxy_id"]),
            id=device_id,
            name=device["name"],
            backend=_backend(device["backend"]),
            read_state=capabilities["read_state"],
            commands=frozenset(command["kind"] for command in capabilities["commands"]),
            owner=owner,
        )
    except (KeyError, TypeError) as error:
        raise ApiError("proxy returned invalid device data") from error


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
    if not isinstance(value, str):
        raise ApiError("proxy returned invalid proxy identity")
    identifier = value
    try:
        parsed = UUID(identifier)
    except ValueError as error:
        raise ApiError("proxy returned invalid proxy identity") from error
    if parsed.version != 4 or parsed.variant != RFC_4122 or str(parsed) != identifier:
        raise ApiError("proxy returned invalid proxy identity")
    return identifier


def _valid_identifier(value: str) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and len(value) <= 64
        and all(
            character.isascii() and (character.isalnum() or character in "_-")
            for character in value
        )
    )


def _decode_readings(state: Any, backend: Backend) -> Readings:
    settings = state["settings"]
    if settings["backend"] != backend or state["provenance"]["backend"] != backend:
        raise ApiError("proxy returned state for a different backend")
    decoded: LegacySettings | QuickConnectSettings
    if backend == "legacy_ble":
        decoded = LegacySettings(
            mode=settings["mode"],
            controller_fan_on=settings["controller_fan_on"],
            automatic_temperature_tenths_f=settings["automatic_temperature_tenths_f"],
            automatic_humidity_tenths_percent=settings[
                "automatic_humidity_tenths_percent"
            ],
            timer_remaining_minutes=settings["timer_remaining_minutes"],
            timer_original_minutes=settings["timer_original_minutes"],
        )
    else:
        decoded = QuickConnectSettings(
            mode=settings["mode"],
            automatic_temperature_f=settings["automatic_temperature_f"],
            automatic_humidity_percent=settings["automatic_humidity_percent"],
            timer_duration_minutes=settings["timer_duration_minutes"],
            humidity_monitor=settings["humidity_monitor"],
        )
    diagnostics = state["diagnostics"]
    return Readings(
        settings=decoded,
        temperature_f=state["temperature_f"],
        humidity_percent=state["humidity_percent"],
        estimated_running=state["estimated_running"],
        diagnostics=Diagnostics(**diagnostics)
        if diagnostics is not None
        else Diagnostics(),
    )


def _state_response(payload: Any, device_id: str) -> DeviceState:
    try:
        backend = _backend(payload["backend"])
        inventory_status = payload["inventory_status"]
        if inventory_status not in ("unknown", "present", "missing", "unavailable"):
            raise ApiError("proxy returned invalid device state")
        available = payload["available"]
        state = payload["state"]
        if payload["id"] != device_id:
            raise ApiError("proxy returned mismatched device state")
        if available is not (state is not None):
            raise ApiError("proxy returned inconsistent device state")
        values = _decode_readings(state, backend) if state is not None else None
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
            observed_at_unix_ms=state["provenance"]["observed_at_unix_ms"]
            if state is not None
            else None,
            last_error=payload["last_error"],
            state=values,
        )
    except (KeyError, TypeError) as error:
        raise ApiError("proxy returned invalid device state") from error
