"""Small, validated client for the local Updraft API."""

import asyncio
import math
import time
from collections.abc import Mapping
from typing import Any
from urllib.parse import urljoin, urlparse
from uuid import UUID, RFC_4122, uuid4


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

class ApiError(Exception):
    """A safe-to-display Updraft API error."""


class ControlOutcomeUnknown(ApiError):
    """The submitted request has no trustworthy confirmation."""

    def __init__(self, request_id: str) -> None:
        self.request_id = request_id
        super().__init__(f"Control outcome unknown for request {request_id}; read current state before sending another command")


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
            or not _valid_proxy_id(device.get("proxy_id"))
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
            or any(
                not isinstance(device.get(source), str)
                or device[source] not in {"http", "mqtt"}
                for source in ("state_source", "command_source")
            )
            or device["state_source"] != device["command_source"]
            for device in devices
        ):
            raise ApiError("proxy returned invalid device data")
        identifiers = [device["id"] for device in devices]
        if len({device["proxy_id"] for device in devices}) > 1:
            raise ApiError("proxy returned inconsistent proxy identities")
        if len(identifiers) != len(set(identifiers)):
            raise ApiError("proxy returned duplicate device identifiers")
        return [
            {
                "proxy_id": device["proxy_id"],
                "id": device["id"],
                "name": device["name"],
                "state": device["capabilities"]["read_state"],
                "backend": device["backend"],
                "commands": device["capabilities"]["commands"],
                "capabilities": device["capabilities"],
                "state_source": device["state_source"],
                "command_source": device["command_source"],
            }
            for device in devices
        ]

    async def fetch_state(self, device_id: str) -> dict[str, Any]:
        if not _valid_identifier(device_id):
            raise ApiError("invalid configured device")
        payload = await self._get_json(f"api/v2/devices/{device_id}/state")
        return _state_response(payload, device_id)

    async def refresh(self, device_id: str, backend: str) -> dict[str, Any]:
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
                        raise ApiError("device refresh was superseded by another operation")
                    raise ApiError("device refresh did not complete")
                result = _state_response(payload, device_id)
                if payload["backend"] != backend:
                    raise ApiError("proxy returned a refresh for a different backend")
                if not result["available"] or payload["inventory_status"] != "present":
                    raise ApiError("proxy returned a refresh without current readings")
                return result
        except asyncio.CancelledError:
            raise
        except ApiError:
            raise
        except Exception as error:
            raise ApiError("cannot refresh readings from the local proxy") from error

    async def set_control(self, device_id: str, command: str | Mapping[str, Any]) -> None:
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
        except asyncio.CancelledError:
            raise
        except Exception as error:
            raise ControlOutcomeUnknown(request_id) from error
        if (
            not isinstance(payload, Mapping)
            or payload.get("request_id") != request_id
            or not isinstance(payload.get("status"), str)
            or CONTROL_HTTP_STATUSES.get(payload["status"]) != http_status
        ):
            raise ControlOutcomeUnknown(request_id)
        if http_status != 200 or payload["status"] != "confirmed":
            raise ApiError(f"{_control_error(payload, http_status)} (request {request_id})")

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

QUICKCONNECT_MODES = frozenset({"off", "automatic", "timer", "manual"})
QUICKCONNECT_NUMBER_RANGES = {
    "automatic_temperature": (90, 120, 1),
    "automatic_humidity": (30, 80, 1),
    "timer_duration": (30, 360, 30),
}
LEGACY_NUMBER_RANGES = {
    "automatic_temperature": (90, 120, 1),
    "automatic_humidity": (30, 80, 1),
    "timer_duration": (0, 360, 1),
}
LEGACY_NUMBER_COMMANDS = {
    "automatic_temperature": ("legacy_automatic_temperature", "temperature_f"),
    "automatic_humidity": ("legacy_automatic_humidity", "humidity_percent"),
    "timer_duration": ("legacy_timer", "minutes"),
}
QUICKCONNECT_SENSOR_KEYS = {
    "temperature",
    "humidity",
    "mode",
    "firmware_version",
    "signal_strength_raw",
    "verified_raw",
}
LEGACY_SENSOR_KEYS = {
    "temperature",
    "humidity",
    "mode",
    "firmware_version",
    "automatic_temperature_threshold",
    "automatic_humidity_threshold",
    "timer_remaining",
    "timer_original",
}


def select_device(devices: list[dict[str, Any]], device_id: str) -> dict[str, Any]:
    """Resolve the explicitly selected HTTP-owned device from current inventory."""
    device = next((item for item in devices if item["id"] == device_id), None)
    if device is None or not entity_platforms(device):
        raise ApiError("selected device is unavailable")
    return device


def entity_platforms(device: Mapping[str, Any]) -> set[str]:
    """Return HA platforms owned by this adapter for the device capabilities."""
    return set(entity_keys(device))


def entity_keys(device: Mapping[str, Any]) -> dict[str, set[str]]:
    """Project device ownership and capabilities into exact HA entity keys."""
    capabilities = device.get("capabilities")
    commands = device.get("commands")
    if isinstance(capabilities, Mapping):
        commands = capabilities.get("commands")
    command_kinds = {
        command.get("kind")
        for command in commands or []
        if isinstance(command, Mapping)
    }
    backend = device.get("backend")
    has_state = device.get("state") is True
    state_owned = device.get("state_source", "http") == "http"
    commands_owned = device.get("command_source", "http") == "http"
    entities: dict[str, set[str]] = {}
    if state_owned and has_state:
        entities["button"] = {"refresh"}
        entities["sensor"] = (
            set(LEGACY_SENSOR_KEYS)
            if backend == "legacy_ble"
            else set(QUICKCONNECT_SENSOR_KEYS)
        )
        if backend == "quick_connect":
            entities["binary_sensor"] = {"running_estimate", "ota_in_progress", "automatic_mode", "timer_mode", "manual_mode", "humidity_monitor"}
        else:
            entities["binary_sensor"] = {"controller_fan_flag"}
    if not commands_owned:
        return entities
    if backend == "legacy_ble" and "legacy_preset" in command_kinds:
        entities["select"] = {"automatic_thresholds", "timer"}
    if backend == "legacy_ble":
        number_keys = {
            key for key, (capability, _) in LEGACY_NUMBER_COMMANDS.items()
            if capability in command_kinds
        }
        if number_keys:
            entities["number"] = number_keys
    if backend == "quick_connect":
        if "quick_connect_mode" in command_kinds:
            entities["select"] = {"mode"}
            entities["switch"] = {"automatic_mode", "timer_mode", "manual_mode"}
            entities.setdefault("button", set()).add("all_off")
        number_keys = {
            key
            for key, capability in (
                ("automatic_temperature", "quick_connect_targets"),
                ("automatic_humidity", "quick_connect_targets"),
                ("timer_duration", "quick_connect_timer_duration"),
            )
            if capability in command_kinds
        }
        if number_keys:
            entities["number"] = number_keys
    return entities


def _control_command(command: str | Mapping[str, Any]) -> dict[str, Any]:
    if isinstance(command, str):
        if command not in CONTROL_PRESETS:
            raise ApiError("unsupported control preset")
        return {"kind": "legacy_preset", "preset": command}
    if not isinstance(command, Mapping):
        raise ApiError("unsupported control command")
    kind = command.get("kind")
    for key, (capability, field) in LEGACY_NUMBER_COMMANDS.items():
        if kind == capability and set(command) == {"kind", field}:
            minimum, maximum, _ = LEGACY_NUMBER_RANGES[key]
            if _integer_in_range(command[field], minimum, maximum):
                return dict(command)
    if kind == "quick_connect_mode" and set(command) == {"kind", "mode"}:
        if isinstance(command["mode"], str) and command["mode"] in QUICKCONNECT_MODES:
            return dict(command)
    if kind == "quick_connect_conditional_off" and set(command) == {"kind", "only_if_current"}:
        if isinstance(command["only_if_current"], str) and command["only_if_current"] in QUICKCONNECT_MODES:
            return dict(command)
    for kind_name, field, minimum, maximum in (
        ("quick_connect_automatic_temperature", "temperature_f", 90, 120),
        ("quick_connect_automatic_humidity", "humidity_percent", 30, 80),
    ):
        if kind == kind_name and set(command) == {"kind", field} and _integer_in_range(command[field], minimum, maximum):
            return dict(command)
    if kind == "quick_connect_targets" and set(command) == {
        "kind",
        "temperature_f",
        "humidity_percent",
    }:
        temperature = command["temperature_f"]
        humidity = command["humidity_percent"]
        temperature_min, temperature_max, _ = QUICKCONNECT_NUMBER_RANGES[
            "automatic_temperature"
        ]
        humidity_min, humidity_max, _ = QUICKCONNECT_NUMBER_RANGES[
            "automatic_humidity"
        ]
        if _integer_in_range(temperature, temperature_min, temperature_max) and (
            _integer_in_range(humidity, humidity_min, humidity_max)
        ):
            return dict(command)
    if kind == "quick_connect_timer_duration" and set(command) == {"kind", "minutes"}:
        duration = command["minutes"]
        minimum, maximum, step = QUICKCONNECT_NUMBER_RANGES["timer_duration"]
        if _integer_in_range(duration, minimum, maximum) and duration % step == 0:
            return dict(command)
    raise ApiError("invalid device control command")


def _integer_in_range(value: Any, minimum: int, maximum: int) -> bool:
    return type(value) is int and minimum <= value <= maximum


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


def _valid_proxy_id(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        parsed = UUID(value)
    except ValueError:
        return False
    return parsed.version == 4 and parsed.variant == RFC_4122 and str(parsed) == value


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
    if diagnostics is not None and (
        any(diagnostics.get(key) is not None and not isinstance(diagnostics[key], str)
            for key in ("firmware_version", "signal_strength_raw", "verified_raw"))
        or diagnostics.get("ota_in_progress") is not None and type(diagnostics["ota_in_progress"]) is not bool
    ):
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
        ) and (
            settings.get("humidity_monitor") is None
            or isinstance(settings.get("humidity_monitor"), bool)
        ) and (
            diagnostics is None
            or diagnostics.get("firmware_version") is None
            or isinstance(diagnostics.get("firmware_version"), str)
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
            controller_fan_flag=fan_on,
            automatic_temperature_threshold_f=_tenths(
                settings.get("automatic_temperature_tenths_f")
            ),
            automatic_humidity_threshold_percent=_tenths(
                settings.get("automatic_humidity_tenths_percent")
            ),
            timer_remaining_minutes=settings.get("timer_remaining_minutes"),
            timer_original_minutes=settings.get("timer_original_minutes"),
        )
    else:
        diagnostics = state.get("diagnostics") or {}
        values.update(
            mode=settings["mode"],
            automatic_temperature_f=settings.get("automatic_temperature_f"),
            automatic_humidity_percent=settings.get("automatic_humidity_percent"),
            timer_duration_minutes=settings.get("timer_duration_minutes"),
            humidity_monitor=settings.get("humidity_monitor"),
            automatic_mode=_mode_flag(settings["mode"], "automatic"),
            timer_mode=_mode_flag(settings["mode"], "timer"),
            manual_mode=_mode_flag(settings["mode"], "manual"),
            running_estimate=state.get("estimated_running"),
            running_estimate_provenance=(
                "inferred" if state.get("estimated_running") is not None else None
            ),
            firmware_version=diagnostics.get("firmware_version"),
            signal_strength_raw=diagnostics.get("signal_strength_raw"),
            verified_raw=diagnostics.get("verified_raw"),
            ota_in_progress=diagnostics.get("ota_in_progress"),
        )
    return values


def _mode_flag(mode: str, expected: str) -> bool | None:
    return mode == expected if mode in QUICKCONNECT_MODES else None


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


def _state_response(payload: Any, device_id: str) -> dict[str, Any]:
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
