"""HTTP client for the Gafctl API."""

from __future__ import annotations

import re
import time
from collections.abc import Mapping
from typing import Any, cast
from urllib.parse import urljoin, urlparse
from uuid import UUID, uuid4

from .controls import CONTROL_HTTP_STATUSES
from .models import (
    ApiError,
    ControlOutcomeUnknown,
    Device,
    DeviceState,
    JsonObject,
)


def normalize_api_url(value: str) -> str:
    """Validate a proxy URL and return it without a trailing slash."""
    try:
        parsed = urlparse(value.strip())
        # Accessing port validates its syntax and range. Omitted ports are valid.
        _ = parsed.port
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

    async def fetch_devices(self) -> list[Device]:
        payload = await self._get_json("api/v2/devices")
        devices = payload.get("devices") if isinstance(payload, Mapping) else None
        if not isinstance(devices, list):
            raise ApiError("proxy returned no devices")
        selected = cast(list[Device], devices)
        try:
            for device in selected:
                _validate_proxy_id(device["proxy_id"])
                if (
                    not _valid_identifier(device["id"])
                    or device["backend"] not in ("legacy_ble", "quick_connect")
                    or device["state_source"] not in ("http", "mqtt")
                    or device["state_source"] != device["command_source"]
                ):
                    raise ApiError(
                        "proxy returned invalid device identity or ownership"
                    )
            if len({device["proxy_id"] for device in selected}) > 1:
                raise ApiError("proxy returned inconsistent proxy identities")
            identifiers = [device["id"] for device in selected]
            if len(identifiers) != len(set(identifiers)):
                raise ApiError("proxy returned duplicate device identifiers")
        except (KeyError, TypeError) as error:
            raise ApiError("proxy returned invalid device data") from error
        return selected

    async def fetch_state(self, device_id: str) -> DeviceState:
        payload = await self._get_json(_device_path(device_id, "state"))
        return _state_response(payload, device_id)

    async def set_control(self, device_id: str, command: JsonObject) -> None:
        """Send exactly once and require a matching confirmed response."""
        path = _device_path(device_id, "control")
        request_id = uuid4().hex
        try:
            http_status, payload = await self._request(
                "POST",
                path,
                json={
                    "request_id": request_id,
                    "issued_at_unix_ms": time.time_ns() // 1_000_000,
                    "command": command,
                },
            )
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
            detail = payload.get("message") or status
            raise ApiError(
                f"control was not confirmed: {detail} (request {request_id})"
            )

    async def _get_json(self, path: str) -> Any:
        try:
            _, payload = await self._request("GET", path)
            return payload
        except ApiError:
            raise
        except Exception as error:
            raise ApiError("cannot connect to the local proxy") from error

    async def _request(self, method: str, path: str, **kwargs: Any) -> tuple[int, Any]:
        async with self._session.request(
            method,
            urljoin(self._base_url, path),
            timeout=10 if method == "GET" else 300,
            allow_redirects=False,
            **kwargs,
        ) as response:
            if method == "GET" and response.status != 200:
                raise ApiError(f"proxy returned HTTP {response.status}")
            return response.status, await response.json()


def _validate_proxy_id(value: object) -> None:
    if not isinstance(value, str):
        raise ApiError("proxy returned invalid proxy identity")
    try:
        parsed = UUID(value)
    except ValueError as error:
        raise ApiError("proxy returned invalid proxy identity") from error
    if parsed.version != 4 or str(parsed) != value:
        raise ApiError("proxy returned invalid proxy identity")


def _valid_identifier(value: str) -> bool:
    return (
        isinstance(value, str)
        and re.fullmatch(r"[A-Za-z0-9_-]{1,64}", value) is not None
    )


def _device_path(device_id: str, action: str) -> str:
    if not _valid_identifier(device_id):
        raise ApiError("invalid configured device")
    return f"api/v2/devices/{device_id}/{action}"


def _state_response(raw: Any, device_id: str) -> DeviceState:
    response = cast(DeviceState, raw)
    try:
        if response["id"] != device_id:
            raise ApiError("proxy returned mismatched device state")
        duration = response["timer_duration_minutes"]
        if duration is not None and (
            response["backend"] != "legacy_ble"
            or type(duration) is not int
            or not 0 <= duration <= 360
        ):
            raise ApiError("proxy returned invalid timer duration")
        state = response["state"]
        if response["available"] is not (state is not None):
            raise ApiError("proxy returned inconsistent device state")
        if response["inventory_status"] not in (
            "unknown",
            "present",
            "missing",
            "unavailable",
        ):
            raise ApiError("proxy returned invalid device state")
        if state is not None and (
            state["settings"]["backend"] != response["backend"]
            or state["provenance"]["backend"] != response["backend"]
        ):
            raise ApiError("proxy returned state for a different backend")
        return response
    except (KeyError, TypeError) as error:
        raise ApiError("proxy returned invalid device state") from error
