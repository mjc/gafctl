"""Standard-library tests for proxy response handling with a fake HTTP session."""

import asyncio
import importlib.util
import unittest
from pathlib import Path

CLIENT_PATH = (
    Path(__file__).parents[1] / "custom_components" / "updraft" / "client.py"
)
CLIENT_SPEC = importlib.util.spec_from_file_location("updraft_client", CLIENT_PATH)
CLIENT = importlib.util.module_from_spec(CLIENT_SPEC)
CLIENT_SPEC.loader.exec_module(CLIENT)
ApiClient = CLIENT.ApiClient
ApiError = CLIENT.ApiError
normalize_api_url = CLIENT.normalize_api_url
PROXY_ID = "550e8400-e29b-41d4-a716-446655440000"


def inventory_device(**overrides):
    return {
        "proxy_id": PROXY_ID,
        "id": "configured",
        "name": "Vent",
        "backend": "legacy_ble",
        "capabilities": {"read_state": True, "commands": []},
        "state_source": "http",
        "command_source": "http",
    } | overrides


def legacy_state(**overrides):
    state = {
        "temperature_f": 98.6,
        "humidity_percent": 42.1,
        "settings": {
            "backend": "legacy_ble",
            "mode": "automatic",
            "controller_fan_on": False,
            "automatic_temperature_tenths_f": 1050,
            "automatic_humidity_tenths_percent": 300,
            "timer_remaining_minutes": 0,
            "timer_original_minutes": 0,
        },
        "estimated_running": None,
        "diagnostics": {"firmware_version": "3.0.0"},
        "provenance": {
            "backend": "legacy_ble",
            "fetched_at_unix_ms": 2000,
            "observed_at_unix_ms": 1234,
        },
    }
    state.update(overrides)
    return state


def quickconnect_state(**overrides):
    state = {
        "temperature_f": 101.4,
        "humidity_percent": 37.0,
        "settings": {
            "backend": "quick_connect",
            "mode": "automatic",
            "automatic_temperature_f": 105,
            "automatic_humidity_percent": 40,
            "timer_duration_minutes": 60,
            "humidity_monitor": True,
        },
        "estimated_running": True,
        "diagnostics": {
            "firmware_version": "1.2.3",
            "signal_strength_raw": -45,
            "verified_raw": True,
        },
        "provenance": {
            "backend": "quick_connect",
            "fetched_at_unix_ms": 2000,
            "observed_at_unix_ms": None,
        },
    }
    state.update(overrides)
    return state


def v2_state_payload(state=None, **overrides):
    payload = {
        "id": "configured",
        "backend": "legacy_ble",
        "available": state is not None,
        "inventory_status": "present" if state is not None else "unknown",
        "last_error": None,
        "state": state,
    }
    payload.update(overrides)
    return payload


class FakeResponse:
    def __init__(self, payload, status=200):
        self.payload = payload
        self.status = status

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        return None

    async def json(self):
        if isinstance(self.payload, dict) and self.payload.get("request_id") == "$request_id":
            return self.payload | {"request_id": self.request_id}
        return self.payload


class FakeSession:
    def __init__(self, responses):
        self.responses = list(responses)
        self.urls = []
        self.posts = []

    def get(self, url, **_kwargs):
        self.urls.append(url)
        return self.responses.pop(0)

    def post(self, url, **kwargs):
        self.urls.append(url)
        self.posts.append(kwargs)
        response = self.responses.pop(0)
        response.request_id = kwargs.get("json", {}).get("request_id")
        return response


class FailedSession:
    def get(self, _url, **_kwargs):
        raise OSError("private host detail")


class ApiClientTests(unittest.TestCase):
    def test_refresh_reads_device_once_and_rejects_cached_failed_outcome(self):
        payload = v2_state_payload(legacy_state(), status="fresh")
        session = FakeSession([FakeResponse(payload)])
        result = asyncio.run(ApiClient("http://proxy/prefix", session).refresh("configured", "legacy_ble"))
        self.assertTrue(result["available"])
        self.assertEqual(session.urls, ["http://proxy/prefix/api/v2/devices/configured/refresh"])
        self.assertEqual(len(session.posts), 1)
        self.assertEqual(session.posts[0]["timeout"], 300)
        self.assertNotIn("json", session.posts[0])
        for status, http_status in (("failed", 502), ("superseded", 409), ("fresh", 502), (None, 200)):
            session = FakeSession([FakeResponse(payload | {"status": status}, http_status)])
            with self.assertRaises(ApiError):
                asyncio.run(ApiClient("http://proxy", session).refresh("configured", "legacy_ble"))

    def test_inventory_requires_proxy_identity_and_one_owner(self):
        for fields in (
            {"proxy_id": None},
            {"proxy_id": "not-a-uuid"},
            {"proxy_id": "00000000-0000-0000-0000-000000000000"},
            {"state_source": "mqtt", "command_source": "http"},
            {"state_source": None},
        ):
            with self.subTest(fields=fields):
                client = ApiClient("http://127.0.0.1:8787", FakeSession([
                    FakeResponse({"devices": [inventory_device(**fields)]})
                ]))
                with self.assertRaises(ApiError):
                    asyncio.run(client.fetch_devices())

    def test_inventory_preserves_proxy_identity_and_rejects_mixed_proxies(self):
        client = ApiClient("http://127.0.0.1:8787", FakeSession([
            FakeResponse({"devices": [inventory_device()]})
        ]))
        self.assertEqual(asyncio.run(client.fetch_devices())[0]["proxy_id"], PROXY_ID)
        client = ApiClient("http://127.0.0.1:8787", FakeSession([
            FakeResponse({"devices": [inventory_device(), inventory_device(
                id="other", proxy_id="650e8400-e29b-41d4-a716-446655440000"
            )]})
        ]))
        with self.assertRaises(ApiError):
            asyncio.run(client.fetch_devices())

    def test_timer_preset_requires_matching_remaining_and_original_duration(self):
        for remaining, original, expected in (
            (0, 0, "timer_clear"),
            (1, 1, "timer_one_minute"),
            (0, 1, None),
            (2, 1, None),
            (1, 2, None),
            (0, 2, None),
        ):
            with self.subTest(remaining=remaining, original=original):
                self.assertEqual(
                    CLIENT.timer_control_preset({
                        "timer_remaining_minutes": remaining,
                        "timer_original_minutes": original,
                    }),
                    expected,
                )

    def test_normalizes_proxy_url_and_rejects_embedded_credentials(self):
        self.assertEqual(
            normalize_api_url(" http://127.0.0.1:8787/ "),
            "http://127.0.0.1:8787",
        )
        with self.assertRaises(ApiError):
            normalize_api_url("http://user:secret@127.0.0.1:8787")

    def test_discovers_devices_without_returning_private_identifiers(self):
        session = FakeSession(
            [
                FakeResponse(
                    {
                        "devices": [
                            {
                       "proxy_id": PROXY_ID,
                       "state_source": "http",
                       "command_source": "http",
                                "id": "configured",
                                "name": "GAF Wi-Fi Vent",
                                "backend": "legacy_ble",
                                "capabilities": {
                                    "read_state": True,
                                    "commands": [{"kind": "legacy_preset", "value": "timer_clear"}],
                                },
                                "peripheral_id": "private-peripheral-id",
                            }
                        ]
                    }
                )
            ]
        )
        client = ApiClient("http://127.0.0.1:8787", session)

        devices = asyncio.run(client.fetch_devices())

        self.assertEqual(devices[0]["id"], "configured")
        self.assertNotIn("peripheral_id", devices[0])
        self.assertEqual(session.urls, ["http://127.0.0.1:8787/api/v2/devices"])

    def test_inventory_keeps_capabilities_for_explicit_device_selection(self):
        session = FakeSession(
            [
                FakeResponse(
                    {
                        "devices": [
                            {
                       "proxy_id": PROXY_ID,
                       "state_source": "http",
                       "command_source": "http",
                                "id": "ble-device",
                                "name": "Legacy Vent",
                                "backend": "legacy_ble",
                                "capabilities": {
                                    "read_state": True,
                                    "commands": [
                                        {"kind": "legacy_preset", "value": "timer_clear"}
                                    ],
                                },
                            },
                            {
                       "proxy_id": PROXY_ID,
                       "state_source": "http",
                       "command_source": "http",
                                "id": "cloud-device",
                                "name": "QuickConnect Vent",
                                "backend": "quick_connect",
                                "capabilities": {
                                    "read_state": True,
                                    "commands": [
                                        {"kind": "quick_connect_mode"},
                                        {"kind": "quick_connect_targets"},
                                        {"kind": "quick_connect_timer_duration"},
                                    ],
                                },
                            },
                        ]
                    }
                )
            ]
        )

        devices = asyncio.run(ApiClient("http://ha:8787", session).fetch_devices())

        self.assertEqual(
            CLIENT.select_device(devices, "cloud-device")["name"],
            "QuickConnect Vent",
        )
        self.assertEqual(
            CLIENT.entity_platforms(devices[1]),
            {"sensor", "binary_sensor", "select", "number", "button"},
        )
        self.assertEqual(
            CLIENT.entity_keys(devices[1]),
            {
                "sensor": {
                    "temperature",
                    "humidity",
                    "mode",
                    "firmware_version",
                    "humidity_monitor",
                },
                "binary_sensor": {"running_estimate"},
                "button": {"refresh"},
                "select": {"mode"},
                "number": {
                    "automatic_temperature",
                    "automatic_humidity",
                    "timer_duration",
                },
            },
        )
        self.assertNotIn("number", CLIENT.entity_platforms(devices[0]))
        self.assertEqual(
            CLIENT.QUICKCONNECT_NUMBER_RANGES,
            {
                "automatic_temperature": (90, 120, 1),
                "automatic_humidity": (30, 80, 1),
                "timer_duration": (30, 360, 30),
            },
        )

    def test_device_selection_never_falls_back_to_first_device(self):
        devices = [
            {"id": "first", "state": True, "backend": "legacy_ble", "commands": []},
            {"id": "second", "state": True, "backend": "quick_connect", "commands": []},
        ]

        with self.assertRaisesRegex(ApiError, "selected device is unavailable"):
            CLIENT.select_device(devices, "missing")

    def test_maps_quickconnect_measurements_targets_mode_and_running_provenance(self):
        client = ApiClient(
            "http://ha:8787",
            FakeSession(
                [
                    FakeResponse(
                        v2_state_payload(
                            quickconnect_state(), backend="quick_connect"
                        )
                    )
                ]
            ),
        )

        result = asyncio.run(client.fetch_state("configured"))

        self.assertEqual(result["state"]["temperature_f"], 101.4)
        self.assertEqual(result["state"]["automatic_temperature_f"], 105)
        self.assertEqual(result["state"]["timer_duration_minutes"], 60)
        self.assertEqual(result["state"]["mode"], "automatic")
        self.assertEqual(result["state"]["humidity_monitor"], "on")
        self.assertIs(result["state"]["running_estimate"], True)
        self.assertEqual(result["state"]["running_estimate_provenance"], "inferred")
        self.assertNotIn("timer_remaining_minutes", result["state"])
        self.assertNotIn("signal_strength", result["state"])
        self.assertNotIn("is_verified", result["state"])

    def test_device_source_transition_removes_http_entity_ownership(self):
        device = {
            "id": "cloud-device",
            "backend": "quick_connect",
            "state": True,
            "commands": [{"kind": "quick_connect_mode"}],
            "state_source": "mqtt",
            "command_source": "mqtt",
        }

        self.assertEqual(CLIENT.entity_platforms(device), set())

    def test_mixed_sources_keep_only_the_http_owned_entity_sets(self):
        device = {
            "id": "cloud-device",
            "backend": "quick_connect",
            "state": True,
            "commands": [
                {"kind": "quick_connect_mode"},
                {"kind": "quick_connect_targets"},
                {"kind": "quick_connect_timer_duration"},
            ],
        }

        self.assertEqual(
            CLIENT.entity_platforms(device | {"state_source": "mqtt"}),
            {"select", "number"},
        )
        self.assertEqual(
            CLIENT.entity_platforms(device | {"command_source": "mqtt"}),
            {"sensor", "binary_sensor", "button"},
        )

    def test_accepts_an_empty_device_inventory(self):
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse({"devices": []})])
        )

        self.assertEqual(asyncio.run(client.fetch_devices()), [])

    def test_rejects_duplicate_inventory_ids(self):
        device = {
            "proxy_id": PROXY_ID,
            "state_source": "http",
            "command_source": "http",
            "id": "same",
            "name": "Vent",
            "backend": "quick_connect",
            "capabilities": {"read_state": True, "commands": []},
        }
        client = ApiClient(
            "http://ha:8787",
            FakeSession([FakeResponse({"devices": [device, device]})]),
        )

        with self.assertRaisesRegex(ApiError, "duplicate device identifiers"):
            asyncio.run(client.fetch_devices())

    def test_fetches_state_and_preserves_freshness(self):
        values = {
            "firmware_version": "3.0.0",
            "mode": "automatic",
            "controller_fan_flag": "off",
            "temperature_f": 98.6,
            "humidity_percent": 42.1,
            "automatic_temperature_threshold_f": 105.0,
            "automatic_humidity_threshold_percent": 30.0,
            "timer_remaining_minutes": 0,
            "timer_original_minutes": 0,
        }
        payload = v2_state_payload(
            legacy_state() | {"identity_suffix": "private-suffix"},
            peripheral_id="private-peripheral-id",
        )
        session = FakeSession([FakeResponse(payload)])
        client = ApiClient("http://127.0.0.1:8787/", session)

        result = asyncio.run(client.fetch_state("configured"))

        self.assertEqual(result["state"], values)
        self.assertEqual(result["freshness"], "fresh")
        self.assertNotIn("peripheral_id", result)
        self.assertNotIn("identity_suffix", result["state"])
        self.assertEqual(
            session.urls,
            ["http://127.0.0.1:8787/api/v2/devices/configured/state"],
        )

    def test_rejects_path_injection_in_device_ids(self):
        client = ApiClient("http://127.0.0.1:8787", FakeSession([]))

        with self.assertRaisesRegex(ApiError, "invalid configured device"):
            asyncio.run(client.fetch_state("../other"))

    def test_preserves_stale_unavailable_state_without_inventing_values(self):
        payload = {
            "id": "configured",
            "backend": "legacy_ble",
            "available": False,
            "inventory_status": "present",
            "last_error": "BLE unavailable",
            "state": None,
        }
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse(payload)])
        )

        result = asyncio.run(client.fetch_state("configured"))

        self.assertIsNone(result["state"])
        self.assertFalse(result["available"])
        self.assertEqual(result["freshness"], "stale")

    def test_sends_only_supported_controls_and_requires_confirmation(self):
        response = FakeResponse({"request_id": "$request_id", "status": "confirmed"})
        session = FakeSession([response])
        client = ApiClient("http://127.0.0.1:8787", session)

        asyncio.run(client.set_control("configured", "timer_clear"))

        self.assertEqual(
            session.urls,
            ["http://127.0.0.1:8787/api/v2/devices/configured/control"],
        )
        request = session.posts[0]["json"]
        self.assertEqual(
            request["command"],
            {"kind": "legacy_preset", "preset": "timer_clear"},
        )
        self.assertTrue(request["request_id"])
        self.assertIsInstance(request["issued_at_unix_ms"], int)
        self.assertEqual(session.posts[0]["timeout"], 90)
        with self.assertRaisesRegex(ApiError, "unsupported control preset"):
            asyncio.run(client.set_control("configured", "timer_999"))

    def test_sends_quickconnect_mode_with_correlation_and_exact_command_shape(self):
        response = FakeResponse({"request_id": "$request_id", "status": "confirmed"})
        session = FakeSession([response])
        client = ApiClient("http://ha:8787", session)

        asyncio.run(
            client.set_control("quick-device", {"kind": "quick_connect_mode", "mode": "manual"})
        )

        request = session.posts[0]["json"]
        self.assertEqual(request["command"], {"kind": "quick_connect_mode", "mode": "manual"})
        self.assertIsInstance(request["request_id"], str)
        self.assertEqual(request["request_id"], response.request_id)

    def test_rejects_invalid_quickconnect_values_before_post(self):
        session = FakeSession([])
        client = ApiClient("http://ha:8787", session)

        for command in (
            {"kind": "quick_connect_targets", "temperature_f": 90.5, "humidity_percent": 40},
            {"kind": "quick_connect_targets", "temperature_f": 121, "humidity_percent": 40},
            {"kind": "quick_connect_timer_duration", "minutes": 45},
        ):
            with self.subTest(command=command), self.assertRaises(ApiError):
                asyncio.run(client.set_control("quick-device", command))
        self.assertEqual(session.posts, [])

    def test_sends_valid_quickconnect_targets_and_duration_as_typed_commands(self):
        responses = [
            FakeResponse({"request_id": "$request_id", "status": "confirmed"})
            for _ in range(2)
        ]
        session = FakeSession(responses)
        client = ApiClient("http://ha:8787", session)

        asyncio.run(
            client.set_control(
                "quick-device",
                {
                    "kind": "quick_connect_targets",
                    "temperature_f": 120,
                    "humidity_percent": 80,
                },
            )
        )
        asyncio.run(
            client.set_control(
                "quick-device",
                {"kind": "quick_connect_timer_duration", "minutes": 360},
            )
        )

        self.assertEqual(
            [post["json"]["command"] for post in session.posts],
            [
                {
                    "kind": "quick_connect_targets",
                    "temperature_f": 120,
                    "humidity_percent": 80,
                },
                {"kind": "quick_connect_timer_duration", "minutes": 360},
            ],
        )

    def test_rejects_unconfirmed_control_readback(self):
        client = ApiClient(
            "http://127.0.0.1:8787",
            FakeSession(
                [
                    FakeResponse(
                        {
                            "request_id": "ignored",
                            "status": "unconfirmed",
                            "message": "readback differed",
                        },
                        status=502,
                    )
                ]
            ),
        )

        with self.assertRaisesRegex(ApiError, "readback differed"):
            asyncio.run(client.set_control("configured", "timer_clear"))

    def test_rejects_mismatched_control_confirmation(self):
        client = ApiClient(
            "http://127.0.0.1:8787",
            FakeSession(
                [
                    FakeResponse(
                        {"request_id": "wrong", "status": "confirmed"}
                    )
                ]
            ),
        )

        with self.assertRaisesRegex(ApiError, "mismatched control confirmation"):
            asyncio.run(client.set_control("configured", "timer_clear"))


    def test_rejects_inconsistent_or_invalid_proxy_responses(self):
        invalid = v2_state_payload(legacy_state(), available=False)
        client = ApiClient("http://127.0.0.1:8787", FakeSession([FakeResponse(invalid)]))

        with self.assertRaisesRegex(ApiError, "inconsistent"):
            asyncio.run(client.fetch_state("configured"))

    def test_rejects_invalid_inventory_status(self):
        for inventory_status in ([], {}):
            with self.subTest(inventory_status=inventory_status):
                payload = {
                    "id": "configured",
                    "backend": "legacy_ble",
                    "inventory_status": inventory_status,
                    "available": False,
                    "state": None,
                }
                client = ApiClient(
                    "http://127.0.0.1:8787",
                    FakeSession([FakeResponse(payload)]),
                )
                with self.assertRaisesRegex(ApiError, "invalid device state"):
                    asyncio.run(client.fetch_state("configured"))

    def test_rejects_malformed_sensor_values(self):
        payload = v2_state_payload(legacy_state(temperature_f="unknown"))
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse(payload)])
        )

        with self.assertRaisesRegex(ApiError, "invalid device values"):
            asyncio.run(client.fetch_state("configured"))

    def test_rejects_non_finite_and_oversized_sensor_values(self):
        for field, value in (
            ("temperature_f", float("inf")),
            ("temperature_f", float("nan")),
            ("temperature_f", 10**400),
            ("timer_remaining_minutes", 10**400),
        ):
            with self.subTest(field=field, value=type(value).__name__):
                state = legacy_state()
                if field in {"temperature_f", "humidity_percent"}:
                    state[field] = value
                elif field == "timer_remaining_minutes":
                    state["settings"][field] = value
                payload = v2_state_payload(state)
                client = ApiClient(
                    "http://127.0.0.1:8787",
                    FakeSession([FakeResponse(payload)]),
                )
                with self.assertRaisesRegex(ApiError, "invalid device values"):
                    asyncio.run(client.fetch_state("configured"))

    def test_maps_http_failures_to_safe_errors(self):
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse({}, status=503)])
        )

        with self.assertRaisesRegex(ApiError, "HTTP 503"):
            asyncio.run(client.fetch_devices())

    def test_maps_connection_failures_without_exposing_exception_details(self):
        client = ApiClient("http://127.0.0.1:8787", FailedSession())

        with self.assertRaisesRegex(ApiError, "cannot connect") as raised:
            asyncio.run(client.fetch_devices())

        self.assertNotIn("private host detail", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
