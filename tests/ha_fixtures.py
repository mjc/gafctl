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


def readings(*, settings, **overrides):
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


def state_data(*, state=None, available=True, freshness="fresh", backend="legacy_ble"):
    return {
        "id": "configured",
        "backend": backend,
        "available": available,
        "inventory_status": "unknown" if freshness == "unknown" else "present",
        "last_error": None,
        "state": state,
    }
