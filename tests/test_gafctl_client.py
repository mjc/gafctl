"""Proxy contract tests with an in-memory HTTP session."""

import importlib
import sys
import unittest
from pathlib import Path
from types import ModuleType

from ha_fixtures import (
    PROXY_ID,
    device,
    diagnostics,
    legacy_settings,
    quickconnect_settings,
    readings,
    state_data,
)

PACKAGE = ModuleType("gafctl_client_tests")
PACKAGE.__path__ = [str(Path(__file__).parents[1] / "custom_components/gafctl")]
sys.modules[PACKAGE.__name__] = PACKAGE
CLIENT = importlib.import_module(f"{PACKAGE.__name__}.client")
MODELS = importlib.import_module(f"{PACKAGE.__name__}.models")
CONTROLS = importlib.import_module(f"{PACKAGE.__name__}.controls")
ApiError = MODELS.ApiError
COMMAND = {"kind": "legacy_preset", "preset": "timer_clear"}


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


class FakeResponse:
    def __init__(self, payload, status=200):
        self.payload, self.status = payload, status

    async def __aenter__(self):
        if isinstance(self.payload, Exception):
            raise self.payload
        return self

    async def __aexit__(self, *_):
        pass

    async def json(self):
        if (
            isinstance(self.payload, dict)
            and self.payload.get("request_id") == "$request_id"
        ):
            return self.payload | {"request_id": self.request_id}
        return self.payload


class FakeSession:
    def __init__(self, *responses):
        self.responses = list(responses)
        self.urls, self.posts = [], []

    def request(self, method, url, **kwargs):
        self.urls.append(url)
        response = self.responses.pop(0)
        if method.lower() == "post":
            self.posts.append(kwargs)
            response.request_id = kwargs.get("json", {}).get("request_id")
        return response


class ApiClientTests(unittest.IsolatedAsyncioTestCase):
    def client(self, payload, status=200, url="http://proxy"):
        session = FakeSession(FakeResponse(payload, status))
        return CLIENT.ApiClient(url, session), session

    async def state(self, raw, backend="legacy_ble", **fields):
        payload = (
            state_data(state=raw, available=raw is not None, backend=backend) | fields
        )
        client, _ = self.client(payload)
        return await client.fetch_state("configured")

    async def test_wire_records_preserve_identity_capabilities_and_private_field_boundaries(
        self,
    ):
        raw = device(
            name="GAF Wi-Fi Vent", commands=["future_command", "legacy_preset"]
        )
        client, session = self.client({"devices": [raw]})
        result = (await client.fetch_devices())[0]
        self.assertIs(result, raw)
        self.assertEqual(result["proxy_id"], PROXY_ID)
        self.assertEqual(result["id"], "configured")
        self.assertNotIn("peripheral_id", result)
        self.assertEqual(
            CONTROLS.command_kinds(result), {"future_command", "legacy_preset"}
        )
        self.assertEqual(session.urls, ["http://proxy/api/v2/devices"])
        payload = state_data(state=reported_state())
        client, session = self.client(payload, url="http://proxy/")
        result = await client.fetch_state("configured")
        self.assertIs(result, payload)
        self.assertTrue(result["available"])
        self.assertEqual(result["state"]["provenance"]["observed_at_unix_ms"], 1234)
        self.assertEqual(session.urls, ["http://proxy/api/v2/devices/configured/state"])

    async def test_inventory_requires_unique_ids_consistent_proxy_and_single_owner(
        self,
    ):
        invalid = [
            [device(**fields)]
            for fields in (
                {"proxy_id": None},
                {"proxy_id": "not-a-uuid"},
                {"proxy_id": "00000000-0000-0000-0000-000000000000"},
                {"state_source": None},
                {"state_source": "mqtt", "command_source": "http"},
                {"state_source": "http", "command_source": "mqtt"},
            )
        ] + [
            [device(), device()],
            [
                device(),
                device(id="other", proxy_id="650e8400-e29b-41d4-a716-446655440000"),
            ],
        ]
        for devices in invalid:
            with self.subTest(devices=devices), self.assertRaises(ApiError):
                client, _ = self.client({"devices": devices})
                await client.fetch_devices()
        client, _ = self.client({"devices": []})
        self.assertEqual(await client.fetch_devices(), [])

    def test_selection_and_entity_eligibility_use_selected_device_capabilities(self):
        devices = [
            device(id="ble-device", commands=["legacy_preset"]),
            device(
                id="cloud-device",
                name="QuickConnect Vent",
                backend="quick_connect",
                commands=[
                    "quick_connect_mode",
                    "quick_connect_targets",
                    "quick_connect_timer_duration",
                ],
            ),
        ]
        selected = CONTROLS.select_device(devices, "cloud-device")
        self.assertEqual(selected["name"], "QuickConnect Vent")
        self.assertEqual(
            CONTROLS.entity_keys(selected),
            {
                "sensor": {
                    "temperature",
                    "humidity",
                    "mode",
                    "firmware_version",
                    "signal_strength_raw",
                    "verified_raw",
                },
                "binary_sensor": {
                    "running_estimate",
                    "ota_in_progress",
                    "automatic_mode",
                    "timer_mode",
                    "manual_mode",
                    "humidity_monitor",
                },
                "button": {"refresh", "all_off"},
                "switch": {"automatic_mode", "timer_mode", "manual_mode"},
                "select": {"mode"},
                "number": {
                    "automatic_temperature",
                    "automatic_humidity",
                    "timer_duration",
                },
            },
        )
        self.assertNotIn("number", CONTROLS.entity_keys(devices[0]))
        self.assertEqual(
            CONTROLS.entity_keys(
                device(
                    owner="mqtt",
                    backend="quick_connect",
                    commands=["quick_connect_mode"],
                )
            ),
            {},
        )
        with self.assertRaisesRegex(ApiError, "selected device is unavailable"):
            CONTROLS.select_device(devices, "missing")

    def test_number_bounds_and_timer_presets(self):
        self.assertEqual(
            {
                c.key: (c.minimum, c.maximum, c.step)
                for c in CONTROLS.NUMBER_CONTROLS["quick_connect"]
            },
            {
                "automatic_temperature": (90, 120, 1),
                "automatic_humidity": (30, 80, 1),
                "timer_duration": (30, 360, 30),
            },
        )
        for backend, controls in CONTROLS.NUMBER_CONTROLS.items():
            for control in controls:
                with self.subTest(backend=backend, control=control.key):
                    self.assertEqual(control.validate(control.minimum), control.minimum)
                    self.assertEqual(
                        control.validate(float(control.maximum)), control.maximum
                    )
                    for invalid in (
                        True,
                        None,
                        "100",
                        control.minimum - 1,
                        control.maximum + 1,
                        control.minimum + 0.5,
                        float("inf"),
                        float("nan"),
                        10**1000,
                    ):
                        with self.assertRaises(ApiError):
                            control.validate(invalid)
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
                    CONTROLS.timer_control_preset(
                        readings(
                            settings=legacy_settings(
                                timer_remaining_minutes=remaining,
                                timer_original_minutes=original,
                            )
                        )
                    ),
                    expected,
                )

    async def test_readings_preserve_reported_fields_and_unknown_values(self):
        raw = reported_state(
            "quick_connect",
            diagnostics=diagnostics(
                signal_strength_raw="-42",
                verified_raw="unknown-token",
                ota_in_progress=True,
            ),
        )
        state = (await self.state(raw, "quick_connect"))["state"]
        self.assertEqual(state, raw)
        self.assertEqual(state["temperature_f"], 101.4)
        self.assertEqual(state["settings"]["automatic_temperature_f"], 105)
        self.assertEqual(state["settings"]["timer_duration_minutes"], 60)
        self.assertEqual(state["settings"]["mode"], "automatic")
        self.assertIs(state["settings"]["humidity_monitor"], True)
        self.assertIs(state["estimated_running"], True)
        self.assertEqual(state["settings"]["backend"], "quick_connect")
        self.assertEqual(state["diagnostics"]["signal_strength_raw"], "-42")
        self.assertEqual(state["diagnostics"]["verified_raw"], "unknown-token")
        self.assertIs(state["diagnostics"]["ota_in_progress"], True)
        self.assertNotIn("timer_remaining_minutes", state["settings"])
        self.assertNotIn("signal_strength", state)
        self.assertNotIn("is_verified", state)
        raw["settings"]["mode"] = "conflicting"
        self.assertEqual(
            (await self.state(raw, "quick_connect"))["state"]["settings"]["mode"],
            "conflicting",
        )
        for backend, settings in (
            ("legacy_ble", legacy_settings()),
            ("quick_connect", quickconnect_settings()),
        ):
            with self.subTest(backend=backend):
                unknown = (await self.state(readings(settings=settings), backend))[
                    "state"
                ]
                for field in (
                    "temperature_f",
                    "humidity_percent",
                    "diagnostics",
                    "estimated_running",
                ):
                    self.assertIsNone(unknown[field])
                for key, value in unknown["settings"].items():
                    if key not in {"backend", "mode"}:
                        self.assertIsNone(value)
        stale = await self.state(
            None, inventory_status="present", last_error="BLE unavailable"
        )
        self.assertIsNone(stale["state"])
        self.assertFalse(stale["available"])
        self.assertEqual(stale["inventory_status"], "present")

    async def test_rejects_state_inconsistency_missing_records_and_mismatched_backends(
        self,
    ):
        invalid = [state_data(state=reported_state(), available=False)]
        invalid += [
            state_data(state=None, available=False) | {"inventory_status": value}
            for value in ([], {})
        ]
        for field in ("settings", "provenance"):
            raw = reported_state()
            del raw[field]
            invalid.append(state_data(state=raw))
            raw = reported_state()
            raw[field]["backend"] = "quick_connect"
            invalid.append(state_data(state=raw))
        for payload in invalid:
            with self.subTest(payload=payload), self.assertRaises(ApiError):
                client, _ = self.client(payload)
                await client.fetch_state("configured")

    async def test_refresh_posts_once_and_requires_fresh_success(self):
        payload = state_data(state=reported_state()) | {"status": "fresh"}
        client, session = self.client(payload, url="http://proxy/prefix")
        self.assertTrue((await client.refresh("configured", "legacy_ble"))["available"])
        self.assertEqual(
            session.urls, ["http://proxy/prefix/api/v2/devices/configured/refresh"]
        )
        self.assertEqual(session.posts, [{"timeout": 300}])
        for status, http_status in (
            ("failed", 502),
            ("superseded", 409),
            ("fresh", 502),
            (None, 200),
        ):
            with (
                self.subTest(status=status, http_status=http_status),
                self.assertRaises(ApiError),
            ):
                client, _ = self.client(payload | {"status": status}, http_status)
                await client.refresh("configured", "legacy_ble")

    async def test_commands_keep_exact_shape_correlation_and_single_submission(self):
        commands = [
            COMMAND,
            {"kind": "legacy_automatic_temperature", "temperature_f": 90},
            {"kind": "legacy_automatic_humidity", "humidity_percent": 80},
            {"kind": "legacy_timer", "minutes": 360},
            {"kind": "quick_connect_mode", "mode": "manual"},
            {
                "kind": "quick_connect_targets",
                "temperature_f": 120,
                "humidity_percent": 80,
            },
            {"kind": "quick_connect_timer_duration", "minutes": 360},
        ]
        self.assertEqual(
            CONTROLS.entity_keys(device(commands=[c["kind"] for c in commands[1:4]]))[
                "number"
            ],
            {"automatic_temperature", "automatic_humidity", "timer_duration"},
        )
        session = FakeSession(
            *(
                FakeResponse({"request_id": "$request_id", "status": "confirmed"})
                for _ in commands
            )
        )
        client = CLIENT.ApiClient("http://proxy", session)
        for command in commands:
            await client.set_control("configured", command)
        self.assertEqual([post["json"]["command"] for post in session.posts], commands)
        self.assertEqual(len(session.posts), len(commands))
        for post in session.posts:
            self.assertIsInstance(post["json"]["request_id"], str)
            self.assertTrue(post["json"]["request_id"])

    async def test_uncertain_commands_preserve_request_without_retry_or_private_details(
        self,
    ):
        replies = [
            (TimeoutError("private host"), 200),
            (OSError("private connection"), 200),
            ({"request_id": "wrong", "status": "confirmed"}, 200),
        ]
        replies += [
            ({"request_id": "$request_id", "status": status}, code)
            for status, code in (("banana", 200), ("confirmed", 502), (None, 200))
        ]
        for payload, status in replies:
            with self.subTest(payload=payload):
                client, session = self.client(payload, status)
                with self.assertRaises(MODELS.ControlOutcomeUnknown) as raised:
                    await client.set_control("configured", COMMAND)
                request_id = session.posts[0]["json"]["request_id"]
                self.assertEqual(raised.exception.request_id, request_id)
                self.assertIn("outcome unknown", str(raised.exception))
                self.assertIn(request_id, str(raised.exception))
                self.assertNotIn("private", str(raised.exception))
                self.assertEqual(len(session.posts), 1)
        client, _ = self.client(
            {
                "request_id": "$request_id",
                "status": "unconfirmed",
                "message": "readback differed",
            },
            502,
        )
        with self.assertRaisesRegex(ApiError, "readback differed"):
            await client.set_control("configured", COMMAND)

    async def test_url_identity_and_transport_errors_are_safe(self):
        self.assertEqual(
            CLIENT.normalize_api_url(" http://127.0.0.1:8787/ "),
            "http://127.0.0.1:8787",
        )
        with self.assertRaises(ApiError):
            CLIENT.normalize_api_url("http://user:secret@127.0.0.1:8787")
        client, _ = self.client({})
        with self.assertRaisesRegex(ApiError, "invalid configured device"):
            await client.fetch_state("../other")
        client, _ = self.client({}, 503)
        with self.assertRaisesRegex(ApiError, "HTTP 503"):
            await client.fetch_devices()

        class FailedSession:
            def request(self, *_args, **_kwargs):
                raise OSError("private host detail")

        with self.assertRaisesRegex(ApiError, "cannot connect") as raised:
            await CLIENT.ApiClient("http://proxy", FailedSession()).fetch_devices()
        self.assertNotIn("private host detail", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
