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
        response.request_id = kwargs["json"]["request_id"]
        return response


class FailedSession:
    def get(self, _url, **_kwargs):
        raise OSError("private host detail")


class ApiClientTests(unittest.TestCase):
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

    def test_accepts_an_empty_device_inventory(self):
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse({"devices": []})])
        )

        self.assertEqual(asyncio.run(client.fetch_devices()), [])

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
        session = FakeSession(
            [FakeResponse({"request_id": "$request_id", "status": "confirmed"})]
        )
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
