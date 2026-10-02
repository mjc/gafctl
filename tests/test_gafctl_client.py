"""Standard-library tests for proxy response handling with a fake HTTP session."""

import importlib
import sys
import unittest
from dataclasses import FrozenInstanceError
from pathlib import Path
from types import ModuleType

COMPONENT_PATH = Path(__file__).parents[1] / "custom_components" / "gafctl"
PACKAGE = ModuleType("gafctl_client_tests")
PACKAGE.__path__ = [str(COMPONENT_PATH)]
sys.modules[PACKAGE.__name__] = PACKAGE
CLIENT = importlib.import_module("gafctl_client_tests.client")
MODELS = importlib.import_module("gafctl_client_tests.models")
CONTROLS = importlib.import_module("gafctl_client_tests.controls")
ApiClient = CLIENT.ApiClient
ApiError = MODELS.ApiError
normalize_api_url = CLIENT.normalize_api_url
Readings = MODELS.Readings
Device = MODELS.Device
LegacySettings = MODELS.LegacySettings
QuickConnectSettings = MODELS.QuickConnectSettings
Diagnostics = MODELS.Diagnostics
PROXY_ID = "550e8400-e29b-41d4-a716-446655440000"


def model_device(**overrides):
    return Device(
        **{
            "proxy_id": PROXY_ID,
            "id": "configured",
            "name": "Vent",
            "backend": "legacy_ble",
            "read_state": True,
            "commands": frozenset(),
            "owner": "http",
        }
        | overrides
    )


def inventory_device(**overrides: object) -> dict[str, object]:
    return {
        "proxy_id": PROXY_ID,
        "id": "configured",
        "name": "Vent",
        "backend": "legacy_ble",
        "capabilities": {"read_state": True, "commands": []},
        "state_source": "http",
        "command_source": "http",
    } | overrides


def legacy_state(**overrides: object) -> dict[str, object]:
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
        "diagnostics": {
            "firmware_version": "3.0.0",
            "signal_strength_raw": None,
            "verified_raw": None,
            "ota_in_progress": None,
        },
        "provenance": {
            "backend": "legacy_ble",
            "fetched_at_unix_ms": 2000,
            "observed_at_unix_ms": 1234,
        },
    }
    state.update(overrides)
    return state


def quickconnect_state(**overrides: object) -> dict[str, object]:
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
            "signal_strength_raw": "-45",
            "verified_raw": "true",
            "ota_in_progress": None,
        },
        "provenance": {
            "backend": "quick_connect",
            "fetched_at_unix_ms": 2000,
            "observed_at_unix_ms": None,
        },
    }
    state.update(overrides)
    return state


def v2_state_payload(
    state: dict[str, object] | None = None, **overrides: object
) -> dict[str, object]:
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
    def __init__(self, payload, status=200) -> None:
        self.payload = payload
        self.status = status

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_):
        return None

    async def json(self):
        if (
            isinstance(self.payload, dict)
            and self.payload.get("request_id") == "$request_id"
        ):
            return self.payload | {"request_id": self.request_id}
        return self.payload


class FakeSession:
    def __init__(self, responses) -> None:
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


class ApiClientTests(unittest.IsolatedAsyncioTestCase):
    async def test_normalized_models_are_immutable_and_detached_from_wire_data(
        self,
    ) -> None:
        raw = inventory_device(
            capabilities={"read_state": True, "commands": [{"kind": "future_command"}]}
        )
        selected = (
            await ApiClient(
                "http://proxy", FakeSession([FakeResponse({"devices": [raw]})])
            ).fetch_devices()
        )[0]
        raw["capabilities"]["commands"].clear()
        self.assertEqual(selected.commands, frozenset({"future_command"}))
        with self.assertRaises(FrozenInstanceError):
            selected.owner = "mqtt"
        payload = v2_state_payload(legacy_state())
        state = await ApiClient(
            "http://proxy", FakeSession([FakeResponse(payload)])
        ).fetch_state("configured")
        payload["state"]["settings"]["mode"] = "timer"
        self.assertEqual(state.state.settings.mode, "automatic")
        with self.assertRaises(FrozenInstanceError):
            state.state.settings.mode = "timer"

    def test_number_controls_validate_bounds_without_coercion(self) -> None:
        for backend in ("legacy_ble", "quick_connect"):
            for control in CONTROLS.NUMBER_CONTROLS[backend]:
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

    async def test_projects_reported_diagnostics_without_inventing_units_or_boolean_meaning(
        self,
    ) -> None:
        raw = quickconnect_state()
        raw["diagnostics"] = {
            "firmware_version": None,
            "signal_strength_raw": "-42",
            "verified_raw": "unknown-token",
            "ota_in_progress": True,
        }
        session = FakeSession(
            [FakeResponse(v2_state_payload(raw, backend="quick_connect"))]
        )
        result = (
            await ApiClient("http://proxy", session).fetch_state("configured")
        ).state
        self.assertEqual(result.diagnostics.signal_strength_raw, "-42")
        self.assertEqual(result.diagnostics.verified_raw, "unknown-token")
        self.assertIs(result.diagnostics.ota_in_progress, True)
        self.assertIs(result.settings.is_mode("automatic"), True)
        self.assertIs(result.settings.is_mode("timer"), False)
        self.assertIs(result.settings.is_mode("manual"), False)
        raw["settings"]["mode"] = "conflicting"
        result = CLIENT._decode_readings(raw, "quick_connect")
        self.assertIsNone(result.settings.is_mode("automatic"))
        self.assertIsNone(result.settings.is_mode("timer"))
        self.assertIsNone(result.settings.is_mode("manual"))

    async def test_adjustable_ble_capabilities_create_numbers_and_send_bounded_commands(
        self,
    ) -> None:
        commands = [
            {"kind": "legacy_automatic_temperature", "temperature_f": 90},
            {"kind": "legacy_automatic_humidity", "humidity_percent": 80},
            {"kind": "legacy_timer", "minutes": 360},
        ]
        device = model_device(
            backend="legacy_ble", commands=frozenset(item["kind"] for item in commands)
        )
        self.assertEqual(
            CONTROLS.entity_keys(device)["number"],
            {"automatic_temperature", "automatic_humidity", "timer_duration"},
        )
        session = FakeSession(
            [
                FakeResponse({"request_id": "$request_id", "status": "confirmed"})
                for _ in commands
            ]
        )
        client = ApiClient("http://proxy", session)
        for command in commands:
            await client.set_control("configured", command)
        self.assertEqual([post["json"]["command"] for post in session.posts], commands)
        for command in commands:
            field = next(key for key in command if key != "kind")
            for value in [-1, 361, 1.5, True, None]:
                with self.assertRaises(ApiError):
                    CONTROLS._control_command(command | {field: value})

    async def test_refresh_reads_device_once_and_rejects_cached_failed_outcome(
        self,
    ) -> None:
        payload = v2_state_payload(legacy_state(), status="fresh")
        session = FakeSession([FakeResponse(payload)])
        result = await ApiClient("http://proxy/prefix", session).refresh(
            "configured", "legacy_ble"
        )
        self.assertTrue(result.available)
        self.assertEqual(
            session.urls, ["http://proxy/prefix/api/v2/devices/configured/refresh"]
        )
        self.assertEqual(len(session.posts), 1)
        self.assertEqual(session.posts[0]["timeout"], 300)
        self.assertNotIn("json", session.posts[0])
        for status, http_status in (
            ("failed", 502),
            ("superseded", 409),
            ("fresh", 502),
            (None, 200),
        ):
            session = FakeSession(
                [FakeResponse(payload | {"status": status}, http_status)]
            )
            with self.assertRaises(ApiError):
                await ApiClient("http://proxy", session).refresh(
                    "configured", "legacy_ble"
                )

    async def test_inventory_requires_proxy_identity_and_one_owner(self) -> None:
        for fields in (
            {"proxy_id": None},
            {"proxy_id": "not-a-uuid"},
            {"proxy_id": "00000000-0000-0000-0000-000000000000"},
            {"state_source": "mqtt", "command_source": "http"},
            {"state_source": None},
        ):
            with self.subTest(fields=fields):
                client = ApiClient(
                    "http://127.0.0.1:8787",
                    FakeSession(
                        [FakeResponse({"devices": [inventory_device(**fields)]})]
                    ),
                )
                with self.assertRaises(ApiError):
                    await client.fetch_devices()

    async def test_inventory_preserves_proxy_identity_and_rejects_mixed_proxies(
        self,
    ) -> None:
        client = ApiClient(
            "http://127.0.0.1:8787",
            FakeSession([FakeResponse({"devices": [inventory_device()]})]),
        )
        self.assertEqual((await client.fetch_devices())[0].proxy_id, PROXY_ID)
        client = ApiClient(
            "http://127.0.0.1:8787",
            FakeSession(
                [
                    FakeResponse(
                        {
                            "devices": [
                                inventory_device(),
                                inventory_device(
                                    id="other",
                                    proxy_id="650e8400-e29b-41d4-a716-446655440000",
                                ),
                            ]
                        }
                    )
                ]
            ),
        )
        with self.assertRaises(ApiError):
            await client.fetch_devices()

    def test_timer_preset_requires_matching_remaining_and_original_duration(
        self,
    ) -> None:
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
                        Readings(
                            settings=LegacySettings(
                                timer_remaining_minutes=remaining,
                                timer_original_minutes=original,
                            )
                        )
                    ),
                    expected,
                )

    def test_normalizes_proxy_url_and_rejects_embedded_credentials(self) -> None:
        self.assertEqual(
            normalize_api_url(" http://127.0.0.1:8787/ "), "http://127.0.0.1:8787"
        )
        with self.assertRaises(ApiError):
            normalize_api_url("http://user:secret@127.0.0.1:8787")

    async def test_discovers_devices_without_returning_private_identifiers(
        self,
    ) -> None:
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
                                    "commands": [
                                        {
                                            "kind": "legacy_preset",
                                            "value": "timer_clear",
                                        }
                                    ],
                                },
                                "peripheral_id": "private-peripheral-id",
                            }
                        ]
                    }
                )
            ]
        )
        client = ApiClient("http://127.0.0.1:8787", session)
        devices = await client.fetch_devices()
        self.assertEqual(devices[0].id, "configured")
        self.assertFalse(hasattr(devices[0], "peripheral_id"))
        self.assertEqual(session.urls, ["http://127.0.0.1:8787/api/v2/devices"])

    async def test_inventory_keeps_capabilities_for_explicit_device_selection(
        self,
    ) -> None:
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
                                        {
                                            "kind": "legacy_preset",
                                            "value": "timer_clear",
                                        }
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
        devices = await ApiClient("http://ha:8787", session).fetch_devices()
        self.assertEqual(
            CONTROLS.select_device(devices, "cloud-device").name, "QuickConnect Vent"
        )
        self.assertEqual(
            CONTROLS.entity_platforms(devices[1]),
            {"sensor", "binary_sensor", "select", "number", "button", "switch"},
        )
        self.assertEqual(
            CONTROLS.entity_keys(devices[1]),
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
        self.assertNotIn("number", CONTROLS.entity_platforms(devices[0]))
        self.assertEqual(
            {
                control.key: (control.minimum, control.maximum, control.step)
                for control in CONTROLS.NUMBER_CONTROLS["quick_connect"]
            },
            {
                "automatic_temperature": (90, 120, 1),
                "automatic_humidity": (30, 80, 1),
                "timer_duration": (30, 360, 30),
            },
        )

    def test_device_selection_never_falls_back_to_first_device(self) -> None:
        devices = [
            model_device(
                id="first",
                read_state=True,
                backend="legacy_ble",
                commands=frozenset([]),
            ),
            model_device(
                id="second",
                read_state=True,
                backend="quick_connect",
                commands=frozenset([]),
            ),
        ]
        with self.assertRaisesRegex(ApiError, "selected device is unavailable"):
            CONTROLS.select_device(devices, "missing")

    async def test_maps_quickconnect_measurements_targets_mode_and_running_provenance(
        self,
    ) -> None:
        client = ApiClient(
            "http://ha:8787",
            FakeSession(
                [
                    FakeResponse(
                        v2_state_payload(quickconnect_state(), backend="quick_connect")
                    )
                ]
            ),
        )
        result = await client.fetch_state("configured")
        self.assertEqual(result.state.temperature_f, 101.4)
        self.assertEqual(result.state.settings.automatic_temperature_f, 105)
        self.assertEqual(result.state.settings.timer_duration_minutes, 60)
        self.assertEqual(result.state.settings.mode, "automatic")
        self.assertIs(result.state.settings.humidity_monitor, True)
        self.assertIs(result.state.estimated_running, True)
        self.assertIsInstance(result.state.settings, QuickConnectSettings)
        self.assertFalse(hasattr(result.state.settings, "timer_remaining_minutes"))
        self.assertFalse(hasattr(result.state, "signal_strength"))
        self.assertFalse(hasattr(result.state, "is_verified"))

    def test_device_source_transition_removes_http_entity_ownership(self) -> None:
        device = model_device(
            id="cloud-device",
            backend="quick_connect",
            read_state=True,
            commands=frozenset(["quick_connect_mode"]),
            owner="mqtt",
        )
        self.assertEqual(CONTROLS.entity_platforms(device), set())

    async def test_mixed_sources_are_rejected_before_entity_creation(self) -> None:
        for state_owner, command_owner in (("mqtt", "http"), ("http", "mqtt")):
            session = FakeSession(
                [
                    FakeResponse(
                        {
                            "devices": [
                                inventory_device(
                                    state_source=state_owner,
                                    command_source=command_owner,
                                )
                            ]
                        }
                    )
                ]
            )
            with self.assertRaises(ApiError):
                await ApiClient("http://proxy", session).fetch_devices()

    async def test_accepts_an_empty_device_inventory(self) -> None:
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse({"devices": []})])
        )
        self.assertEqual(await client.fetch_devices(), [])

    async def test_rejects_duplicate_inventory_ids(self) -> None:
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
            "http://ha:8787", FakeSession([FakeResponse({"devices": [device, device]})])
        )
        with self.assertRaisesRegex(ApiError, "duplicate device identifiers"):
            await client.fetch_devices()

    async def test_fetches_state_and_preserves_freshness(self) -> None:
        values = Readings(
            settings=LegacySettings(
                mode="automatic",
                controller_fan_on=False,
                automatic_temperature_tenths_f=1050,
                automatic_humidity_tenths_percent=300,
                timer_remaining_minutes=0,
                timer_original_minutes=0,
            ),
            diagnostics=Diagnostics(firmware_version="3.0.0"),
            temperature_f=98.6,
            humidity_percent=42.1,
        )
        payload = v2_state_payload(
            legacy_state() | {"identity_suffix": "private-suffix"},
            peripheral_id="private-peripheral-id",
        )
        session = FakeSession([FakeResponse(payload)])
        client = ApiClient("http://127.0.0.1:8787/", session)
        result = await client.fetch_state("configured")
        self.assertEqual(result.state, values)
        self.assertEqual(result.freshness, "fresh")
        self.assertFalse(hasattr(result, "peripheral_id"))
        self.assertFalse(hasattr(result.state, "identity_suffix"))
        self.assertEqual(
            session.urls, ["http://127.0.0.1:8787/api/v2/devices/configured/state"]
        )

    async def test_rejects_path_injection_in_device_ids(self) -> None:
        client = ApiClient("http://127.0.0.1:8787", FakeSession([]))
        with self.assertRaisesRegex(ApiError, "invalid configured device"):
            await client.fetch_state("../other")

    async def test_preserves_stale_unavailable_state_without_inventing_values(
        self,
    ) -> None:
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
        result = await client.fetch_state("configured")
        self.assertIsNone(result.state)
        self.assertFalse(result.available)
        self.assertEqual(result.freshness, "stale")

    async def test_sends_only_supported_controls_and_requires_confirmation(
        self,
    ) -> None:
        response = FakeResponse({"request_id": "$request_id", "status": "confirmed"})
        session = FakeSession([response])
        client = ApiClient("http://127.0.0.1:8787", session)
        await client.set_control("configured", "timer_clear")
        self.assertEqual(
            session.urls, ["http://127.0.0.1:8787/api/v2/devices/configured/control"]
        )
        request = session.posts[0]["json"]
        self.assertEqual(
            request["command"], {"kind": "legacy_preset", "preset": "timer_clear"}
        )
        self.assertTrue(request["request_id"])
        self.assertIsInstance(request["issued_at_unix_ms"], int)
        self.assertEqual(session.posts[0]["timeout"], 300)
        with self.assertRaisesRegex(ApiError, "unsupported control preset"):
            await client.set_control("configured", "timer_999")

    async def test_control_timeout_and_disconnect_preserve_unknown_request_without_retry(
        self,
    ) -> None:

        class LostResponse(FakeResponse):
            async def __aenter__(self):
                raise self.payload

        for error in (TimeoutError("private host"), OSError("private connection")):
            session = FakeSession([LostResponse(error)])
            client = ApiClient("http://proxy", session)
            with self.assertRaises(ApiError) as raised:
                await client.set_control("configured", "timer_clear")
            self.assertEqual(
                raised.exception.request_id, session.posts[0]["json"]["request_id"]
            )
            self.assertIn("outcome unknown", str(raised.exception))
            self.assertIn(raised.exception.request_id, str(raised.exception))
            self.assertNotIn("private", str(raised.exception))
            self.assertEqual(len(session.posts), 1)

    async def test_sends_quickconnect_mode_with_correlation_and_exact_command_shape(
        self,
    ) -> None:
        response = FakeResponse({"request_id": "$request_id", "status": "confirmed"})
        session = FakeSession([response])
        client = ApiClient("http://ha:8787", session)
        await client.set_control(
            "quick-device", {"kind": "quick_connect_mode", "mode": "manual"}
        )
        request = session.posts[0]["json"]
        self.assertEqual(
            request["command"], {"kind": "quick_connect_mode", "mode": "manual"}
        )
        self.assertIsInstance(request["request_id"], str)
        self.assertEqual(request["request_id"], response.request_id)

    async def test_rejects_invalid_quickconnect_values_before_post(self) -> None:
        session = FakeSession([])
        client = ApiClient("http://ha:8787", session)
        for command in (
            {
                "kind": "quick_connect_targets",
                "temperature_f": 90.5,
                "humidity_percent": 40,
            },
            {
                "kind": "quick_connect_targets",
                "temperature_f": 121,
                "humidity_percent": 40,
            },
            {"kind": "quick_connect_timer_duration", "minutes": 45},
        ):
            with self.subTest(command=command), self.assertRaises(ApiError):
                await client.set_control("quick-device", command)
        self.assertEqual(session.posts, [])

    async def test_sends_valid_quickconnect_targets_and_duration_as_typed_commands(
        self,
    ) -> None:
        responses = [
            FakeResponse({"request_id": "$request_id", "status": "confirmed"})
            for _ in range(2)
        ]
        session = FakeSession(responses)
        client = ApiClient("http://ha:8787", session)
        await client.set_control(
            "quick-device",
            {
                "kind": "quick_connect_targets",
                "temperature_f": 120,
                "humidity_percent": 80,
            },
        )
        await client.set_control(
            "quick-device", {"kind": "quick_connect_timer_duration", "minutes": 360}
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

    async def test_rejects_unconfirmed_control_readback(self) -> None:
        client = ApiClient(
            "http://127.0.0.1:8787",
            FakeSession(
                [
                    FakeResponse(
                        {
                            "request_id": "$request_id",
                            "status": "unconfirmed",
                            "message": "readback differed",
                        },
                        status=502,
                    )
                ]
            ),
        )
        with self.assertRaisesRegex(ApiError, "readback differed"):
            await client.set_control("configured", "timer_clear")

    async def test_unknown_or_inconsistent_control_reply_preserves_uncertain_submission(
        self,
    ) -> None:
        for status, http_status in (("banana", 200), ("confirmed", 502), (None, 200)):
            session = FakeSession(
                [
                    FakeResponse(
                        {"request_id": "$request_id", "status": status}, http_status
                    )
                ]
            )
            with self.assertRaises(MODELS.ControlOutcomeUnknown) as raised:
                await ApiClient("http://proxy", session).set_control(
                    "configured", "timer_clear"
                )
            self.assertEqual(
                raised.exception.request_id, session.posts[0]["json"]["request_id"]
            )
            self.assertIn("outcome unknown", str(raised.exception))
            self.assertEqual(len(session.posts), 1)

    async def test_rejects_mismatched_control_confirmation(self) -> None:
        client = ApiClient(
            "http://127.0.0.1:8787",
            FakeSession([FakeResponse({"request_id": "wrong", "status": "confirmed"})]),
        )
        with self.assertRaisesRegex(ApiError, "outcome unknown"):
            await client.set_control("configured", "timer_clear")

    async def test_rejects_inconsistent_or_invalid_proxy_responses(self) -> None:
        invalid = v2_state_payload(legacy_state(), available=False)
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse(invalid)])
        )
        with self.assertRaisesRegex(ApiError, "inconsistent"):
            await client.fetch_state("configured")

    async def test_rejects_invalid_inventory_status(self) -> None:
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
                    "http://127.0.0.1:8787", FakeSession([FakeResponse(payload)])
                )
                with self.assertRaisesRegex(ApiError, "invalid device state"):
                    await client.fetch_state("configured")

    async def test_nullable_readings_preserve_unknown_values(self) -> None:
        for backend, raw in (
            ("legacy_ble", legacy_state()),
            ("quick_connect", quickconnect_state()),
        ):
            raw["temperature_f"] = None
            raw["humidity_percent"] = None
            raw["estimated_running"] = None
            raw["diagnostics"] = None
            raw["settings"] = {
                key: value if key in {"backend", "mode"} else None
                for key, value in raw["settings"].items()
            }
            if backend == "legacy_ble":
                raw["settings"]["mode"] = None
            with self.subTest(backend=backend):
                result = await ApiClient(
                    "http://proxy",
                    FakeSession([FakeResponse(v2_state_payload(raw, backend=backend))]),
                ).fetch_state("configured")
                self.assertIsNone(result.state.temperature_f)
                self.assertIsNone(result.state.humidity_percent)
                self.assertIsNone(result.state.settings.automatic_temperature_f)
                self.assertIsNone(result.state.settings.automatic_humidity_percent)
                self.assertIsNone(result.state.settings.timer_duration_minutes)
                self.assertEqual(result.state.diagnostics, Diagnostics())
                self.assertIsNone(result.state.estimated_running)

    async def test_rejects_incomplete_or_mismatched_state(self) -> None:
        for changed in (
            "settings",
            "temperature_f",
            "diagnostics",
            "estimated_running",
            "settings_backend",
            "provenance_backend",
            "observed_at",
        ):
            raw = legacy_state()
            if changed == "settings_backend":
                raw["settings"]["backend"] = "quick_connect"
            elif changed == "provenance_backend":
                raw["provenance"]["backend"] = "quick_connect"
            elif changed == "observed_at":
                del raw["provenance"]["observed_at_unix_ms"]
            else:
                del raw[changed]
            with self.subTest(changed=changed), self.assertRaises(ApiError):
                await ApiClient(
                    "http://proxy", FakeSession([FakeResponse(v2_state_payload(raw))])
                ).fetch_state("configured")

    async def test_maps_http_failures_to_safe_errors(self) -> None:
        client = ApiClient(
            "http://127.0.0.1:8787", FakeSession([FakeResponse({}, status=503)])
        )
        with self.assertRaisesRegex(ApiError, "HTTP 503"):
            await client.fetch_devices()

    async def test_maps_connection_failures_without_exposing_exception_details(
        self,
    ) -> None:
        client = ApiClient("http://127.0.0.1:8787", FailedSession())
        with self.assertRaisesRegex(ApiError, "cannot connect") as raised:
            await client.fetch_devices()
        self.assertNotIn("private host detail", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
