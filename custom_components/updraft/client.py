"""Small, validated client for the local Updraft API."""

import asyncio
import math
from collections.abc import Mapping
from typing import Any
from urllib.parse import urljoin, urlparse

STATE_VALUE_KEYS = (
    "firmware_version",
    "mode",
    "controller_fan_flag",
    "temperature_f",
    "humidity_percent",
    "automatic_temperature_threshold_f",
    "automatic_humidity_threshold_percent",
    "timer_remaining_minutes",
    "timer_original_minutes",
)


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
        payload = await self._get_json("api/v1/devices")
        devices = payload.get("devices") if isinstance(payload, Mapping) else None
        if not isinstance(devices, list) or not devices:
            raise ApiError("proxy returned no devices")
        if any(
            not isinstance(device, Mapping)
            or not isinstance(device.get("id"), str)
            or not _valid_identifier(device["id"])
            or not isinstance(device.get("name"), str)
            or not isinstance(device.get("state"), bool)
            for device in devices
        ):
            raise ApiError("proxy returned invalid device data")
        return [
            {key: device[key] for key in ("id", "name", "state")}
            for device in devices
        ]

    async def fetch_state(self, device_id: str) -> dict[str, Any]:
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        payload = await self._get_json(f"api/v1/devices/{device_id}/state")
        if not isinstance(payload, Mapping):
            raise ApiError("proxy returned invalid device state")
        freshness = payload.get("freshness")
        available = payload.get("available")
        state = payload.get("state")
        last_error = payload.get("last_error")
        observed_at = payload.get("observed_at_unix_ms")
        if not isinstance(freshness, str) or freshness not in {
            "unknown",
            "fresh",
            "stale",
        }:
            raise ApiError("proxy returned invalid device state")
        if not isinstance(available, bool):
            raise ApiError("proxy returned invalid device state")
        if state is not None and not isinstance(state, Mapping):
            raise ApiError("proxy returned invalid device state")
        if last_error is not None and not isinstance(last_error, str):
            raise ApiError("proxy returned invalid device state")
        if observed_at is not None and (
            not isinstance(observed_at, int)
            or isinstance(observed_at, bool)
            or not 0 <= observed_at <= 2**64 - 1
        ):
            raise ApiError("proxy returned invalid device state")
        if payload.get("device_id") != device_id:
            raise ApiError("proxy returned mismatched device state")
        if freshness == "fresh" and (not available or state is None):
            raise ApiError("proxy returned inconsistent device state")
        if freshness != "fresh" and available:
            raise ApiError("proxy returned inconsistent device state")
        if freshness == "unknown" and state is not None:
            raise ApiError("proxy returned inconsistent device state")
        if state is not None and not _valid_values(state):
            raise ApiError("proxy returned invalid device values")
        return {
            "device_id": device_id,
            "available": available,
            "freshness": freshness,
            "observed_at_unix_ms": observed_at,
            "last_error": last_error,
            "state": (
                {key: state[key] for key in STATE_VALUE_KEYS}
                if state is not None
                else None
            ),
        }

    async def set_control(self, device_id: str, preset: str) -> None:
        """Send one verified preset and reject mismatched or unverified readback."""
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        if preset not in CONTROL_PRESETS:
            raise ApiError("unsupported control preset")
        url = urljoin(self._base_url, f"api/v1/devices/{device_id}/control")
        try:
            async with self._session.post(
                url, json={"preset": preset}, timeout=90
            ) as response:
                payload = await response.json()
                if response.status != 200 or not isinstance(payload, Mapping):
                    raise ApiError(_control_error(payload, response.status))
                if payload.get("preset") != preset:
                    raise ApiError("proxy returned a mismatched control confirmation")
                if payload.get("success") is not True:
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
    return f"proxy returned HTTP {status} for control"


def _valid_identifier(value: str) -> bool:
    return bool(value) and all(
        character.isascii() and (character.isalnum() or character in "_-")
        for character in value
    )


def _valid_values(state: Mapping[str, Any]) -> bool:
    text_values = {
        "firmware_version",
        "mode",
        "controller_fan_flag",
    }
    number_values = {
        "temperature_f",
        "humidity_percent",
        "automatic_temperature_threshold_f",
        "automatic_humidity_threshold_percent",
    }
    integer_values = {"timer_remaining_minutes", "timer_original_minutes"}
    if any(not isinstance(state.get(key), str) for key in text_values):
        return False
    if state["mode"] not in {"automatic", "timer", "ota"}:
        return False
    if state["controller_fan_flag"] not in {"on", "off"}:
        return False
    if any(not _is_finite_number(state.get(key)) for key in number_values):
        return False
    return all(
        isinstance(state.get(key), int)
        and not isinstance(state.get(key), bool)
        and 0 <= state[key] <= 65535
        for key in integer_values
    )


def _is_finite_number(value: Any) -> bool:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False
