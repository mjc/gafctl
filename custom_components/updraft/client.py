"""Small, validated client for the local Updraft API."""

import asyncio
import math
import time
from collections.abc import Mapping
from typing import Any
from urllib.parse import urljoin, urlparse
from uuid import uuid4

class ApiError(Exception):
    """A safe-to-display Updraft API error."""


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
    def __init__(self, base_url: str, session: Any) -> None:
        self._base_url = base_url.rstrip("/") + "/"
        self._session = session

    async def fetch_devices(self) -> list[dict[str, Any]]:
        payload = await self._get_json("api/v2/devices")
        devices = payload.get("devices") if isinstance(payload, Mapping) else None
        if not isinstance(devices, list):
            raise ApiError("proxy returned no devices")
        if any(
            not isinstance(device, Mapping)
            or not isinstance(device.get("id"), str)
            or not _valid_identifier(device["id"])
            or not isinstance(device.get("name"), str)
            or not isinstance(device.get("backend"), str)
            or device["backend"] not in {"legacy_ble", "quick_connect"}
            or not isinstance(device.get("capabilities"), Mapping)
            or not isinstance(device["capabilities"].get("read_state"), bool)
            or not isinstance(device["capabilities"].get("commands"), list)
            or not all(
                isinstance(command, Mapping)
                and isinstance(command.get("kind"), str)
                for command in device["capabilities"]["commands"]
            )
            for device in devices
        ):
            raise ApiError("proxy returned invalid device data")
        return [
            {
                "id": device["id"],
                "name": device["name"],
                "state": device["capabilities"]["read_state"],
                "backend": device["backend"],
                "commands": device["capabilities"]["commands"],
            }
            for device in devices
        ]

    async def fetch_state(self, device_id: str) -> dict[str, Any]:
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        payload = await self._get_json(f"api/v2/devices/{device_id}/state")
        if not isinstance(payload, Mapping):
            raise ApiError("proxy returned invalid device state")
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
            if not _valid_state(state, backend):
                raise ApiError("proxy returned invalid device values")
            observed_at = state["provenance"].get("observed_at_unix_ms")
            values = _home_assistant_values(state, backend)
        freshness = "fresh" if available else (
            "unknown" if inventory_status == "unknown" else "stale"
        )
        return {
            "device_id": device_id,
            "available": available,
            "freshness": freshness,
            "observed_at_unix_ms": observed_at,
            "last_error": last_error,
            "state": values,
        }

    async def set_control(self, device_id: str, preset: str) -> None:
        """Send one verified preset and reject mismatched or unverified readback."""
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        if preset not in CONTROL_PRESETS:
            raise ApiError("unsupported control preset")
        request_id = uuid4().hex
        url = urljoin(self._base_url, f"api/v2/devices/{device_id}/control")
        try:
            async with self._session.post(
                url,
                json={
                    "request_id": request_id,
                    "issued_at_unix_ms": time.time_ns() // 1_000_000,
                    "command": {"kind": "legacy_preset", "preset": preset},
                },
                timeout=90,
            ) as response:
                payload = await response.json()
                if response.status != 200 or not isinstance(payload, Mapping):
                    raise ApiError(_control_error(payload, response.status))
                if payload.get("request_id") != request_id:
                    raise ApiError("proxy returned a mismatched control confirmation")
                if payload.get("status") != "confirmed":
                    raise ApiError(_control_error(payload, response.status))
        except asyncio.CancelledError:
            raise
        except ApiError:
            raise
        except Exception as error:
            raise ApiError("cannot send control to the local proxy") from error

    async def _get_json(self, path: str) -> Any:
        url = urljoin(self._base_url, path)
        try:
            async with self._session.get(url, timeout=10) as response:
                if response.status != 200:
                    raise ApiError(f"proxy returned HTTP {response.status}")
                return await response.json()
        except asyncio.CancelledError:
            raise
        except ApiError:
            raise
        except Exception as error:
            raise ApiError("cannot connect to the local proxy") from error


CONTROL_PRESETS = frozenset(
    {
        "automatic105_f30_percent",
        "automatic105_1_f30_1_percent",
        "timer_clear",
        "timer_one_minute",
    }
)


def _control_error(payload: Any, status: int) -> str:
    if isinstance(payload, Mapping):
        message = payload.get("message")
        if isinstance(message, str):
            return f"control was not confirmed: {message}"
        outcome = payload.get("status")
        if isinstance(outcome, str):
            return f"control was not confirmed: {outcome}"
    return f"proxy returned HTTP {status} for control"


def timer_control_preset(state: Mapping[str, Any]) -> str | None:
    return {(0, 0): "timer_clear", (1, 1): "timer_one_minute"}.get(
        (state.get("timer_remaining_minutes"), state.get("timer_original_minutes"))
    )


def _valid_identifier(value: str) -> bool:
    return bool(value) and len(value) <= 64 and all(
        character.isascii() and (character.isalnum() or character in "_-")
        for character in value
    )


def _valid_state(state: Mapping[str, Any], backend: str) -> bool:
    settings = state.get("settings")
    diagnostics = state.get("diagnostics")
    provenance = state.get("provenance")
    if not isinstance(settings, Mapping) or not isinstance(provenance, Mapping):
        return False
    if settings.get("backend") != backend or provenance.get("backend") != backend:
        return False
    if diagnostics is not None and not isinstance(diagnostics, Mapping):
        return False
    if state.get("estimated_running") is not None and not isinstance(
        state.get("estimated_running"), bool
    ):
        return False
    if not all(
        _optional_finite_number(state.get(key))
        for key in ("temperature_f", "humidity_percent")
    ):
        return False
    if any(
        not _valid_timestamp(provenance.get(key))
        for key in ("fetched_at_unix_ms", "observed_at_unix_ms")
    ):
        return False
    mode = settings.get("mode")
    valid_modes = (
        {None, "automatic", "timer", "ota"}
        if backend == "legacy_ble"
        else {"off", "automatic", "timer", "manual", "unknown", "conflicting"}
    )
    if mode is None:
        if backend == "quick_connect":
            return False
    elif not isinstance(mode, str) or mode not in valid_modes:
        return False
    fan_on = settings.get("controller_fan_on")
    if backend == "quick_connect":
        return all(
            _optional_integer(settings.get(key), 65535)
            for key in (
                "automatic_temperature_f",
                "automatic_humidity_percent",
                "timer_duration_minutes",
            )
        )
    if fan_on is not None and not isinstance(fan_on, bool):
        return False
    return all(
        _optional_integer(settings.get(key), 65535)
        for key in (
            "automatic_temperature_tenths_f",
            "automatic_humidity_tenths_percent",
            "timer_remaining_minutes",
            "timer_original_minutes",
        )
    ) and (
        diagnostics is None
        or diagnostics.get("firmware_version") is None
        or isinstance(diagnostics.get("firmware_version"), str)
    )


def _home_assistant_values(state: Mapping[str, Any], backend: str) -> dict[str, Any]:
    values: dict[str, Any] = {
        "temperature_f": state.get("temperature_f"),
        "humidity_percent": state.get("humidity_percent"),
    }
    settings = state["settings"]
    if backend == "legacy_ble":
        diagnostics = state.get("diagnostics") or {}
        fan_on = settings.get("controller_fan_on")
        values.update(
            firmware_version=diagnostics.get("firmware_version"),
            mode=settings.get("mode"),
            controller_fan_flag=("on" if fan_on else "off") if fan_on is not None else None,
            automatic_temperature_threshold_f=_tenths(
                settings.get("automatic_temperature_tenths_f")
            ),
            automatic_humidity_threshold_percent=_tenths(
                settings.get("automatic_humidity_tenths_percent")
            ),
            timer_remaining_minutes=settings.get("timer_remaining_minutes"),
            timer_original_minutes=settings.get("timer_original_minutes"),
        )
    return values


def _optional_finite_number(value: Any) -> bool:
    return value is None or _is_finite_number(value)


def _optional_integer(value: Any, maximum: int) -> bool:
    return value is None or (
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 <= value <= maximum
    )


def _valid_timestamp(value: Any) -> bool:
    return _optional_integer(value, 2**64 - 1)


def _tenths(value: int | None) -> float | None:
    return value / 10 if value is not None else None


def _is_finite_number(value: Any) -> bool:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False
