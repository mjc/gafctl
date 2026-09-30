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


class FakeResponse:
    def __init__(self, payload, status=200):
        self.payload = payload
        self.status = status

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        return None

    async def json(self):
        return self.payload


class FakeSession:
    def __init__(self, responses):
        self.responses = list(responses)
        self.urls = []

    def get(self, url, **_kwargs):
        self.urls.append(url)
        return self.responses.pop(0)


class FailedSession:
    def get(self, _url, **_kwargs):
        raise OSError("private host detail")


class ApiClientTests(unittest.TestCase):
    def test_normalizes_proxy_url_and_rejects_embedded_credentials(self):
        self.assertEqual(
            normalize_api_url(" http://127.0.0.1:8787/ "),
            "http://127.0.0.1:8787",
        )
        with self.assertRaises(ApiError):
            normalize_api_url("http://user:secret@127.0.0.1:8787")

    def test_discovers_configured_device_without_using_ble_identifier(self):
        session = FakeSession(
            [
                FakeResponse(
                    {
                        "devices": [
                            {
                                "id": "configured",
                                "name": "GAF Wi-Fi Vent",
                                "state": True,
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
        self.assertEqual(session.urls, ["http://127.0.0.1:8787/api/v1/devices"])

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
        payload = {
            "device_id": "configured",
            "available": True,
            "freshness": "fresh",
            "observed_at_unix_ms": 1234,
            "last_error": None,
            "state": values | {"identity_suffix": "private-suffix"},
            "peripheral_id": "private-peripheral-id",
        }
        session = FakeSession([FakeResponse(payload)])
        client = ApiClient("http://127.0.0.1:8787/", session)

        result = asyncio.run(client.fetch_state("configured"))

        self.assertEqual(result["state"], values)
        self.assertEqual(result["freshness"], "fresh")
        self.assertNotIn("peripheral_id", result)
        self.assertNotIn("identity_suffix", result["state"])
        self.assertEqual(
            session.urls,
            ["http://127.0.0.1:8787/api/v1/devices/configured/state"],
        )

    def test_rejects_path_injection_in_device_ids(self):
        client = ApiClient("http://127.0.0.1:8787", FakeSession([]))

        with self.assertRaisesRegex(ApiError, "invalid configured device"):
            asyncio.run(client.fetch_state("../other"))

    def test_preserves_stale_unavailable_state_without_inventing_values(self):
        payload = {
            "device_id": "configured",
            "available": False,
            "freshness": "stale",
            "observed_at_unix_ms": 1234,
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


    def test_rejects_inconsistent_or_invalid_proxy_responses(self):
        invalid = {
            "device_id": "configured",
            "freshness": "fresh",
            "available": False,
            "state": None,
        }
        client = ApiClient("http://127.0.0.1:8787", FakeSession([FakeResponse(invalid)]))

        with self.assertRaisesRegex(ApiError, "inconsistent"):
            asyncio.run(client.fetch_state("configured"))

    def test_rejects_non_string_freshness_values(self):
        for freshness in ([], {}):
            with self.subTest(freshness=freshness):
                payload = {
                    "device_id": "configured",
                    "freshness": freshness,
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
        payload = {
            "device_id": "configured",
            "freshness": "fresh",
            "available": True,
            "state": {"temperature_f": "unknown"},
        }
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
                values[field] = value
                payload = {
                    "device_id": "configured",
                    "freshness": "fresh",
                    "available": True,
                    "state": values,
                }
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
