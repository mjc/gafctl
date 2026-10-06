"""Registry tests run with the installed Home Assistant Python environment."""

import asyncio
import json
import logging
import os
import shutil
import sys
import tempfile
import unittest
from contextlib import asynccontextmanager, contextmanager
from datetime import timedelta
from functools import partial
from pathlib import Path
from types import MappingProxyType
from unittest.mock import AsyncMock, MagicMock, patch
from uuid import uuid4

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ha_fixtures import (
    PROXY_ID,
    changed_device,
    device,
    diagnostics,
    legacy_settings,
    quickconnect_settings,
    readings,
    reported_state,
    state_data,
)
from homeassistant import loader
from homeassistant.components.mqtt import binary_sensor as mqtt_binary
from homeassistant.components.mqtt import button as mqtt_button
from homeassistant.components.mqtt import discovery as mqtt_discovery
from homeassistant.components.mqtt import number as mqtt_number
from homeassistant.components.mqtt import select as mqtt_select
from homeassistant.components.mqtt import sensor as mqtt_sensor
from homeassistant.components.mqtt import switch as mqtt_switch
from homeassistant.components.mqtt.discovery import (
    MQTTDiscoveryPayload,
    _merge_common_device_options,
)
from homeassistant.components.mqtt.models import DATA_MQTT, MqttData, ReceiveMessage
from homeassistant.components.mqtt.schemas import DEVICE_DISCOVERY_SCHEMA
from homeassistant.components.sensor import SensorDeviceClass, SensorStateClass
from homeassistant.config_entries import ConfigEntries, ConfigEntry, ConfigEntryState
from homeassistant.const import (
    PERCENTAGE,
    EntityCategory,
    UnitOfTemperature,
    UnitOfTime,
)
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryNotReady, HomeAssistantError
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.entity_platform import EntityPlatform
from homeassistant.helpers.entity_registry import RegistryEntryDisabler
from homeassistant.helpers.template import Template
from homeassistant.setup import async_setup_component

from custom_components import gafctl as gafctl_integration
from custom_components.gafctl import GafctlCoordinator
from custom_components.gafctl import binary_sensor as gafctl_binary
from custom_components.gafctl import button as gafctl_button
from custom_components.gafctl import number as gafctl_number
from custom_components.gafctl import select as gafctl_select
from custom_components.gafctl import sensor as gafctl_sensor
from custom_components.gafctl import switch as gafctl_switch
from custom_components.gafctl.config_flow import GafctlConfigFlow
from custom_components.gafctl.controls import NUMBER_CONTROLS, entity_keys
from custom_components.gafctl.models import ApiError, ControlOutcomeUnknown

COMPONENT_DIR = Path(__file__).resolve().parents[1] / "custom_components/gafctl"
REGISTRY_FIXTURE_DIR = COMPONENT_DIR.parents[1] / "target/ha-registry-tests"


@contextmanager
def api_client(client, module="custom_components.gafctl.config_flow"):
    with (
        patch(f"{module}.async_get_clientsession", return_value=object()),
        patch(f"{module}.ApiClient", return_value=client),
    ):
        yield


def fake_client(*, devices=None, state=None, states=None):
    client = AsyncMock()
    client.fetch_devices.return_value = [device()] if devices is None else devices
    client.fetch_state.return_value = state
    client.fetch_state.side_effect = states
    return client


def coordinator_for(hass, client, selected, entry):
    hass.config_entries.async_update_entry(
        entry, data=entry.data | {"backend": selected["backend"]}
    )
    coordinator = GafctlCoordinator(hass, client, entry)
    coordinator.device = selected
    entry.runtime_data = coordinator
    coordinator.loaded_entity_keys = gafctl_integration.entity_keys(selected)
    return coordinator


def read_discovery_fixture(path: str) -> list[tuple[str, dict[str, object]]]:
    return json.loads(Path(path).read_text())


class RegistryTests(unittest.IsolatedAsyncioTestCase):
    async def native_service_case(self, operation):
        component = self.hass.config.path("custom_components/gafctl")
        if not await asyncio.to_thread(Path(component).exists):
            await asyncio.to_thread(shutil.copytree, COMPONENT_DIR, component)
            loader.async_setup(self.hass)
        entry = await self.entry(proxy_id=str(uuid4()))
        backend = "quick_connect" if operation == "mode" else "legacy_ble"
        selected = device(
            entry.data["proxy_id"],
            backend=backend,
            commands=["quick_connect_mode"]
            if operation == "mode"
            else ["legacy_preset", "legacy_automatic_temperature"],
        )
        self.hass.config_entries.async_update_entry(
            entry, data=entry.data | {"backend": backend}
        )
        state = state_data(backend=backend, state=reported_state(backend))
        client = fake_client(devices=[selected], state=state)
        with api_client(client, "custom_components.gafctl"):
            self.assertTrue(await async_setup_component(self.hass, "gafctl", {}))
            if entry.state is not ConfigEntryState.LOADED:
                self.assertTrue(
                    await self.hass.config_entries.async_setup(entry.entry_id)
                )
            await self.hass.async_block_till_done()
        domain, service, key, fields = {
            "preset": (
                "select",
                "select_option",
                "automatic_thresholds",
                {"option": "105.0°F / 30.0%"},
            ),
            "mode": ("select", "select_option", "mode", {"option": "Automatic"}),
            "number": ("number", "set_value", "automatic_temperature", {"value": 105}),
        }[operation]
        entity_id = self.entities.async_get_entity_id(
            domain, "gafctl", f"{entry.unique_id}_{key}"
        )
        self.assertIsNotNone(entity_id)
        call = partial(
            self.hass.services.async_call,
            domain,
            service,
            {"entity_id": entity_id} | fields,
            blocking=True,
        )
        return entry, client, entry.runtime_data, call

    @asynccontextmanager
    async def invalidate_entry(self, entry, client, lifecycle):
        if lifecycle == "replaced":
            coordinator_for(
                self.hass, client, client.fetch_devices.return_value[0], entry
            )
        elif lifecycle == "unloaded":
            self.assertTrue(await self.hass.config_entries.async_unload(entry.entry_id))
        else:
            entered, release = asyncio.Event(), asyncio.Event()
            original_unload = ConfigEntries.async_unload_platforms

            async def delayed_unload(manager, target, platforms):
                entered.set()
                await release.wait()
                return await original_unload(manager, target, platforms)

            with patch.object(
                ConfigEntries, "async_unload_platforms", new=delayed_unload
            ):
                task = asyncio.create_task(
                    self.hass.config_entries.async_unload(entry.entry_id)
                )
                try:
                    await asyncio.wait_for(entered.wait(), 2)
                    self.assertIs(entry.state, ConfigEntryState.UNLOAD_IN_PROGRESS)
                    yield
                finally:
                    release.set()
                    self.assertTrue(await asyncio.wait_for(task, 2))
            return
        try:
            yield
        finally:
            if lifecycle == "replaced":
                self.assertTrue(
                    await self.hass.config_entries.async_unload(entry.entry_id)
                )

    async def test_queued_native_services_reject_unloaded_or_replaced_coordinator(self):
        for operation in ("preset", "mode", "number"):
            for lifecycle in ("unloaded", "replaced", "unloading"):
                with self.subTest(operation=operation, lifecycle=lifecycle):
                    entry, client, coordinator, call = await self.native_service_case(
                        operation
                    )
                    lock = coordinator.command_lock
                    await lock.acquire()
                    queued = asyncio.Event()
                    acquire = lock.acquire

                    async def signal_acquire(queued=queued, acquire=acquire):
                        queued.set()
                        return await acquire()

                    with patch.object(lock, "acquire", side_effect=signal_acquire):
                        task = asyncio.create_task(call())
                        held = True
                        try:
                            await asyncio.wait_for(queued.wait(), 2)
                            client.fetch_devices.reset_mock()
                            client.fetch_state.reset_mock()
                            async with self.invalidate_entry(entry, client, lifecycle):
                                lock.release()
                                held = False
                                with self.assertRaisesRegex(
                                    HomeAssistantError, "no longer active"
                                ):
                                    await asyncio.wait_for(task, 2)
                                client.fetch_devices.assert_not_awaited()
                                client.fetch_state.assert_not_awaited()
                                client.set_control.assert_not_awaited()
                                self.assertFalse(lock.locked())
                        finally:
                            if held:
                                lock.release()
                            await asyncio.gather(task, return_exceptions=True)

    async def test_native_services_recheck_lifecycle_after_preparatory_read(self):
        for operation in ("preset", "mode", "number"):
            for lifecycle in ("unloaded", "replaced", "unloading"):
                with self.subTest(operation=operation, lifecycle=lifecycle):
                    entry, client, coordinator, call = await self.native_service_case(
                        operation
                    )
                    entered, release = asyncio.Event(), asyncio.Event()
                    read = client.fetch_state

                    async def delayed_read(
                        *_, entered=entered, release=release, read=read
                    ):
                        entered.set()
                        await release.wait()
                        return read.return_value

                    read.side_effect = delayed_read
                    task = asyncio.create_task(call())
                    try:
                        await asyncio.wait_for(entered.wait(), 2)
                        async with self.invalidate_entry(entry, client, lifecycle):
                            release.set()
                            with self.assertRaisesRegex(
                                HomeAssistantError, "no longer active"
                            ):
                                await asyncio.wait_for(task, 2)
                            client.set_control.assert_not_awaited()
                            self.assertFalse(coordinator.command_lock.locked())
                    finally:
                        release.set()
                        await asyncio.gather(task, return_exceptions=True)

    async def test_failed_native_unload_keeps_retained_services_usable(self):
        for operation in ("preset", "mode", "number"):
            with self.subTest(operation=operation):
                entry, client, coordinator, call = await self.native_service_case(
                    operation
                )
                with patch.object(
                    ConfigEntries,
                    "async_unload_platforms",
                    AsyncMock(return_value=False),
                ):
                    self.assertFalse(
                        await self.hass.config_entries.async_unload(entry.entry_id)
                    )
                self.assertIs(entry.state, ConfigEntryState.FAILED_UNLOAD)
                self.assertIs(entry.runtime_data, coordinator)
                await call()
                client.set_control.assert_awaited_once()
                # Restore retained platforms for fixture cleanup; HA does not
                # automatically recover a FAILED_UNLOAD entry.
                entry._async_set_state(self.hass, ConfigEntryState.LOADED, None)
                self.assertTrue(
                    await self.hass.config_entries.async_unload(entry.entry_id)
                )

    async def test_native_unload_does_not_cancel_or_replay_submitted_requests(self):
        for operation in ("preset", "mode", "number"):
            with self.subTest(operation=operation):
                entry, client, coordinator, call = await self.native_service_case(
                    operation
                )
                entered, release = asyncio.Event(), asyncio.Event()
                submitted = client.set_control
                uncertain = ControlOutcomeUnknown(f"submitted-{operation}")

                async def delayed_submission(
                    *_,
                    entered=entered,
                    release=release,
                    uncertain=uncertain,
                    operation=operation,
                ):
                    entered.set()
                    await release.wait()
                    raise uncertain

                submitted.side_effect = delayed_submission
                task = asyncio.create_task(call())
                try:
                    await asyncio.wait_for(entered.wait(), 2)
                    self.assertTrue(
                        await self.hass.config_entries.async_unload(entry.entry_id)
                    )
                    self.assertFalse(task.done())
                    release.set()
                    message = f"submitted-{operation}"
                    with self.assertRaisesRegex(HomeAssistantError, message):
                        await asyncio.wait_for(task, 2)
                    submitted.assert_awaited_once()
                    self.assertFalse(coordinator.command_lock.locked())
                finally:
                    release.set()
                    await asyncio.gather(task, return_exceptions=True)

    async def test_empty_entry_keeps_polling_and_restores_http_entities(self) -> None:
        for selected in (device(owner="mqtt"), device(commands=[], read_state=False)):
            with self.subTest(selected=selected):
                entry = await self.entry()
                entry._async_set_state(
                    self.hass, ConfigEntryState.SETUP_IN_PROGRESS, None
                )
                client = fake_client(
                    devices=[selected], state=state_data(available=False)
                )
                entities = []

                async def forward(entry, platforms, entities=entities):
                    for platform in platforms:
                        module = __import__(
                            f"custom_components.gafctl.{platform}",
                            fromlist=["async_setup_entry"],
                        )
                        await module.async_setup_entry(
                            self.hass, entry, entities.extend
                        )

                reloaded = asyncio.Event()

                async def reload(
                    entry_id, entry=entry, forward=forward, reloaded=reloaded
                ):
                    self.assertEqual(entry_id, entry.entry_id)
                    await forward(entry, gafctl_integration.PLATFORMS)
                    reloaded.set()
                    return True

                with (
                    api_client(client, "custom_components.gafctl"),
                    patch(
                        "custom_components.gafctl.coordinator.UPDATE_INTERVAL",
                        timedelta(seconds=1),
                    ),
                    patch.object(
                        ConfigEntries, "async_forward_entry_setups", side_effect=forward
                    ),
                    patch.object(ConfigEntries, "async_reload", side_effect=reload),
                ):
                    await gafctl_integration.async_setup_entry(self.hass, entry)
                    self.assertEqual(entities, [])
                    client.fetch_devices.return_value = [device()]
                    await asyncio.wait_for(reloaded.wait(), timeout=3)
                coordinator = entry.runtime_data
                self.assertGreaterEqual(client.fetch_devices.await_count, 2)
                self.assertEqual(coordinator.device["state_source"], "http")
                self.assertTrue(gafctl_integration.entity_keys(coordinator.device))
                self.assertTrue(entities)
                with patch.object(
                    ConfigEntries,
                    "async_unload_platforms",
                    AsyncMock(return_value=True),
                ):
                    self.assertTrue(
                        await gafctl_integration.async_unload_entry(self.hass, entry)
                    )
                await entry._async_process_on_unload(self.hass)
                self.assertEqual(coordinator._listeners, {})
                self.assertIsNone(coordinator._unsub_refresh)
                entry._async_set_state(self.hass, ConfigEntryState.NOT_LOADED, None)

    async def test_setup_resolves_inventory_once_and_cleans_obsolete_entities(
        self,
    ) -> None:
        entry = await self.entry()
        entry._async_set_state(self.hass, ConfigEntryState.SETUP_IN_PROGRESS, None)
        sensor, _ = self.registered_sensor(entry)
        obsolete, _ = self.registered_sensor(entry, "obsolete")
        registered_device = self.devices.async_get_or_create(
            config_entry_id=entry.entry_id,
            identifiers={(entry.domain, entry.unique_id)},
            name="Vent",
        )
        refresh_button = self.entities.async_get_or_create(
            "button",
            entry.domain,
            f"{entry.unique_id}_refresh",
            config_entry=entry,
            device_id=registered_device.id,
        )
        client = fake_client(
            devices=[device()], state=state_data(state=readings(temperature_f=99))
        )
        with (
            api_client(client, "custom_components.gafctl"),
            patch.object(
                ConfigEntries, "async_forward_entry_setups", AsyncMock()
            ) as forward,
        ):
            self.assertTrue(
                await gafctl_integration.async_setup_entry(self.hass, entry)
            )
        entry._async_set_state(self.hass, ConfigEntryState.NOT_LOADED, None)
        client.fetch_devices.assert_awaited_once()
        client.fetch_state.assert_awaited_once_with("configured")
        forward.assert_awaited_once()
        self.assertEqual(entry.runtime_data.data["state"]["temperature_f"], 99)
        self.assertTrue(entry.runtime_data._entities_loaded)
        self.assertIsNotNone(self.entities.async_get(sensor.entity_id))
        self.assertIsNone(self.entities.async_get(obsolete.entity_id))
        self.assertIsNone(self.entities.async_get(refresh_button.entity_id))

    async def test_setup_failure_preserves_registry_until_matching_device_is_resolved(
        self,
    ) -> None:
        entry = await self.entry()
        for failure in ("network", "absent", "backend", "proxy", "owner"):
            entry._async_set_state(self.hass, ConfigEntryState.SETUP_IN_PROGRESS, None)
            registered, _ = self.registered_sensor(entry)
            registered_device_id = registered.device_id
            registered = self.entities.async_update_entity(
                registered.entity_id,
                new_entity_id="sensor.custom_vent_temperature",
                name="Custom vent temperature",
                disabled_by=RegistryEntryDisabler.USER,
            )
            client = AsyncMock()
            if failure == "network":
                client.fetch_devices.side_effect = ApiError("inventory unavailable")
            else:
                client.fetch_devices.return_value = {
                    "absent": [],
                    "backend": [device(backend="quick_connect")],
                    "proxy": [device(proxy_id="another-proxy")],
                    "owner": [device(owner="mqtt")],
                }[failure]
            client.fetch_state.return_value = state_data(available=False)
            with (
                self.subTest(failure=failure),
                api_client(client, "custom_components.gafctl"),
                patch.object(
                    ConfigEntries, "async_forward_entry_setups", AsyncMock()
                ) as forward,
            ):
                if failure == "owner":
                    self.assertTrue(
                        await gafctl_integration.async_setup_entry(self.hass, entry)
                    )
                else:
                    with self.assertRaises(ConfigEntryNotReady):
                        await gafctl_integration.async_setup_entry(self.hass, entry)
                    forward.assert_not_awaited()
            entry._async_set_state(self.hass, ConfigEntryState.NOT_LOADED, None)
            client.fetch_devices.assert_awaited_once()
            restored = self.entities.async_get(registered.entity_id)
            if failure == "owner":
                self.assertIsNone(restored)
            else:
                self.assertIsNotNone(
                    restored, f"registry lost during {failure} recovery"
                )
                self.assertEqual(restored.name, "Custom vent temperature")
                self.assertEqual(restored.disabled_by, RegistryEntryDisabler.USER)
                self.assertEqual(
                    self.devices.async_get(registered_device_id).id,
                    registered_device_id,
                )
            if failure != "owner":
                client.fetch_state.assert_not_awaited()
                entry._async_set_state(
                    self.hass, ConfigEntryState.SETUP_IN_PROGRESS, None
                )
                client.fetch_devices.side_effect = None
                client.fetch_devices.return_value = [device()]
                with (
                    api_client(client, "custom_components.gafctl"),
                    patch.object(
                        ConfigEntries, "async_forward_entry_setups", AsyncMock()
                    ),
                ):
                    self.assertTrue(
                        await gafctl_integration.async_setup_entry(self.hass, entry)
                    )
                restored = self.entities.async_get(registered.entity_id)
                self.assertIsNotNone(
                    restored,
                    f"registry lost after {failure}: {registered}; entries="
                    f"{er.async_entries_for_config_entry(self.entities, entry.entry_id)}; "
                    f"identity={entry.unique_id}; expected={entity_keys(device())}",
                )
                self.assertEqual(restored.name, "Custom vent temperature")
                self.assertEqual(restored.disabled_by, RegistryEntryDisabler.USER)
                self.assertEqual(restored.device_id, registered_device_id)
                entry._async_set_state(self.hass, ConfigEntryState.NOT_LOADED, None)

    async def test_coordinator_rejects_invalid_input_without_inventory_or_submission(
        self,
    ) -> None:
        coordinator, client, *_ = await self.control_case("mode")
        operations = [
            partial(coordinator.async_set_mode, "invalid"),
            partial(coordinator.async_set_mode, "manual", only_if_current="automatic"),
            partial(coordinator.async_set_mode, "off", only_if_current="unknown"),
            partial(coordinator.async_set_preset, "invalid"),
            *(
                partial(coordinator.async_set_number, control, True)
                for controls in NUMBER_CONTROLS.values()
                for control in controls
            ),
            partial(
                coordinator.async_set_number, NUMBER_CONTROLS["quick_connect"][2], 45
            ),
        ]
        for operation in operations:
            with self.assertRaises(ApiError):
                await operation()
        client.fetch_devices.assert_not_awaited()
        client.fetch_state.assert_not_awaited()
        client.set_control.assert_not_awaited()

    async def test_onboarding_bulk_selects_only_http_owned_unconfigured_devices(
        self,
    ) -> None:
        await self.entry()
        eligible = [device(id=key) for key in ("one", "two")]
        flow = self.flow({"source": "user"})
        client = fake_client(
            devices=[device(), device(owner="mqtt", id="mqtt"), *eligible]
        )
        with api_client(client):
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
        selected = [device(id=key) for key in ("one", "two")]
        client = fake_client(devices=selected)
        with (
            api_client(client),
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
        flow = self.flow({"source": "user"})
        selected = device()
        flow._api_url = "http://proxy:8787"
        flow._devices = {selected["id"]: selected}
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
        requested = {
            "api_url": "http://proxy:8787",
            "device_id": "configured",
            "proxy_id": PROXY_ID,
            "backend": "legacy_ble",
        }
        for proxy_id in (PROXY_ID, "650e8400-e29b-41d4-a716-446655440000"):
            flow = self.flow({"source": "import"})
            selected = device(proxy_id)
            with patch.object(
                flow, "_fetch_devices", AsyncMock(return_value=[selected])
            ):
                if proxy_id == PROXY_ID:
                    from homeassistant.data_entry_flow import AbortFlow

                    with self.assertRaises(AbortFlow) as raised:
                        await flow.async_step_import(requested)
                    self.assertEqual(raised.exception.reason, "already_configured")
                else:
                    result = await flow.async_step_import(requested)
                    self.assertEqual(result["reason"], "device_unavailable")

    async def test_import_rejects_missing_identity_fields(self) -> None:
        requested = {
            "api_url": "http://proxy:8787",
            "device_id": "configured",
            "proxy_id": PROXY_ID,
            "backend": "legacy_ble",
        }
        for missing in ("proxy_id", "device_id", "backend"):
            with self.subTest(missing=missing):
                flow = self.flow({"source": "import"})
                with patch.object(
                    flow, "_fetch_devices", AsyncMock(return_value=[device()])
                ):
                    result = await flow.async_step_import(
                        {
                            key: value
                            for key, value in requested.items()
                            if key != missing
                        }
                    )
                self.assertEqual(result["reason"], "device_unavailable")

    async def test_mode_entities_preserve_known_and_unknown_projections(self) -> None:
        entry = await self.entry()
        selected = device(backend="quick_connect", commands=["quick_connect_mode"])
        coordinator = coordinator_for(self.hass, AsyncMock(), selected, entry)
        switches = await self.platform_entities(gafctl_switch, coordinator)
        sensors = await self.platform_entities(gafctl_binary, coordinator)
        sensors = [entity for entity in sensors if entity.unique_id.endswith("_mode")]
        self.assertEqual(len(switches), 3)
        self.assertEqual(len(sensors), 3)
        for reported in (
            "automatic",
            "timer",
            "manual",
            "off",
            "unknown",
            "conflicting",
            None,
        ):
            with self.subTest(reported=reported):
                coordinator.async_set_updated_data(
                    state_data(
                        backend="quick_connect",
                        state=readings(settings=quickconnect_settings(mode=reported)),
                    )
                )
                for entity in (*switches, *sensors):
                    expected_mode = entity.unique_id.rsplit("_", 2)[-2]
                    expected = (
                        reported == expected_mode
                        if reported in ("automatic", "timer", "manual", "off")
                        else None
                    )
                    self.assertIs(entity.is_on, expected)

    async def test_reconfigure_preserves_identity_and_rejects_another_proxy(
        self,
    ) -> None:
        entry = await self.entry()
        registered, registered_device = self.registered_sensor(entry)
        for proxy_id, expected in (
            ("650e8400-e29b-41d4-a716-446655440000", "wrong_device"),
            (PROXY_ID, None),
        ):
            flow = self.flow({"source": "reconfigure", "entry_id": entry.entry_id})
            client = fake_client(devices=[device(proxy_id)])
            with (
                api_client(client),
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

    async def test_switches_and_all_off_button_delegate_only_advertised_mode_control(
        self,
    ) -> None:
        entry = await self.entry()
        selected = device(backend="quick_connect", commands=["quick_connect_mode"])
        coordinator = coordinator_for(self.hass, AsyncMock(), selected, entry)
        coordinator.async_set_updated_data(
            state_data(backend="quick_connect", state=reported_state("quick_connect"))
        )
        coordinator.async_set_mode = AsyncMock()
        switches = await self.platform_entities(gafctl_switch, coordinator)
        buttons = await self.platform_entities(gafctl_button, coordinator)
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
        coordinator.device = changed_device(selected, commands=[])
        self.assertFalse(all_off.available)
        self.assertTrue(all(not switch.available for switch in switches))
        self.assertEqual(await self.platform_entities(gafctl_switch, coordinator), [])

    async def test_mode_control_rejects_mismatched_readback_and_unknown_conditional_off(
        self,
    ) -> None:
        entry = await self.entry()
        selected = device(backend="quick_connect", commands=["quick_connect_mode"])
        client = fake_client(devices=[selected])
        old = state_data(backend="quick_connect", state=reported_state("quick_connect"))
        client.fetch_state.return_value = old
        coordinator = coordinator_for(self.hass, client, selected, entry)
        with self.assertRaises(ApiError):
            await coordinator.async_set_mode("manual")
        client.set_control.assert_awaited_once()
        client.set_control.reset_mock()
        client.fetch_state.return_value = old | {
            "state": readings(settings=quickconnect_settings(mode="conflicting"))
        }
        with self.assertRaises(ApiError):
            await coordinator.async_set_mode("off", only_if_current="automatic")
        client.set_control.assert_not_called()

    async def test_mode_controls_recheck_identity_and_confirm_current_mode(
        self,
    ) -> None:
        entry = await self.entry()
        selected = device(backend="quick_connect", commands=["quick_connect_mode"])
        client = fake_client(devices=[selected])
        old = state_data(backend="quick_connect", state=reported_state("quick_connect"))
        new = old | {"state": readings(settings=quickconnect_settings(mode="off"))}
        client.fetch_state.side_effect = [old, new]
        coordinator = coordinator_for(self.hass, client, selected, entry)
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
        client.fetch_devices.return_value = [
            selected | {"state_source": "mqtt", "command_source": "mqtt"}
        ]
        with self.assertRaises(ApiError):
            await coordinator.async_set_mode("manual")
        client.set_control.assert_not_called()

    async def test_ble_timer_can_replace_manual_sentinel_with_bounded_timer(
        self,
    ) -> None:
        entry = await self.entry()
        selected = device(commands=["legacy_timer"])
        old = state_data(
            state=readings(settings=legacy_settings(timer_original_minutes=600))
        )
        new = old | {
            "state": readings(settings=legacy_settings(timer_original_minutes=1))
        }
        client = fake_client(devices=[selected], states=[old, new])
        coordinator = coordinator_for(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        entities = await self.platform_entities(gafctl_number, coordinator)
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
        selected = device(
            commands=[
                "legacy_automatic_temperature",
                "legacy_automatic_humidity",
                "legacy_timer",
            ]
        )
        old = state_data(
            state=readings(
                settings=legacy_settings(
                    automatic_temperature_tenths_f=1051,
                    automatic_humidity_tenths_percent=301,
                    timer_original_minutes=0,
                )
            )
        )
        new = old | {
            "state": old["state"]
            | {
                "settings": old["state"]["settings"]
                | {"automatic_temperature_tenths_f": 1100}
            }
        }
        client = fake_client(devices=[selected], states=[old, new])
        coordinator = coordinator_for(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        entities = await self.platform_entities(gafctl_number, coordinator)
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
        selected = device(backend="quick_connect", commands=["quick_connect_targets"])
        old = state_data(
            backend="quick_connect",
            state=readings(
                settings=quickconnect_settings(
                    automatic_temperature_f=105, automatic_humidity_percent=40
                )
            ),
        )
        new = old | {
            "state": readings(
                settings=quickconnect_settings(
                    automatic_temperature_f=110, automatic_humidity_percent=45
                )
            )
        }
        client = fake_client(devices=[selected], states=[old, new])
        coordinator = coordinator_for(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        numbers = await self.platform_entities(gafctl_number, coordinator)
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
            "legacy_mode": "legacy_mode",
            "number": "legacy_automatic_temperature",
            "preset": "legacy_preset",
        }[operation]
        selected = device(backend=backend, commands=[capability])
        client = fake_client(devices=[selected])
        old = state_data(backend=backend, state=reported_state(backend))
        updated = (
            quickconnect_settings(mode="manual")
            if operation == "mode"
            else legacy_settings(
                mode="timer",
                controller_fan_on=False,
                automatic_temperature_tenths_f=1100,
                timer_original_minutes=1,
                timer_remaining_minutes=1,
            )
        )
        new = old | {"state": readings(settings=updated)}
        client.fetch_state.side_effect = [old, new]
        coordinator = coordinator_for(self.hass, client, selected, entry)
        submit = {
            "mode": partial(coordinator.async_set_mode, "manual"),
            "legacy_mode": partial(coordinator.async_set_mode, "off"),
            "number": partial(
                coordinator.async_set_number, NUMBER_CONTROLS["legacy_ble"][0], 110
            ),
            "preset": partial(coordinator.async_set_preset, "timer_one_minute"),
        }[operation]
        return (coordinator, client, selected, old, new, submit)

    async def test_all_controls_preserve_unknown_request_when_refresh_fails(
        self,
    ) -> None:
        for operation in ("mode", "legacy_mode", "number", "preset"):
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

    async def test_all_controls_recheck_identity_ownership_and_capabilities(self):
        for operation in ("mode", "legacy_mode", "number", "preset"):
            for phase in ("before", "after"):
                changes = (
                    ("owner", "capability")
                    if phase == "before"
                    else ("owner", "capability", "backend", "proxy")
                )
                for change in changes:
                    with self.subTest(operation=operation, phase=phase, change=change):
                        (
                            coordinator,
                            client,
                            selected,
                            _,
                            _,
                            submit,
                        ) = await self.control_case(operation)
                        changed = {
                            "owner": changed_device(selected, owner="mqtt"),
                            "capability": changed_device(selected, commands=[]),
                            "backend": selected
                            | {
                                "backend": "legacy_ble"
                                if selected["backend"] == "quick_connect"
                                else "quick_connect"
                            },
                            "proxy": selected
                            | {"proxy_id": "650e8400-e29b-41d4-a716-446655440000"},
                        }[change]
                        client.fetch_devices.side_effect = (
                            [[changed]]
                            if phase == "before"
                            else [[selected], [changed]]
                        )
                        with self.assertRaises(ApiError):
                            await submit()
                        self.assertEqual(
                            client.set_control.await_count, phase == "after"
                        )
                        self.assertFalse(coordinator.command_lock.locked())

    async def test_all_controls_propagate_cancellation_without_replaying(self) -> None:
        for operation in ("mode", "legacy_mode", "number", "preset"):
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
        client.fetch_state.return_value = old | {"backend": "quick_connect"}
        with self.assertRaisesRegex(UpdateFailed, "different backend"):
            await coordinator._async_update_data()
        with self.assertRaises(ApiError):
            await submit()
        client.set_control.assert_not_called()

    async def test_preset_confirmation_requires_mode_and_clear_fan_flag(self):
        fields = {
            "automatic105_f30_percent": {
                "automatic_temperature_tenths_f": 1050,
                "automatic_humidity_tenths_percent": 300,
            },
            "timer_one_minute": {
                "timer_original_minutes": 1,
                "timer_remaining_minutes": 1,
            },
            "timer_clear": {"timer_original_minutes": 0, "timer_remaining_minutes": 0},
        }
        cases = (
            ("automatic105_f30_percent", "timer", False, False),
            ("automatic105_f30_percent", "automatic", False, True),
            ("timer_one_minute", "automatic", False, False),
            ("timer_one_minute", "timer", True, True),
            ("timer_clear", "timer", True, False),
            ("timer_clear", "timer", None, False),
            ("timer_clear", "timer", False, True),
        )
        for preset, mode, flag, matches in cases:
            with self.subTest(preset=preset, mode=mode, flag=flag):
                coordinator, client, _, old, _, _ = await self.control_case("preset")
                client.fetch_state.side_effect = [
                    old,
                    old
                    | {
                        "state": readings(
                            settings=legacy_settings(
                                mode=mode, controller_fan_on=flag, **fields[preset]
                            )
                        )
                    },
                ]
                if matches:
                    await coordinator.async_set_preset(preset)
                else:
                    with self.assertRaisesRegex(ApiError, "no matching current preset"):
                        await coordinator.async_set_preset(preset)
                client.set_control.assert_awaited_once_with(
                    "configured", {"kind": "legacy_preset", "preset": preset}
                )

    async def asyncSetUp(self) -> None:
        await asyncio.to_thread(REGISTRY_FIXTURE_DIR.mkdir, parents=True, exist_ok=True)
        self.directory = await asyncio.to_thread(
            tempfile.TemporaryDirectory, dir=REGISTRY_FIXTURE_DIR
        )
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

    def flow(self, context):
        flow = GafctlConfigFlow()
        flow.hass = self.hass
        flow.handler = "gafctl"
        flow.context = context
        return flow

    async def platform_entities(self, module, coordinator):
        entities = []
        await module.async_setup_entry(self.hass, coordinator.entry, entities.extend)
        return entities

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

    async def test_queued_reload_is_invalidated_on_unload_or_replacement(self) -> None:
        for replace in (False, True):
            with self.subTest(replace=replace):
                entry = await self.entry()
                sensor, _ = self.registered_sensor(entry)
                coordinator = coordinator_for(self.hass, AsyncMock(), device(), entry)
                coordinator._entities_loaded = True
                coordinator.device = device(owner="mqtt")
                entered = asyncio.Event()
                release = asyncio.Event()
                reload_entry = coordinator._reload_entry

                async def blocked_reload(
                    entered=entered, release=release, reload_entry=reload_entry
                ):
                    entered.set()
                    await release.wait()
                    await reload_entry()

                with (
                    patch.object(coordinator, "_reload_entry", blocked_reload),
                    patch.object(ConfigEntries, "async_reload", AsyncMock()) as reload,
                    patch.object(
                        ConfigEntries,
                        "async_unload_platforms",
                        AsyncMock(return_value=True),
                    ),
                ):
                    coordinator._reload_changed_entities()
                    await entered.wait()
                    if replace:
                        coordinator_for(self.hass, AsyncMock(), device(), entry)
                    else:
                        self.assertTrue(
                            await gafctl_integration.async_unload_entry(
                                self.hass, entry
                            )
                        )
                    release.set()
                    await asyncio.gather(
                        coordinator._reload_task, return_exceptions=True
                    )
                    reload.assert_not_awaited()
                self.assertIsNotNone(self.entities.async_get(sensor.entity_id))

    async def test_reload_can_unload_its_own_coordinator(self) -> None:
        entry = await self.entry()
        coordinator = coordinator_for(self.hass, AsyncMock(), device(), entry)
        coordinator._entities_loaded = True
        coordinator.device = device(owner="mqtt")

        async def reload(entry_id):
            return await gafctl_integration.async_unload_entry(self.hass, entry)

        with (
            patch.object(
                ConfigEntries, "async_reload", AsyncMock(side_effect=reload)
            ) as reload_mock,
            patch.object(
                ConfigEntries, "async_unload_platforms", AsyncMock(return_value=True)
            ),
        ):
            coordinator._reload_changed_entities()
            await coordinator._reload_task
            reload_mock.assert_awaited_once_with(entry.entry_id)
            self.assertFalse(coordinator._reload_task.cancelled())

    async def test_invalid_ports_never_fetch_inventory(self) -> None:
        entry = await self.entry()
        for source in ("user", "reconfigure"):
            for address in (
                "http://proxy:bad",
                "http://proxy:65536",
                "http://proxy:-1",
            ):
                with self.subTest(source=source, address=address):
                    flow = self.flow({"source": source, "entry_id": entry.entry_id})
                    with patch.object(flow, "_fetch_devices", AsyncMock()) as fetch:
                        result = await getattr(flow, f"async_step_{source}")(
                            {"api_url": address}
                        )
                    self.assertEqual(result["errors"]["base"], "invalid_url")
                    fetch.assert_not_awaited()

    async def test_handoff_preserves_another_proxy_and_mqtt_registry(self) -> None:
        entry = await self.entry()
        other = await self.entry("650e8400-e29b-41d4-a716-446655440000")
        mqtt = await self.entry(domain="mqtt")
        old_sensor, old_device = self.registered_sensor(entry)
        other_sensor, other_device = self.registered_sensor(other)
        mqtt_sensor, mqtt_device = self.registered_sensor(mqtt)
        coordinator = coordinator_for(self.hass, AsyncMock(), device(), entry)
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
        coordinator = coordinator_for(
            self.hass, AsyncMock(), device(owner="mqtt"), entry
        )
        coordinator.device = device()
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNotNone(self.entities.async_get(sensor.entity_id))
        self.assertIsNone(self.entities.async_get(obsolete.entity_id))
        self.assertIsNotNone(self.devices.async_get(registered.id))

    async def test_reading_expiry_cannot_replace_newer_data_and_is_cancelled_on_unload(
        self,
    ):
        entry = await self.entry()
        coordinator = coordinator_for(self.hass, AsyncMock(), device(), entry)
        response = state_data(
            state=reported_state(
                provenance={
                    "backend": "legacy_ble",
                    "fetched_at_unix_ms": 9000,
                    "observed_at_unix_ms": 9000,
                }
            )
        )
        with (
            patch(
                "custom_components.gafctl.coordinator.time",
                return_value=10,
                create=True,
            ),
            patch(
                "custom_components.gafctl.coordinator.async_call_later", create=True
            ) as schedule,
        ):
            coordinator.async_set_updated_data(response)
            self.assertIsNotNone(coordinator.current_readings)
            schedule.assert_called_once()
            self.assertEqual(schedule.call_args.args[1], 89)
            old_expiry = schedule.call_args.args[2]
            newer = response | {"state": response["state"] | {"temperature_f": 99.0}}
            coordinator.async_set_updated_data(newer)
            old_expiry(None)
            self.assertIs(coordinator.data, newer)
            schedule.call_args.args[2](None)
            self.assertIsNone(coordinator.current_readings)
            self.assertFalse(coordinator.data["available"])
            coordinator.async_set_updated_data(newer)
            await coordinator.async_unload()
            schedule.return_value.assert_called()
            schedule.call_args.args[2](None)
            self.assertIs(coordinator.data, newer)

    async def test_confirmed_clear_timer_accepts_small_clock_skew(self):
        coordinator, client, _, old, _, _ = await self.control_case("preset")
        new = old | {
            "state": readings(
                settings=legacy_settings(
                    mode="timer",
                    controller_fan_on=False,
                    timer_remaining_minutes=0,
                    timer_original_minutes=0,
                ),
                provenance={
                    "backend": "legacy_ble",
                    "fetched_at_unix_ms": 100005,
                    "observed_at_unix_ms": 100005,
                },
            )
        }
        with (
            patch("custom_components.gafctl.coordinator.time", return_value=100),
            patch("custom_components.gafctl.coordinator.async_call_later") as schedule,
        ):
            client.fetch_state.side_effect = [
                old
                | {
                    "state": old["state"]
                    | {
                        "provenance": {
                            "backend": "legacy_ble",
                            "fetched_at_unix_ms": 99000,
                            "observed_at_unix_ms": 99000,
                        }
                    }
                },
                new,
            ]
            await coordinator.async_set_preset("timer_clear")
            self.assertIsNotNone(coordinator.current_readings)
            self.assertEqual(schedule.call_args.args[1], 90)
            client.set_control.assert_awaited_once_with(
                "configured", {"kind": "legacy_preset", "preset": "timer_clear"}
            )

    async def test_legacy_mode_selector_submits_and_confirms_all_three_modes(self):
        for option, mode, fan, duration in (
            ("Automatic", "automatic", False, 0),
            ("Timer", "timer", True, 60),
            ("Timer", "automatic", False, 0),
            ("Off", "timer", False, 0),
        ):
            with self.subTest(option=option):
                entry = await self.entry()
                selected = device(commands=["legacy_mode"])
                old = state_data(state=reported_state())
                updated = old | {
                    "state": reported_state(
                        settings=legacy_settings(
                            mode=mode,
                            controller_fan_on=fan,
                            automatic_temperature_tenths_f=1051,
                            automatic_humidity_tenths_percent=301,
                            timer_original_minutes=duration,
                        )
                    )
                }
                client = fake_client(devices=[selected], states=[old, updated])
                coordinator = coordinator_for(self.hass, client, selected, entry)
                coordinator.async_set_updated_data(old)
                selectors = await self.platform_entities(gafctl_select, coordinator)
                self.assertEqual(len(selectors), 1)
                selector = selectors[0]
                self.assertEqual(selector.options, ["Automatic", "Timer", "Off"])
                self.assertIsNone(selector.entity_category)
                self.assertTrue(selector.available)
                await selector.async_select_option(option)
                client.set_control.assert_awaited_once_with(
                    "configured", {"kind": "legacy_mode", "mode": option.lower()}
                )
                self.assertEqual(
                    selector.current_option,
                    "Automatic" if option == "Timer" and duration == 0 else option,
                )
                client.fetch_state.side_effect = None
                client.fetch_state.return_value = old | {
                    "state": reported_state(settings=legacy_settings(mode="ota"))
                }
                with self.assertRaises(ApiError):
                    await coordinator.async_set_mode("timer")
                self.assertIsNone(selector.current_option)

    async def test_future_timestamp_does_not_extend_expiry_on_cached_response(self):
        entry = await self.entry()
        coordinator = coordinator_for(self.hass, AsyncMock(), device(), entry)
        response = state_data(
            state=reported_state(
                provenance={
                    "backend": "legacy_ble",
                    "fetched_at_unix_ms": 100005,
                    "observed_at_unix_ms": 100005,
                }
            )
        )
        with (
            patch("custom_components.gafctl.coordinator.time") as now,
            patch("custom_components.gafctl.coordinator.async_call_later") as schedule,
        ):
            for instant, remaining in ((100, 90), (103, 87)):
                now.return_value = instant
                coordinator.async_set_updated_data(response)
                self.assertEqual(schedule.call_args.args[1], remaining)
            now.return_value = 190
            coordinator.async_set_updated_data(response)
            self.assertIsNone(coordinator.current_readings)
            self.assertEqual(schedule.call_count, 2)

    async def test_expired_reading_is_unavailable_on_receipt(self):
        entry = await self.entry()
        coordinator = coordinator_for(self.hass, AsyncMock(), device(), entry)
        with patch(
            "custom_components.gafctl.coordinator.time", return_value=100, create=True
        ):
            for observed in (0, 102000, None):
                coordinator.async_set_updated_data(
                    state_data(
                        state=reported_state(
                            provenance={
                                "backend": "legacy_ble",
                                "fetched_at_unix_ms": 99000,
                                "observed_at_unix_ms": observed,
                            }
                        )
                    )
                )
                if observed is None:
                    self.assertIsNotNone(coordinator.current_readings)
                else:
                    self.assertIsNone(coordinator.current_readings)

    async def test_generated_mqtt_templates_accept_nullable_payloads(self) -> None:
        configs = await self.discovery_configs()
        normal = state_data(
            state=reported_state(
                provenance={
                    "backend": "legacy_ble",
                    "fetched_at_unix_ms": None,
                    "observed_at_unix_ms": None,
                },
            )
        )
        initial = state_data(available=False, freshness="unknown")
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
        checked = set()
        for key, domain, config in configs:
            schemas = {
                "sensor": mqtt_sensor,
                "select": mqtt_select,
                "number": mqtt_number,
                "binary_sensor": mqtt_binary,
                "button": mqtt_button,
                "switch": mqtt_switch,
            }
            schemas[domain].DISCOVERY_SCHEMA(config)
            template = config.get("value_template")
            if not template or key == "sensor_control_result":
                continue
            rendered = [
                Template(template, self.hass).async_render({"value_json": payload})
                for payload in (initial, expired, partial, normal)
            ]
            if key == "sensor_automatic_temperature_threshold":
                checked.add(key)
                self.assertEqual(rendered, [None, None, None, 105.0])
            if key == "sensor_freshness":
                checked.add(key)
                self.assertEqual(rendered, ["unknown", "stale", "fresh", "fresh"])
        self.assertEqual(
            checked, {"sensor_automatic_temperature_threshold", "sensor_freshness"}
        )

    async def test_mqtt_migration_failure_replays_old_config_and_preserves_customization(
        self,
    ):
        fixture = os.environ.get("GAFCTL_DISCOVERY_FIXTURE")
        if not fixture:
            self.skipTest("set GAFCTL_DISCOVERY_FIXTURE to generated discovery configs")
        topic, document = (await asyncio.to_thread(read_discovery_fixture, fixture))[0]
        identifier = document["device"]["identifiers"][0]
        component = document["components"]["sensor_temperature"]
        grouped = document | {"components": {"sensor_temperature": component}}
        individual = MQTTDiscoveryPayload(component)
        del individual["platform"]
        individual["device"] = document["device"]
        individual["origin"] = document["origin"]
        _merge_common_device_options(individual, document)
        old_topic = f"homeassistant/sensor/gafctl/{identifier}_temperature/config"
        retained = {}
        platforms = []
        subscriptions = []
        entry_id = None

        async def start_discovery():
            nonlocal entry_id
            loader.async_setup(self.hass)
            entry = ConfigEntry(
                version=1,
                minor_version=1,
                domain="mqtt",
                title="Test MQTT",
                source="user",
                data={"broker": "localhost"},
                options={},
                unique_id=None,
                discovery_keys=MappingProxyType({}),
                subentries_data=[],
                entry_id=entry_id,
            )
            entry_id = entry.entry_id
            with patch.object(
                ConfigEntries, "async_setup", AsyncMock(return_value=True)
            ):
                await self.hass.config_entries.async_add(entry)
            entry._async_set_state(self.hass, ConfigEntryState.SETUP_IN_PROGRESS, None)

            def subscribe(pattern, callback, *_args):
                subscription = (pattern, callback)
                subscriptions.append(subscription)
                return lambda: subscriptions.remove(subscription)

            client = MagicMock()
            client.connected = True
            client.async_subscribe.side_effect = subscribe
            self.hass.data[DATA_MQTT] = MqttData(client=client, config=[])

            async def forward(hass, config_entry, components):
                self.assertEqual(components, {"sensor"})
                platform = EntityPlatform(
                    hass=hass,
                    logger=logging.getLogger(__name__),
                    domain="sensor",
                    platform_name="mqtt",
                    platform=mqtt_sensor,
                    scan_interval=timedelta(seconds=30),
                    entity_namespace=None,
                )
                platform.config_entry = config_entry
                platforms.append(platform)
                await mqtt_sensor.async_setup_entry(
                    hass, config_entry, platform._async_schedule_add_entities_for_entry
                )
                hass.data[DATA_MQTT].platforms_loaded.add("sensor")

            forward_patch = patch.object(
                mqtt_discovery,
                "async_forward_entry_setup_and_setup_discovery",
                side_effect=forward,
            )
            forward_patch.start()
            self.addCleanup(forward_patch.stop)
            await mqtt_discovery.async_start(self.hass, "homeassistant", entry)

        async def deliver(config_topic, payload, *, retain):
            message = json.dumps(payload)
            if retain:
                retained[config_topic] = message
            pattern = (
                "homeassistant/device/+/+/config"
                if config_topic == topic
                else "homeassistant/sensor/+/+/config"
            )
            callback = next(
                callback
                for subscribed, callback in subscriptions
                if subscribed == pattern
            )
            callback(ReceiveMessage(config_topic, message, 0, retain, pattern, 0))
            await self.hass.async_block_till_done()

        def customized_entry():
            entity = self.entities.async_get("sensor.attic_temperature")
            self.assertIsNotNone(entity)
            self.assertEqual(entity.unique_id, component["unique_id"])
            self.assertEqual(entity.name, "Attic custom temperature")
            self.assertEqual(entity.icon, "mdi:thermometer-alert")
            return entity

        await start_discovery()
        await deliver(old_topic, individual, retain=True)
        entity_id = self.entities.async_get_entity_id(
            "sensor", "mqtt", component["unique_id"]
        )
        self.assertIsNotNone(self.hass.states.get(entity_id))
        registered = self.entities.async_update_entity(
            entity_id,
            new_entity_id="sensor.attic_temperature",
            name="Attic custom temperature",
            icon="mdi:thermometer-alert",
        )
        registry_id, device_id = registered.id, registered.device_id
        await self.hass.async_block_till_done()
        await deliver(old_topic, {"migrate_discovery": True}, retain=False)
        self.assertIsNone(self.hass.states.get("sensor.attic_temperature"))
        self.assertEqual(customized_entry().id, registry_id)
        self.assertEqual(retained, {old_topic: json.dumps(individual)})
        # The broker rejects the grouped publish: no group reaches HA or replaces retention.
        self.assertNotIn(topic, retained)

        # Restart HA with the same registry files, then replay the unchanged retained config.
        await self.entities._store.async_save(self.entities._data_to_save())
        await self.devices._store.async_save(self.devices._data_to_save())
        for platform in platforms:
            await platform.async_reset()
        await self.hass.async_stop()
        platforms.clear()
        subscriptions.clear()
        self.hass = HomeAssistant(self.directory.name)
        self.hass.config_entries = ConfigEntries(self.hass, {})
        await ir.async_load(self.hass)
        dr.async_setup(self.hass)
        await dr.async_load(self.hass)
        await er.async_load(self.hass)
        self.devices = dr.async_get(self.hass)
        self.entities = er.async_get(self.hass)
        await start_discovery()
        await deliver(old_topic, json.loads(retained[old_topic]), retain=True)
        self.assertIsNotNone(self.hass.states.get("sensor.attic_temperature"))
        self.assertEqual(customized_entry().id, registry_id)
        self.assertEqual(customized_entry().device_id, device_id)

        # A later successful retry migrates the restored entity using the same unique ID.
        await deliver(old_topic, {"migrate_discovery": True}, retain=False)
        await deliver(topic, grouped, retain=True)
        self.assertIsNotNone(self.hass.states.get("sensor.attic_temperature"))
        self.assertEqual(customized_entry().id, registry_id)
        self.assertEqual(customized_entry().device_id, device_id)
        self.assertEqual(len(self.entities.entities), 1)
        for platform in platforms:
            await platform.async_reset()

    async def discovery_configs(self):
        fixture = os.environ.get("GAFCTL_DISCOVERY_FIXTURE")
        if not fixture:
            self.skipTest("set GAFCTL_DISCOVERY_FIXTURE to generated discovery configs")
        documents = await asyncio.to_thread(read_discovery_fixture, fixture)
        self.assertTrue(documents, "generated discovery fixture has no devices")
        configs = []
        for topic, document in documents:
            DEVICE_DISCOVERY_SCHEMA(document)
            identifier = document["device"]["identifiers"][0]
            self.assertEqual(topic, f"homeassistant/device/gafctl/{identifier}/config")
            self.assertEqual(document["device"]["manufacturer"], "GAF")
            self.assertEqual(document["availability_mode"], "all")
            self.assertEqual(document["payload_available"], "online")
            self.assertEqual(document["payload_not_available"], "offline")
            self.assertEqual(len(document["availability"]), 2)
            self.assertEqual(
                document["components"]["button_refresh"], {"platform": "button"}
            )
            for key, component in document["components"].items():
                domain = component["platform"]
                self.assertTrue(key.startswith(f"{domain}_"))
                if len(component) == 1:
                    continue
                config = MQTTDiscoveryPayload(component)
                del config["platform"]
                config["device"] = document["device"]
                config["origin"] = document["origin"]
                _merge_common_device_options(config, document)
                self.assertEqual(
                    config["unique_id"],
                    f"{identifier}_{key.removeprefix(f'{domain}_')}",
                )
                configs.append((key, domain, config))
        self.assertTrue(configs, "generated discovery fixture has no active components")
        return configs

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
        checked = set()
        for key, domain, config in await self.discovery_configs():
            template = config.get("command_template")
            if not template:
                continue
            render = partial(self.render_command, template)
            if domain == "number":
                checked.add("number")
                valid = render(config["min"])
                self.assertTrue(valid["request_id"])
                self.assertGreater(valid["issued_at_unix_ms"], 0)
                value_field = next(key for key in valid["command"] if key != "kind")
                self.assertEqual(valid["command"][value_field], config["min"])
                for value in (True, "garbage", 90.5, None):
                    self.assertIsNone(render(value)["command"][value_field])
            elif domain == "switch":
                checked.add("switch")
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
            elif domain == "select" and config.get("options") == [
                "Automatic",
                "Timer",
                "Off",
            ]:
                checked.add("legacy_mode")
                for option in config["options"]:
                    self.assertEqual(
                        render(option)["command"],
                        {"kind": "legacy_mode", "mode": option.lower()},
                    )
                for mode, fan, expected in (
                    ("automatic", False, "Automatic"),
                    ("timer", True, "Timer"),
                    ("timer", False, "Off"),
                    ("timer", None, None),
                    ("ota", True, None),
                ):
                    self.assertEqual(
                        self.render_settings(
                            config, {"mode": mode, "controller_fan_on": fan}
                        ),
                        expected,
                    )
        self.assertEqual(checked, {"number", "switch", "legacy_mode"})

    async def test_generated_mqtt_binary_modes_and_presets_preserve_unknown(
        self,
    ) -> None:
        checked = set()
        for key, domain, config in await self.discovery_configs():
            render = partial(self.render_settings, config)
            if domain == "binary_sensor" and key.endswith("_mode"):
                checked.add("binary_mode")
                mode = config["unique_id"].rsplit("_", 2)[-2]
                self.assertEqual(render({"mode": mode}), "ON")
                self.assertEqual(render({"mode": "off"}), "OFF")
                self.assertIsNone(render({"mode": "conflicting"}))
                self.assertIsNone(render({}))
            elif key == "select_automatic_thresholds":
                checked.add("automatic_thresholds")
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
        self.assertEqual(checked, {"binary_mode", "automatic_thresholds"})

    async def test_poll_requires_current_proxy_identity_and_successful_inventory(self):
        from homeassistant.helpers.update_coordinator import UpdateFailed

        entry = await self.entry()
        for failure in ("proxy", "inventory"):
            with self.subTest(failure=failure):
                client = fake_client(
                    devices=[device("650e8400-e29b-41d4-a716-446655440000")],
                    state=state_data(state=readings(temperature_f=100)),
                )
                if failure == "inventory":
                    client.fetch_devices.side_effect = ApiError("inventory unavailable")
                coordinator = coordinator_for(self.hass, client, device(), entry)
                with self.assertRaises(UpdateFailed):
                    await coordinator._async_update_data()
                client.fetch_state.assert_not_called()

    async def test_reading_entity_presentation_matches_each_backend(self):
        common = {
            "temperature": (
                "Ambient temperature",
                ("temperature_f",),
                UnitOfTemperature.FAHRENHEIT,
                SensorDeviceClass.TEMPERATURE,
            ),
            "humidity": (
                "Relative humidity",
                ("humidity_percent",),
                PERCENTAGE,
                SensorDeviceClass.HUMIDITY,
            ),
            "mode": ("Controller mode", ("settings", "mode"), None, None),
            "firmware_version": (
                "Firmware version",
                ("diagnostics", "firmware_version"),
                None,
                None,
            ),
        }
        sensors = {
            "legacy_ble": common
            | {
                "automatic_temperature_threshold": (
                    "Automatic temperature threshold",
                    ("settings", "automatic_temperature_tenths_f"),
                    UnitOfTemperature.FAHRENHEIT,
                    SensorDeviceClass.TEMPERATURE,
                ),
                "automatic_humidity_threshold": (
                    "Automatic humidity threshold",
                    ("settings", "automatic_humidity_tenths_percent"),
                    PERCENTAGE,
                    None,
                ),
                "timer_remaining": (
                    "Timer remaining",
                    ("settings", "timer_remaining_minutes"),
                    UnitOfTime.MINUTES,
                    None,
                ),
                "timer_original": (
                    "Timer duration",
                    ("settings", "timer_original_minutes"),
                    UnitOfTime.MINUTES,
                    None,
                ),
            },
            "quick_connect": common
            | {
                "signal_strength_raw": (
                    "Signal strength (reported)",
                    ("diagnostics", "signal_strength_raw"),
                    None,
                    None,
                ),
                "verified_raw": (
                    "Verification (reported)",
                    ("diagnostics", "verified_raw"),
                    None,
                    None,
                ),
            },
        }
        binaries = {
            "legacy_ble": {
                "controller_fan_flag": (
                    "Controller fan flag",
                    ("settings", "controller_fan_on"),
                    "controller",
                ),
            },
            "quick_connect": {
                "running_estimate": (
                    "Running estimate",
                    ("estimated_running",),
                    "inferred",
                ),
                "ota_in_progress": (
                    "OTA in progress",
                    ("diagnostics", "ota_in_progress"),
                    "reported",
                ),
                "automatic_mode": ("Automatic mode", ("settings", "mode"), "reported"),
                "timer_mode": ("Timer mode", ("settings", "mode"), "reported"),
                "manual_mode": ("Manual mode", ("settings", "mode"), "reported"),
                "humidity_monitor": (
                    "Humidity monitoring",
                    ("settings", "humidity_monitor"),
                    "reported",
                ),
            },
        }
        entry = await self.entry()
        for backend in sensors:
            coordinator = coordinator_for(
                self.hass, AsyncMock(), device(backend=backend), entry
            )
            coordinator.async_set_updated_data(
                state_data(backend=backend, state=reported_state(backend))
            )
            for platform, expected in (
                (gafctl_sensor, sensors[backend]),
                (gafctl_binary, binaries[backend]),
            ):
                with self.subTest(backend=backend, platform=platform.__name__):
                    entities = await self.platform_entities(platform, coordinator)
                    selected = {entity._key: entity for entity in entities}
                    self.assertEqual(set(selected), set(expected))
                    for key, (name, path, *details) in expected.items():
                        entity = selected[key]
                        self.assertEqual(entity.name, name)
                        self.assertEqual(entity._path, path)
                        measurement = key in {"temperature", "humidity"}
                        self.assertEqual(
                            entity.entity_category,
                            None if measurement else EntityCategory.DIAGNOSTIC,
                        )
                        if platform is gafctl_sensor:
                            unit, device_class = details
                            self.assertEqual(entity.native_unit_of_measurement, unit)
                            self.assertEqual(entity.device_class, device_class)
                            self.assertEqual(
                                entity.state_class,
                                SensorStateClass.MEASUREMENT if measurement else None,
                            )
                        else:
                            self.assertEqual(
                                entity.extra_state_attributes["provenance"], details[0]
                            )

    async def test_reading_entities_preserve_reported_and_unknown_values(self):
        entry = await self.entry()
        cases = (
            (
                gafctl_sensor,
                "legacy_ble",
                readings(
                    settings=legacy_settings(
                        timer_original_minutes=2,
                        automatic_temperature_tenths_f=1055,
                        automatic_humidity_tenths_percent=333,
                    )
                ),
                {
                    "timer_original": 2,
                    "automatic_temperature_threshold": 105.5,
                    "automatic_humidity_threshold": 33.3,
                },
            ),
            (
                gafctl_sensor,
                "legacy_ble",
                readings(),
                {
                    "automatic_temperature_threshold": None,
                    "automatic_humidity_threshold": None,
                },
            ),
            (
                gafctl_sensor,
                "quick_connect",
                readings(
                    settings=quickconnect_settings(),
                    diagnostics=diagnostics(
                        signal_strength_raw="unknown-units",
                        verified_raw="unknown-semantics",
                    ),
                ),
                {
                    "signal_strength_raw": "unknown-units",
                    "verified_raw": "unknown-semantics",
                },
            ),
            (
                gafctl_binary,
                "legacy_ble",
                readings(settings=legacy_settings(controller_fan_on=False)),
                {"controller_fan_flag": False},
            ),
            (
                gafctl_binary,
                "quick_connect",
                readings(
                    settings=quickconnect_settings(),
                    diagnostics=diagnostics(ota_in_progress=True),
                ),
                {
                    "running_estimate": None,
                    "ota_in_progress": True,
                    "automatic_mode": None,
                    "timer_mode": None,
                    "manual_mode": None,
                    "humidity_monitor": None,
                },
            ),
        )
        for platform, backend, values, expected in cases:
            with self.subTest(platform=platform.__name__, backend=backend):
                coordinator = coordinator_for(
                    self.hass, AsyncMock(), device(backend=backend), entry
                )
                coordinator.async_set_updated_data(
                    state_data(backend=backend, state=values)
                )
                entities = await self.platform_entities(platform, coordinator)
                selected = {
                    entity.unique_id.removeprefix(entry.unique_id + "_"): entity
                    for entity in entities
                }
                if platform is gafctl_binary:
                    self.assertEqual(
                        {key: entity.is_on for key, entity in selected.items()},
                        expected,
                    )
                else:
                    for key, value in expected.items():
                        self.assertEqual(selected[key].native_value, value)
                        if key.endswith("_raw"):
                            self.assertIsNone(selected[key].native_unit_of_measurement)
                            self.assertIsNone(selected[key].device_class)
                self.assertTrue(all(entity.available for entity in entities))


if __name__ == "__main__":
    unittest.main()
