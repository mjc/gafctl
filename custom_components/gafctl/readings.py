"""Reading metadata shared by entity eligibility and presentation."""

from typing import NamedTuple

from .models import Backend

MODE_LABELS = {
    "off": "Off",
    "automatic": "Automatic",
    "timer": "Timer",
    "manual": "Manual",
}


class SensorReading(NamedTuple):
    name: str
    path: tuple[str, ...]
    unit: str | None = None
    device_class: str | None = None
    measurement: bool = False
    tenths: bool = False


_COMMON_SENSORS = {
    "temperature": SensorReading(
        "Ambient temperature", ("temperature_f",), "°F", "temperature", measurement=True
    ),
    "humidity": SensorReading(
        "Relative humidity", ("humidity_percent",), "%", "humidity", measurement=True
    ),
    "mode": SensorReading("Controller mode", ("settings", "mode")),
    "firmware_version": SensorReading(
        "Firmware version", ("diagnostics", "firmware_version")
    ),
}

SENSORS: dict[Backend, dict[str, SensorReading]] = {
    "legacy_ble": _COMMON_SENSORS
    | {
        "automatic_temperature_threshold": SensorReading(
            "Automatic temperature threshold",
            ("settings", "automatic_temperature_tenths_f"),
            "°F",
            "temperature",
            tenths=True,
        ),
        "automatic_humidity_threshold": SensorReading(
            "Automatic humidity threshold",
            ("settings", "automatic_humidity_tenths_percent"),
            "%",
            tenths=True,
        ),
        "timer_remaining": SensorReading(
            "Timer remaining", ("settings", "timer_remaining_minutes"), "min"
        ),
        "timer_original": SensorReading(
            "Last timer duration", ("settings", "timer_original_minutes"), "min"
        ),
    },
    "quick_connect": _COMMON_SENSORS
    | {
        "signal_strength_raw": SensorReading(
            "Signal strength (reported)", ("diagnostics", "signal_strength_raw")
        ),
        "verified_raw": SensorReading(
            "Verification (reported)", ("diagnostics", "verified_raw")
        ),
    },
}

BINARY_FIELDS: dict[Backend, dict[str, tuple[str, tuple[str, ...], str]]] = {
    "legacy_ble": {
        "controller_fan_flag": (
            "Controller fan flag",
            ("settings", "controller_fan_on"),
            "controller",
        ),
    },
    "quick_connect": {
        "running_estimate": ("Running estimate", ("estimated_running",), "inferred"),
        "ota_in_progress": (
            "OTA in progress",
            ("diagnostics", "ota_in_progress"),
            "reported",
        ),
        **{
            f"{mode}_mode": (f"{label} mode", ("settings", "mode"), "reported")
            for mode, label in MODE_LABELS.items()
            if mode != "off"
        },
        "humidity_monitor": (
            "Humidity monitoring",
            ("settings", "humidity_monitor"),
            "reported",
        ),
    },
}
