"""Fixed API responses for adapter tests."""

PROXY_ID = "550e8400-e29b-41d4-a716-446655440000"


def device(proxy_id=PROXY_ID, owner="http", **overrides):
    read_state = overrides.pop("read_state", True)
    commands = overrides.pop("commands", ())
    return {
        "proxy_id": proxy_id,
        "id": "configured",
        "name": "Vent",
        "backend": "legacy_ble",
        "capabilities": {
            "read_state": read_state,
            "commands": [{"kind": kind} for kind in commands],
        },
        "state_source": owner,
        "command_source": owner,
    } | overrides


def legacy_settings(**overrides):
    return {
        "backend": "legacy_ble",
        "mode": None,
        "controller_fan_on": None,
        "automatic_temperature_tenths_f": None,
        "automatic_humidity_tenths_percent": None,
        "timer_remaining_minutes": None,
        "timer_original_minutes": None,
    } | overrides


def quickconnect_settings(**overrides):
    return {
        "backend": "quick_connect",
        "mode": "unknown",
        "automatic_temperature_f": None,
        "automatic_humidity_percent": None,
        "timer_duration_minutes": None,
        "humidity_monitor": None,
    } | overrides


def diagnostics(**overrides):
    return {
        "firmware_version": None,
        "signal_strength_raw": None,
        "verified_raw": None,
        "ota_in_progress": None,
    } | overrides


def readings(*, settings=None, **overrides):
    settings = legacy_settings() if settings is None else settings
    return {
        "temperature_f": None,
        "humidity_percent": None,
        "settings": settings,
        "estimated_running": None,
        "diagnostics": None,
        "provenance": {
            "backend": settings["backend"],
            "fetched_at_unix_ms": None,
            "observed_at_unix_ms": None,
        },
    } | overrides


def reported_state(backend="legacy_ble", **overrides):
    settings = (
        legacy_settings(
            mode="automatic",
            controller_fan_on=False,
            automatic_temperature_tenths_f=1050,
            automatic_humidity_tenths_percent=300,
            timer_remaining_minutes=0,
            timer_original_minutes=0,
        )
        if backend == "legacy_ble"
        else quickconnect_settings(
            mode="automatic",
            automatic_temperature_f=105,
            automatic_humidity_percent=40,
            timer_duration_minutes=60,
            humidity_monitor=True,
        )
    )
    return (
        readings(
            settings=settings,
            temperature_f=98.6 if backend == "legacy_ble" else 101.4,
            humidity_percent=42.1 if backend == "legacy_ble" else 37.0,
            estimated_running=None if backend == "legacy_ble" else True,
            diagnostics=diagnostics(
                firmware_version="3.0.0" if backend == "legacy_ble" else "1.2.3",
                signal_strength_raw=None if backend == "legacy_ble" else "-45",
                verified_raw=None if backend == "legacy_ble" else "true",
            ),
            provenance={
                "backend": backend,
                "fetched_at_unix_ms": 2000,
                "observed_at_unix_ms": 1234 if backend == "legacy_ble" else None,
            },
        )
        | overrides
    )


def state_data(*, state=None, available=True, freshness="fresh", backend="legacy_ble"):
    return {
        "id": "configured",
        "backend": backend,
        "available": available,
        "inventory_status": "unknown" if freshness == "unknown" else "present",
        "last_error": None,
        "state": state,
    }


def changed_device(selected, **fields):
    """Update capabilities without repeating the descriptor envelope."""
    selected = selected.copy()
    if "commands" in fields:
        selected["capabilities"] = selected["capabilities"] | {
            "commands": [{"kind": kind} for kind in fields.pop("commands")]
        }
    if "owner" in fields:
        selected["state_source"] = selected["command_source"] = fields.pop("owner")
    return selected | fields
