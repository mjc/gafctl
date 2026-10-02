"""Registry tests run with the installed Home Assistant Python environment."""

import asyncio
import json
import os
import shutil
import sys
import tempfile
import unittest
from dataclasses import replace
from functools import partial
from pathlib import Path
from types import MappingProxyType
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from homeassistant.components.mqtt import binary_sensor as mqtt_binary
from homeassistant.components.mqtt import button as mqtt_button
from homeassistant.components.mqtt import number as mqtt_number
from homeassistant.components.mqtt import select as mqtt_select
from homeassistant.components.mqtt import sensor as mqtt_sensor
from homeassistant.components.mqtt import switch as mqtt_switch
from homeassistant.config_entries import ConfigEntries, ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.template import Template

from custom_components.gafctl import GafctlCoordinator
from custom_components.gafctl import binary_sensor as gafctl_binary
from custom_components.gafctl import button as gafctl_button
from custom_components.gafctl import number as gafctl_number
from custom_components.gafctl import sensor as gafctl_sensor
from custom_components.gafctl import switch as gafctl_switch
from custom_components.gafctl.button import GafctlRefreshButton
from custom_components.gafctl.config_flow import GafctlConfigFlow
from custom_components.gafctl.controls import number_controls
from custom_components.gafctl.models import (
    ApiError,
    ControlOutcomeUnknown,
    Device,
    DeviceState,
    Readings,
)

COMPONENT_DIR = Path(__file__).resolve().parents[1] / "custom_components/gafctl"
PROXY_ID = "550e8400-e29b-41d4-a716-446655440000"


def device(proxy_id=PROXY_ID, owner="http"):
    return Device(
        proxy_id=proxy_id,
        id="configured",
        name="Vent",
        backend="legacy_ble",
        read_state=True,
        commands=frozenset(),
        owner=owner,
    )


def state_data(*, state=None, available=True, freshness="fresh", backend="legacy_ble"):
    return DeviceState(
        device_id="configured",
        backend=backend,
        available=available,
        freshness=freshness,
        observed_at_unix_ms=None,
        last_error=None,
        state=state,
    )


def read_discovery_fixture(path: str) -> list[tuple[str, dict[str, object]]]:
    return json.loads(Path(path).read_text())


class RegistryTests(unittest.IsolatedAsyncioTestCase):
    async def test_onboarding_bulk_selects_only_http_owned_unconfigured_devices(
        self,
    ) -> None:
        await self.entry()
        eligible = [
            replace(device(), id=key, read_state=True, commands=frozenset([]))
            for key in ("one", "two")
        ]
        flow = GafctlConfigFlow()
        flow.hass = self.hass
        flow.handler = "gafctl"
        flow.context = {"source": "user"}
        client = AsyncMock()
        client.fetch_devices.return_value = [
            device(),
            replace(device(owner="mqtt"), id="mqtt"),
            *eligible,
        ]
        with (
            patch(
                "custom_components.gafctl.config_flow.async_get_clientsession",
                return_value=object(),
            ),
            patch(
                "custom_components.gafctl.config_flow.ApiClient", return_value=client
            ),
        ):
            result = await flow.async_step_user({"api_url": "http://proxy:8787"})
            self.assertEqual(result["step_id"], "device")
            self.assertEqual(set(flow._devices), {"one", "two"})
            with patch.object(
                type(self.hass.config_entries.flow),
                "async_init",
                AsyncMock(return_value={"type": "create_entry"}),
            ) as imported:
                result = await flow.async_step_device({"device_ids": ["one", "two"]})
            self.assertEqual(result["type"], "create_entry")
            self.assertEqual(result["data"]["device_id"], "one")
            imported.assert_awaited_once()
            self.assertEqual(imported.await_args.kwargs["data"]["device_id"], "two")

    async def test_real_flow_manager_creates_separate_entries_for_bulk_selection(
        self,
    ) -> None:
        await asyncio.to_thread(
            shutil.copytree,
            COMPONENT_DIR,
            self.hass.config.path("custom_components/gafctl"),
        )
        from homeassistant import loader

        loader.async_setup(self.hass)
        selected = [
            replace(device(), id=key, read_state=True, commands=frozenset([]))
            for key in ("one", "two")
        ]
        client = AsyncMock()
        client.fetch_devices.return_value = selected
        with (
            patch(
                "custom_components.gafctl.config_flow.async_get_clientsession",
                return_value=object(),
            ),
            patch(
                "custom_components.gafctl.config_flow.ApiClient", return_value=client
            ),
            patch.object(ConfigEntries, "async_setup", AsyncMock(return_value=True)),
        ):
            result = await self.hass.config_entries.flow.async_init(
                "gafctl", context={"source": "user"}
            )
            result = await self.hass.config_entries.flow.async_configure(
                result["flow_id"], {"api_url": "http://proxy:8787"}
            )
            result = await self.hass.config_entries.flow.async_configure(
                result["flow_id"], {"device_ids": ["one", "two"]}
            )
            await self.hass.async_block_till_done()
        self.assertEqual(result["type"], "create_entry")
        entries = self.hass.config_entries.async_entries("gafctl")
        self.assertEqual({entry.data["device_id"] for entry in entries}, {"one", "two"})
        self.assertEqual(
            {entry.unique_id for entry in entries},
            {f"gafctl_{PROXY_ID}_one", f"gafctl_{PROXY_ID}_two"},
        )

    async def test_onboarding_rejects_empty_or_changed_selection_before_import(
        self,
    ) -> None:
        flow = GafctlConfigFlow()
        flow.hass = self.hass
        flow.handler = "gafctl"
        flow.context = {"source": "user"}
        selected = replace(device(), read_state=True, commands=frozenset([]))
        flow._api_url = "http://proxy:8787"
        flow._devices = {selected.id: selected}
        for changed in (
            device(owner="mqtt"),
            device("650e8400-e29b-41d4-a716-446655440000"),
        ):
            with patch.object(
                flow, "_fetch_devices", AsyncMock(return_value=[changed])
            ):
                result = await flow.async_step_device({"device_ids": ["configured"]})
            self.assertEqual(result["type"], "form")
            self.assertEqual(result["errors"]["base"], "device_unavailable")
        result = await flow.async_step_device({"device_ids": []})
        self.assertEqual(result["errors"]["base"], "device_unavailable")

    async def test_import_checks_identity_and_existing_entry_without_duplicates(
        self,
    ) -> None:
        await self.entry()
        for proxy_id in (PROXY_ID, "650e8400-e29b-41d4-a716-446655440000"):
            flow = GafctlConfigFlow()
            flow.hass = self.hass
            flow.handler = "gafctl"
            flow.context = {"source": "import"}
            selected = replace(
                device(proxy_id), read_state=True, commands=frozenset([])
            )
            with patch.object(
                flow, "_fetch_devices", AsyncMock(return_value=[selected])
            ):
                if proxy_id == PROXY_ID:
                    from homeassistant.data_entry_flow import AbortFlow

                    with self.assertRaises(AbortFlow) as raised:
                        await flow.async_step_import(
                            {
                                "api_url": "http://proxy:8787",
                                "device_id": "configured",
                                "proxy_id": PROXY_ID,
                                "backend": "legacy_ble",
                            }
                        )
                    self.assertEqual(raised.exception.reason, "already_configured")
                else:
                    result = await flow.async_step_import(
                        {
                            "api_url": "http://proxy:8787",
                            "device_id": "configured",
                            "proxy_id": PROXY_ID,
                            "backend": "legacy_ble",
                        }
                    )
                    self.assertEqual(result["reason"], "device_unavailable")

    async def test_reconfigure_preserves_identity_and_rejects_another_proxy(
        self,
    ) -> None:
        entry = await self.entry()
        registered, registered_device = self.registered_sensor(entry)
        for proxy_id, expected in (
            ("650e8400-e29b-41d4-a716-446655440000", "wrong_device"),
            (PROXY_ID, None),
        ):
            flow = GafctlConfigFlow()
            flow.hass = self.hass
            flow.handler = "gafctl"
            flow.context = {"source": "reconfigure", "entry_id": entry.entry_id}
            client = AsyncMock()
            client.fetch_devices.return_value = [device(proxy_id)]
            with (
                patch(
                    "custom_components.gafctl.config_flow.async_get_clientsession",
                    return_value=object(),
                ),
                patch(
                    "custom_components.gafctl.config_flow.ApiClient",
                    return_value=client,
                ),
                patch.object(
                    ConfigEntries, "async_reload", AsyncMock(return_value=True)
                ),
            ):
                result = await flow.async_step_reconfigure(
                    {"api_url": "http://new-proxy:8787"}
                )
            if expected:
                self.assertEqual(result["errors"]["base"], expected)
                self.assertEqual(entry.data["api_url"], "http://127.0.0.1:8787")
            else:
                self.assertEqual(result["type"], "abort")
                self.assertEqual(entry.data["api_url"], "http://new-proxy:8787")
                self.assertEqual(entry.unique_id, f"gafctl_{PROXY_ID}_configured")
                self.assertEqual(
                    self.entities.async_get(registered.entity_id).device_id,
                    registered_device.id,
                )

    async def test_reported_sensors_keep_raw_diagnostics_and_original_timer(
        self,
    ) -> None:
        entry = await self.entry()
        for backend, values, expected in (
            ("legacy_ble", {"timer_original_minutes": 2}, {"timer_original": 2}),
            (
                "quick_connect",
                {
                    "signal_strength_raw": "unknown-units",
                    "verified_raw": "unknown-semantics",
                },
                {
                    "signal_strength_raw": "unknown-units",
                    "verified_raw": "unknown-semantics",
                },
            ),
        ):
            coordinator = GafctlCoordinator(
                self.hass, AsyncMock(), replace(device(), backend=backend), entry
            )
            coordinator.async_set_updated_data(
                state_data(
                    backend=backend,
                    available=True,
                    freshness="fresh",
                    state=Readings(**values),
                )
            )
            entry.runtime_data = coordinator
            entities = []
            await gafctl_sensor.async_setup_entry(self.hass, entry, entities.extend)
            selected = {entity.entity_description.key: entity for entity in entities}
            for key, value in expected.items():
                self.assertEqual(selected[key].native_value, value)
                if key.endswith("_raw"):
                    self.assertIsNone(selected[key].native_unit_of_measurement)
                    self.assertIsNone(selected[key].device_class)

    async def test_switches_and_all_off_button_delegate_only_advertised_mode_control(
        self,
    ) -> None:
        entry = await self.entry()
        selected = replace(
            device(),
            backend="quick_connect",
            commands=frozenset(["quick_connect_mode"]),
        )
        coordinator = GafctlCoordinator(self.hass, AsyncMock(), selected, entry)
        coordinator.async_set_updated_data(
            state_data(
                backend="quick_connect",
                available=True,
                freshness="fresh",
                state=Readings(mode="automatic"),
            )
        )
        entry.runtime_data = coordinator
        coordinator.async_set_mode = AsyncMock()
        switches, buttons = ([], [])
        await gafctl_switch.async_setup_entry(self.hass, entry, switches.extend)
        await gafctl_button.async_setup_entry(self.hass, entry, buttons.extend)
        self.assertEqual([switch.is_on for switch in switches], [True, False, False])
        self.assertTrue(all(switch.available for switch in switches))
        await switches[2].async_turn_on()
        coordinator.async_set_mode.assert_awaited_with("manual", only_if_current=None)
        await switches[1].async_turn_off()
        coordinator.async_set_mode.assert_awaited_with("off", only_if_current="timer")
        all_off = next(
            button for button in buttons if button.unique_id.endswith("_all_off")
        )
        await all_off.async_press()
        coordinator.async_set_mode.assert_awaited_with("off")
        coordinator.device = replace(selected, commands=frozenset([]))
        self.assertFalse(all_off.available)
        self.assertTrue(all(not switch.available for switch in switches))
        empty = []
        await gafctl_switch.async_setup_entry(self.hass, entry, empty.extend)
        self.assertEqual(empty, [])

    async def test_mode_control_rejects_mismatched_readback_and_unknown_conditional_off(
        self,
    ) -> None:
        entry = await self.entry()
        selected = replace(
            device(),
            backend="quick_connect",
            commands=frozenset(["quick_connect_mode"]),
        )
        self.hass.config_entries.async_update_entry(
            entry, data=dict(entry.data) | {"backend": "quick_connect"}
        )
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        old = state_data(
            backend="quick_connect",
            available=True,
            freshness="fresh",
            state=Readings(mode="automatic"),
        )
        client.fetch_state.return_value = old
        coordinator = GafctlCoordinator(self.hass, client, selected, entry)
        with self.assertRaises(ApiError):
            await coordinator.async_set_mode("manual")
        client.set_control.assert_awaited_once()
        client.set_control.reset_mock()
        client.fetch_state.return_value = replace(
            old, state=Readings(mode="conflicting")
        )
        with self.assertRaises(ApiError):
            await coordinator.async_set_mode("off", only_if_current="automatic")
        client.set_control.assert_not_called()

    async def test_diagnostic_binary_sensors_preserve_unknown_and_reported_values(
        self,
    ) -> None:
        entry = await self.entry()
        for backend, values, expected in (
            (
                "legacy_ble",
                {"controller_fan_flag": False},
                {"controller_fan_flag": False},
            ),
            (
                "quick_connect",
                {
                    "running_estimate": None,
                    "ota_in_progress": True,
                    "automatic_mode": None,
                    "timer_mode": None,
                    "manual_mode": None,
                    "humidity_monitor": None,
                },
                {
                    "running_estimate": None,
                    "ota_in_progress": True,
                    "automatic_mode": None,
                    "timer_mode": None,
                    "manual_mode": None,
                    "humidity_monitor": None,
                },
            ),
        ):
            selected = replace(device(), backend=backend)
            coordinator = GafctlCoordinator(self.hass, AsyncMock(), selected, entry)
            coordinator.async_set_updated_data(
                state_data(
                    backend=backend,
                    available=True,
                    freshness="fresh",
                    state=Readings(**values),
                )
            )
            entry.runtime_data = coordinator
            entities = []
            await gafctl_binary.async_setup_entry(self.hass, entry, entities.extend)
            result = {
                entity.unique_id.removeprefix(entry.unique_id + "_"): entity.is_on
                for entity in entities
            }
            self.assertEqual(result, expected)
            self.assertTrue(all(entity.available for entity in entities))

    async def test_mode_controls_recheck_identity_and_confirm_current_mode(
        self,
    ) -> None:
        entry = await self.entry()
        selected = replace(
            device(),
            backend="quick_connect",
            commands=frozenset(["quick_connect_mode"]),
        )
        entry_data = dict(entry.data) | {"backend": "quick_connect"}
        self.hass.config_entries.async_update_entry(entry, data=entry_data)
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        old = state_data(
            backend="quick_connect",
            available=True,
            freshness="fresh",
            state=Readings(mode="automatic"),
        )
        new = replace(old, state=Readings(mode="off"))
        client.fetch_state.side_effect = [old, new]
        coordinator = GafctlCoordinator(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        await coordinator.async_set_mode("off", only_if_current="automatic")
        client.set_control.assert_awaited_once_with(
            "configured",
            {"kind": "quick_connect_conditional_off", "only_if_current": "automatic"},
        )
        self.assertEqual(coordinator.data, new)
        client.set_control.reset_mock()
        client.fetch_state.side_effect = None
        client.fetch_state.return_value = old
        await coordinator.async_set_mode("off", only_if_current="timer")
        client.set_control.assert_not_called()
        client.fetch_devices.return_value = [replace(selected, owner="mqtt")]
        with self.assertRaises(ApiError):
            await coordinator.async_set_mode("manual")
        client.set_control.assert_not_called()

    async def test_ble_timer_can_replace_manual_sentinel_with_bounded_timer(
        self,
    ) -> None:
        entry = await self.entry()
        selected = replace(device(), commands=frozenset(["legacy_timer"]))
        old = state_data(
            available=True,
            freshness="fresh",
            state=Readings(timer_original_minutes=600),
        )
        new = replace(old, state=Readings(timer_original_minutes=1))
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        client.fetch_state.side_effect = [old, new]
        coordinator = GafctlCoordinator(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        entry.runtime_data = coordinator
        entities = []
        await gafctl_number.async_setup_entry(self.hass, entry, entities.extend)
        timer = entities[0]
        self.assertTrue(timer.available)
        self.assertIsNone(timer.native_value)
        await timer.async_set_native_value(1)
        client.set_control.assert_awaited_once_with(
            "configured", {"kind": "legacy_timer", "minutes": 1}
        )

    async def test_ble_numbers_accept_fractional_readback_and_send_partial_command(
        self,
    ) -> None:
        entry = await self.entry()
        selected = replace(
            device(),
            commands=frozenset(
                [
                    "legacy_automatic_temperature",
                    "legacy_automatic_humidity",
                    "legacy_timer",
                ]
            ),
        )
        old = state_data(
            available=True,
            freshness="fresh",
            state=Readings(
                automatic_temperature_threshold_f=105.1,
                automatic_humidity_threshold_percent=30.1,
                timer_original_minutes=0,
            ),
        )
        new = replace(
            old, state=replace(old.state, automatic_temperature_threshold_f=110.0)
        )
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        client.fetch_state.side_effect = [old, new]
        coordinator = GafctlCoordinator(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        entry.runtime_data = coordinator
        entities = []
        await gafctl_number.async_setup_entry(self.hass, entry, entities.extend)
        self.assertEqual(len(entities), 3)
        temperature, humidity, timer = entities
        self.assertTrue(temperature.available)
        self.assertEqual(temperature.native_value, 105.1)
        self.assertTrue(humidity.available)
        self.assertEqual(humidity.native_value, 30.1)
        self.assertEqual(timer.native_min_value, 0)
        self.assertEqual(timer.native_step, 1)
        await temperature.async_set_native_value(110)
        client.set_control.assert_awaited_once_with(
            "configured", {"kind": "legacy_automatic_temperature", "temperature_f": 110}
        )
        for invalid in [89, 121, 105.1, True, float("nan"), float("inf")]:
            with self.assertRaises(HomeAssistantError):
                await temperature.async_set_native_value(invalid)
        self.assertEqual(client.set_control.await_count, 1)

    async def test_cloud_number_sends_only_selected_target(self) -> None:
        entry = await self.entry()
        selected = replace(
            device(),
            backend="quick_connect",
            commands=frozenset(["quick_connect_targets"]),
        )
        old = state_data(
            backend="quick_connect",
            available=True,
            freshness="fresh",
            state=Readings(automatic_temperature_f=105, automatic_humidity_percent=40),
        )
        new = replace(
            old,
            state=Readings(automatic_temperature_f=110, automatic_humidity_percent=45),
        )
        self.hass.config_entries.async_update_entry(
            entry, data=dict(entry.data) | {"backend": "quick_connect"}
        )
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        client.fetch_state.side_effect = [old, new]
        coordinator = GafctlCoordinator(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        entry.runtime_data = coordinator
        numbers = []
        await gafctl_number.async_setup_entry(self.hass, entry, numbers.extend)
        await numbers[0].async_set_native_value(110)
        client.set_control.assert_awaited_once_with(
            "configured",
            {"kind": "quick_connect_automatic_temperature", "temperature_f": 110},
        )

    async def control_case(self, operation):
        entry = await self.entry()
        backend = "quick_connect" if operation == "mode" else "legacy_ble"
        capability = {
            "mode": "quick_connect_mode",
            "number": "legacy_automatic_temperature",
            "preset": "legacy_preset",
        }[operation]
        selected = replace(device(), backend=backend, commands=frozenset({capability}))
        self.hass.config_entries.async_update_entry(
            entry, data=dict(entry.data) | {"backend": backend}
        )
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        old = state_data(
            backend=backend,
            state=Readings(
                mode="automatic",
                automatic_temperature_threshold_f=105,
                timer_original_minutes=0,
                timer_remaining_minutes=0,
            ),
        )
        new = replace(
            old,
            state=replace(
                old.state,
                mode="manual" if operation == "mode" else "timer",
                automatic_temperature_threshold_f=110,
                timer_original_minutes=1,
                timer_remaining_minutes=1,
            ),
        )
        client.fetch_state.side_effect = [old, new]
        coordinator = GafctlCoordinator(self.hass, client, selected, entry)
        if operation == "mode":
            submit = partial(coordinator.async_set_mode, "manual")
        elif operation == "number":
            submit = partial(
                coordinator.async_set_number, number_controls("legacy_ble")[0], 110
            )
        else:
            submit = partial(coordinator.async_set_preset, "timer_one_minute")
        return coordinator, client, selected, old, new, submit

    async def test_all_controls_preserve_unknown_request_when_refresh_fails(
        self,
    ) -> None:
        for operation in ("mode", "number", "preset"):
            with self.subTest(operation=operation):
                coordinator, client, _, old, _, submit = await self.control_case(
                    operation
                )
                uncertain = ControlOutcomeUnknown("request-under-test")
                client.set_control.side_effect = uncertain
                client.fetch_state.side_effect = [old, ApiError("readback unavailable")]
                with self.assertRaises(ControlOutcomeUnknown) as raised:
                    await submit()
                self.assertIs(raised.exception, uncertain)
                self.assertEqual(raised.exception.request_id, "request-under-test")
                client.set_control.assert_awaited_once()
                self.assertEqual(client.fetch_state.await_count, 2)
                self.assertFalse(coordinator.command_lock.locked())

    async def test_all_controls_recheck_owner_and_capability_after_writing(
        self,
    ) -> None:
        for operation in ("mode", "number", "preset"):
            for change in ("owner", "capability", "backend", "proxy"):
                with self.subTest(operation=operation, change=change):
                    (
                        coordinator,
                        client,
                        selected,
                        _,
                        _,
                        submit,
                    ) = await self.control_case(operation)
                    changed = {
                        "owner": replace(selected, owner="mqtt"),
                        "capability": replace(selected, commands=frozenset()),
                        "backend": replace(
                            selected,
                            backend="legacy_ble"
                            if selected.backend == "quick_connect"
                            else "quick_connect",
                        ),
                        "proxy": replace(
                            selected, proxy_id="650e8400-e29b-41d4-a716-446655440000"
                        ),
                    }[change]
                    client.fetch_devices.side_effect = [[selected], [changed]]
                    with self.assertRaises(ApiError):
                        await submit()
                    client.set_control.assert_awaited_once()
                    self.assertFalse(coordinator.command_lock.locked())

    async def test_all_controls_reject_owner_and_capability_loss_before_writing(
        self,
    ) -> None:
        for operation in ("mode", "number", "preset"):
            for change in ("owner", "capability"):
                with self.subTest(operation=operation, change=change):
                    _, client, selected, _, _, submit = await self.control_case(
                        operation
                    )
                    client.fetch_devices.return_value = [
                        replace(selected, owner="mqtt")
                        if change == "owner"
                        else replace(selected, commands=frozenset())
                    ]
                    with self.assertRaises(ApiError):
                        await submit()
                    client.set_control.assert_not_called()

    async def test_all_controls_propagate_cancellation_without_replaying(self) -> None:
        for operation in ("mode", "number", "preset"):
            for phase in ("submit", "readback"):
                with self.subTest(operation=operation, phase=phase):
                    coordinator, client, _, old, _, submit = await self.control_case(
                        operation
                    )

                    async def cancel_current(*_):
                        asyncio.current_task().cancel()
                        await asyncio.sleep(0)

                    if phase == "submit":
                        client.set_control.side_effect = cancel_current
                    else:
                        calls = 0

                        async def readback(
                            _device_id, *, initial=old, cancel=cancel_current
                        ):
                            nonlocal calls
                            calls += 1
                            if calls == 1:
                                return initial
                            await cancel()

                        client.fetch_state.side_effect = readback
                    with self.assertRaises(asyncio.CancelledError):
                        await asyncio.create_task(submit())
                    client.set_control.assert_awaited_once()
                    self.assertEqual(
                        client.fetch_state.await_count, 1 if phase == "submit" else 2
                    )
                    self.assertFalse(coordinator.command_lock.locked())

    async def test_preset_requires_matching_readback(self) -> None:
        _, client, _, old, new, submit = await self.control_case("preset")
        client.fetch_state.side_effect = [old, old]
        with self.assertRaisesRegex(ApiError, "no matching current preset"):
            await submit()
        client.set_control.assert_awaited_once()
        client.set_control.reset_mock()
        client.fetch_state.side_effect = [old, new]
        await submit()
        client.set_control.assert_awaited_once()

    async def test_state_backend_must_match_selected_inventory(self) -> None:
        from homeassistant.helpers.update_coordinator import UpdateFailed

        coordinator, client, _, old, _, submit = await self.control_case("number")
        client.fetch_state.side_effect = None
        client.fetch_state.return_value = replace(old, backend="quick_connect")
        with self.assertRaisesRegex(UpdateFailed, "different backend"):
            await coordinator._async_update_data()
        with self.assertRaises(ApiError):
            await submit()
        client.set_control.assert_not_called()

    async def test_preset_confirmation_requires_mode_and_clear_fan_flag(self) -> None:
        for preset, fields, mode, flag, matches in (
            (
                "automatic105_f30_percent",
                {
                    "automatic_temperature_threshold_f": 105,
                    "automatic_humidity_threshold_percent": 30,
                },
                "timer",
                False,
                False,
            ),
            (
                "automatic105_f30_percent",
                {
                    "automatic_temperature_threshold_f": 105,
                    "automatic_humidity_threshold_percent": 30,
                },
                "automatic",
                False,
                True,
            ),
            (
                "timer_one_minute",
                {"timer_original_minutes": 1, "timer_remaining_minutes": 1},
                "automatic",
                False,
                False,
            ),
            (
                "timer_one_minute",
                {"timer_original_minutes": 1, "timer_remaining_minutes": 1},
                "timer",
                True,
                True,
            ),
            (
                "timer_clear",
                {"timer_original_minutes": 0, "timer_remaining_minutes": 0},
                "timer",
                True,
                False,
            ),
            (
                "timer_clear",
                {"timer_original_minutes": 0, "timer_remaining_minutes": 0},
                "timer",
                None,
                False,
            ),
            (
                "timer_clear",
                {"timer_original_minutes": 0, "timer_remaining_minutes": 0},
                "timer",
                False,
                True,
            ),
        ):
            with self.subTest(preset=preset, mode=mode, flag=flag):
                coordinator, client, _, old, _, _ = await self.control_case("preset")
                client.fetch_state.side_effect = [
                    old,
                    replace(
                        old,
                        state=Readings(mode=mode, controller_fan_flag=flag, **fields),
                    ),
                ]
                if matches:
                    await coordinator.async_set_preset(preset)
                else:
                    with self.assertRaisesRegex(ApiError, "no matching current preset"):
                        await coordinator.async_set_preset(preset)
                client.set_control.assert_awaited_once_with("configured", preset)

    async def asyncSetUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.hass = HomeAssistant(self.directory.name)
        self.hass.config_entries = ConfigEntries(self.hass, {})
        await ir.async_load(self.hass)
        dr.async_setup(self.hass)
        await dr.async_load(self.hass)
        await er.async_load(self.hass)
        self.devices = dr.async_get(self.hass)
        self.entities = er.async_get(self.hass)

    async def asyncTearDown(self) -> None:
        await self.hass.async_stop()
        self.directory.cleanup()

    async def test_refresh_button_reads_unavailable_device_and_applies_returned_state(
        self,
    ) -> None:
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        client.refresh.return_value = state_data(
            available=True, freshness="fresh", state=Readings(temperature_f=100)
        )
        client.fetch_state.return_value = client.refresh.return_value
        coordinator = GafctlCoordinator(self.hass, client, device(), entry)
        coordinator.async_set_updated_data(state_data(available=False, state=None))
        button = GafctlRefreshButton(coordinator, entry)
        self.assertTrue(button.available)
        await button.async_press()
        client.refresh.assert_awaited_once_with("configured", "legacy_ble")
        client.fetch_state.assert_awaited_once_with("configured")
        self.assertEqual(coordinator.data.state.temperature_f, 100)

    async def test_refresh_failure_preserves_current_data_and_reports_failure(
        self,
    ) -> None:
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        client.refresh.side_effect = ApiError("device refresh did not complete")
        coordinator = GafctlCoordinator(self.hass, client, device(), entry)
        old = state_data(available=True, state=Readings(temperature_f=99))
        coordinator.async_set_updated_data(old)
        with self.assertRaises(HomeAssistantError):
            await GafctlRefreshButton(coordinator, entry).async_press()
        self.assertEqual(coordinator.data, old)
        client.refresh.assert_awaited_once()

    async def test_refresh_rechecks_proxy_identity_and_http_ownership(self) -> None:
        entry = await self.entry()
        for current in (
            device(owner="mqtt"),
            device("650e8400-e29b-41d4-a716-446655440000"),
        ):
            client = AsyncMock()
            client.fetch_devices.return_value = [current]
            coordinator = GafctlCoordinator(self.hass, client, device(), entry)
            with self.assertRaises(HomeAssistantError):
                await GafctlRefreshButton(coordinator, entry).async_press()
            client.refresh.assert_not_called()

    async def test_refresh_reports_failure_if_followup_cannot_get_current_readings(
        self,
    ) -> None:
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        client.refresh.return_value = state_data(
            available=True, state=Readings(temperature_f=100)
        )
        client.fetch_state.return_value = state_data(available=False, state=None)
        coordinator = GafctlCoordinator(self.hass, client, device(), entry)
        with self.assertRaises(HomeAssistantError):
            await GafctlRefreshButton(coordinator, entry).async_press()

    async def test_refresh_completion_rejects_owner_or_proxy_changed_during_read(
        self,
    ) -> None:
        entry = await self.entry()
        for changed in (
            device(owner="mqtt"),
            device("650e8400-e29b-41d4-a716-446655440000"),
        ):
            client = AsyncMock()
            client.fetch_devices.side_effect = [[device()], [changed]]
            client.refresh.return_value = state_data(
                available=True, state=Readings(temperature_f=99)
            )
            coordinator = GafctlCoordinator(self.hass, client, device(), entry)
            old = state_data(available=False, state=None)
            coordinator.async_set_updated_data(old)
            with self.assertRaises(HomeAssistantError):
                await GafctlRefreshButton(coordinator, entry).async_press()
            self.assertEqual(coordinator.data, old)

    async def test_late_refresh_response_does_not_replace_newer_periodic_data(
        self,
    ) -> None:
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        newer = state_data(
            available=True, freshness="fresh", state=Readings(temperature_f=110)
        )
        older = state_data(
            available=True, freshness="fresh", state=Readings(temperature_f=100)
        )
        coordinator = GafctlCoordinator(self.hass, client, device(), entry)

        async def delayed_response(*_):
            coordinator.async_set_updated_data(newer)
            return older

        client.refresh.side_effect = delayed_response
        client.fetch_state.return_value = newer
        await GafctlRefreshButton(coordinator, entry).async_press()
        self.assertEqual(coordinator.data, newer)

    async def entry(self, proxy_id=PROXY_ID, domain="gafctl"):
        entry = ConfigEntry(
            version=2,
            minor_version=1,
            domain=domain,
            title="Vent",
            source="user",
            data={
                "api_url": "http://127.0.0.1:8787",
                "device_id": "configured",
                "proxy_id": proxy_id,
                "backend": "legacy_ble",
            },
            options={},
            unique_id=f"gafctl_{proxy_id}_configured",
            discovery_keys=MappingProxyType({}),
            subentries_data=[],
        )
        with patch.object(ConfigEntries, "async_setup", AsyncMock(return_value=True)):
            await self.hass.config_entries.async_add(entry)
        return entry

    def registered_sensor(self, entry, key="temperature"):
        registered_device = self.devices.async_get_or_create(
            config_entry_id=entry.entry_id,
            identifiers={(entry.domain, entry.unique_id)},
            name="Vent",
        )
        entity = self.entities.async_get_or_create(
            "sensor",
            entry.domain,
            f"{entry.unique_id}_{key}",
            config_entry=entry,
            device_id=registered_device.id,
        )
        return (entity, registered_device)

    async def test_mqtt_handoff_removes_http_entities_and_empty_device(self) -> None:
        entry = await self.entry()
        sensor, old_device = self.registered_sensor(entry)
        coordinator = GafctlCoordinator(self.hass, AsyncMock(), device(), entry)
        coordinator.device = device(owner="mqtt")
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNone(self.entities.async_get(sensor.entity_id))
        self.assertIsNone(self.devices.async_get(old_device.id))

    async def test_handoff_preserves_another_proxy_and_mqtt_registry(self) -> None:
        entry = await self.entry()
        other = await self.entry("650e8400-e29b-41d4-a716-446655440000")
        mqtt = await self.entry(domain="mqtt")
        old_sensor, old_device = self.registered_sensor(entry)
        other_sensor, other_device = self.registered_sensor(other)
        mqtt_sensor, mqtt_device = self.registered_sensor(mqtt)
        coordinator = GafctlCoordinator(self.hass, AsyncMock(), device(), entry)
        coordinator.device = device(owner="mqtt")
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNone(self.entities.async_get(old_sensor.entity_id))
        self.assertIsNone(self.devices.async_get(old_device.id))
        self.assertIsNotNone(self.entities.async_get(other_sensor.entity_id))
        self.assertIsNotNone(self.devices.async_get(other_device.id))
        self.assertIsNotNone(self.entities.async_get(mqtt_sensor.entity_id))
        self.assertIsNotNone(self.devices.async_get(mqtt_device.id))

    async def test_http_return_preserves_current_entities_and_prunes_removed_keys(
        self,
    ) -> None:
        entry = await self.entry()
        sensor, registered = self.registered_sensor(entry)
        obsolete, _ = self.registered_sensor(entry, "obsolete")
        coordinator = GafctlCoordinator(
            self.hass, AsyncMock(), device(owner="mqtt"), entry
        )
        coordinator.device = device()
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNotNone(self.entities.async_get(sensor.entity_id))
        self.assertIsNone(self.entities.async_get(obsolete.entity_id))
        self.assertIsNotNone(self.devices.async_get(registered.id))

    async def test_proxy_identity_change_cannot_supply_readings_for_old_device(
        self,
    ) -> None:
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [
            device("650e8400-e29b-41d4-a716-446655440000")
        ]
        coordinator = GafctlCoordinator(self.hass, client, device(), entry)
        from homeassistant.helpers.update_coordinator import UpdateFailed

        with self.assertRaises(UpdateFailed):
            await coordinator._async_update_data()
        client.fetch_state.assert_not_called()

    async def test_failed_inventory_cannot_bypass_identity_and_ownership_validation(
        self,
    ) -> None:
        entry = await self.entry()
        client = AsyncMock()
        from homeassistant.helpers.update_coordinator import UpdateFailed

        from custom_components.gafctl.client import (
            ApiError,
            Readings,
        )

        client.fetch_devices.side_effect = ApiError("inventory unavailable")
        client.fetch_state.return_value = state_data(
            available=True, state=Readings(temperature_f=100)
        )
        coordinator = GafctlCoordinator(self.hass, client, device(), entry)
        with self.assertRaises(UpdateFailed):
            await coordinator._async_update_data()
        client.fetch_state.assert_not_called()

    async def test_generated_mqtt_templates_accept_nullable_payloads(self) -> None:
        fixture = os.environ.get("GAFCTL_DISCOVERY_FIXTURE")
        if not fixture:
            self.skipTest("set GAFCTL_DISCOVERY_FIXTURE to generated discovery configs")
        configs = await asyncio.to_thread(read_discovery_fixture, fixture)
        normal = {
            "available": True,
            "inventory_status": "present",
            "last_error": None,
            "state": {
                "temperature_f": 98.6,
                "humidity_percent": 42.1,
                "diagnostics": {"firmware_version": "3.0.0"},
                "settings": {
                    "automatic_temperature_tenths_f": 1050,
                    "automatic_humidity_tenths_percent": 300,
                    "mode": "automatic",
                    "timer_remaining_minutes": 0,
                    "timer_original_minutes": 0,
                    "controller_fan_on": False,
                },
            },
        }
        initial = {
            "available": False,
            "inventory_status": "unknown",
            "state": None,
            "last_error": None,
        }
        expired = initial | {
            "inventory_status": "present",
            "last_error": "device state expired",
        }
        partial = normal | {
            "state": {
                "temperature_f": None,
                "humidity_percent": None,
                "diagnostics": None,
                "settings": {
                    "automatic_temperature_tenths_f": None,
                    "automatic_humidity_tenths_percent": None,
                },
            }
        }
        for topic, config in configs:
            schemas = {
                "sensor": mqtt_sensor,
                "select": mqtt_select,
                "number": mqtt_number,
                "binary_sensor": mqtt_binary,
                "button": mqtt_button,
                "switch": mqtt_switch,
            }
            schemas[topic.split("/")[1]].DISCOVERY_SCHEMA(config)
            template = config.get("value_template")
            if not template or topic.endswith("_control_result/config"):
                continue
            rendered = [
                Template(template, self.hass).async_render({"value_json": payload})
                for payload in (initial, expired, partial, normal)
            ]
            if topic.endswith("_automatic_temperature_threshold/config"):
                self.assertEqual(rendered, [None, None, None, 105.0])
            if topic.endswith("_freshness/config"):
                self.assertEqual(rendered, ["unknown", "stale", "fresh", "fresh"])

    def render_command(self, template: str, value: object) -> dict[str, object]:
        return json.loads(
            Template(template, self.hass).async_render(
                {"value": value}, parse_result=False
            )
        )

    def render_settings(
        self, config: dict[str, object], settings: dict[str, object]
    ) -> object:
        return Template(config["value_template"], self.hass).async_render(
            {"value_json": {"state": {"settings": settings}}}
        )

    async def test_generated_mqtt_commands_preserve_types_and_switch_conditions(
        self,
    ) -> None:
        fixture = os.environ.get("GAFCTL_DISCOVERY_FIXTURE")
        if not fixture:
            self.skipTest("set GAFCTL_DISCOVERY_FIXTURE to generated discovery configs")
        for topic, config in await asyncio.to_thread(read_discovery_fixture, fixture):
            template = config.get("command_template")
            if not template:
                continue
            render = partial(self.render_command, template)
            domain = topic.split("/")[1]
            if domain == "number":
                valid = render(config["min"])
                self.assertTrue(valid["request_id"])
                self.assertGreater(valid["issued_at_unix_ms"], 0)
                value_field = next(key for key in valid["command"] if key != "kind")
                self.assertEqual(valid["command"][value_field], config["min"])
                for value in (True, "garbage", 90.5, None):
                    self.assertIsNone(render(value)["command"][value_field])
            elif domain == "switch":
                mode = config["unique_id"].rsplit("_", 2)[-2]
                self.assertEqual(
                    render("ON")["command"],
                    {"kind": "quick_connect_mode", "mode": mode},
                )
                self.assertEqual(
                    render("OFF")["command"],
                    {"kind": "quick_connect_conditional_off", "only_if_current": mode},
                )
                self.assertIsNone(render("invalid")["command"])
            elif topic.endswith("_refresh/config"):
                self.assertEqual(
                    set(render("PRESS")), {"request_id", "issued_at_unix_ms"}
                )
                self.assertEqual(len(config["availability"]), 1)

    async def test_generated_mqtt_binary_modes_and_presets_preserve_unknown(
        self,
    ) -> None:
        fixture = os.environ.get("GAFCTL_DISCOVERY_FIXTURE")
        if not fixture:
            self.skipTest("set GAFCTL_DISCOVERY_FIXTURE to generated discovery configs")
        for topic, config in await asyncio.to_thread(read_discovery_fixture, fixture):
            render = partial(self.render_settings, config)
            if "/binary_sensor/" in topic and topic.endswith("_mode/config"):
                mode = config["unique_id"].rsplit("_", 2)[-2]
                self.assertEqual(render({"mode": mode}), "ON")
                self.assertEqual(render({"mode": "off"}), "OFF")
                self.assertIsNone(render({"mode": "conflicting"}))
                self.assertIsNone(render({}))
            elif topic.endswith("_automatic_thresholds/config"):
                self.assertEqual(
                    render(
                        {
                            "mode": "timer",
                            "automatic_temperature_tenths_f": 1050,
                            "automatic_humidity_tenths_percent": 300,
                        }
                    ),
                    "automatic105_f30_percent",
                )
            elif "/select/" in topic and topic.endswith("_timer/config"):
                self.assertEqual(
                    render(
                        {
                            "mode": "automatic",
                            "timer_remaining_minutes": 0,
                            "timer_original_minutes": 0,
                        }
                    ),
                    "timer_clear",
                )
                self.assertIsNone(
                    render({"timer_remaining_minutes": 0, "timer_original_minutes": 1})
                )


if __name__ == "__main__":
    unittest.main()
