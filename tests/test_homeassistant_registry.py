"""Registry tests run with the installed Home Assistant Python environment."""

import sys
import json
import os
import tempfile
import unittest
from pathlib import Path
from types import MappingProxyType
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from homeassistant.config_entries import ConfigEntries, ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.helpers import device_registry as dr, entity_registry as er
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.template import Template
from homeassistant.components.mqtt import sensor as mqtt_sensor, select as mqtt_select

from custom_components.updraft import UpdraftCoordinator
from custom_components.updraft.button import UpdraftRefreshButton
from custom_components.updraft import number as updraft_number
from custom_components.updraft.client import ApiError
from homeassistant.exceptions import HomeAssistantError


PROXY_ID = "550e8400-e29b-41d4-a716-446655440000"


def device(proxy_id=PROXY_ID, owner="http"):
    return {
        "proxy_id": proxy_id,
        "id": "configured",
        "name": "Vent",
        "backend": "legacy_ble",
        "state": True,
        "commands": [],
        "state_source": owner,
        "command_source": owner,
    }


class RegistryTests(unittest.IsolatedAsyncioTestCase):
    async def test_ble_timer_can_replace_manual_sentinel_with_bounded_timer(self):
        entry = await self.entry()
        selected = device() | {"commands": [{"kind": "legacy_timer"}]}
        old = {"available": True, "freshness": "fresh", "state": {"timer_original_minutes": 600}}
        new = old | {"state": {"timer_original_minutes": 1}}
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        client.fetch_state.side_effect = [old, new]
        coordinator = UpdraftCoordinator(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        self.hass.data["updraft"] = {entry.entry_id: coordinator}
        entities = []
        await updraft_number.async_setup_entry(self.hass, entry, entities.extend)
        timer = entities[0]
        self.assertTrue(timer.available)
        self.assertIsNone(timer.native_value)
        await timer.async_set_native_value(1)
        client.set_control.assert_awaited_once_with("configured", {"kind": "legacy_timer", "minutes": 1})

    async def test_ble_numbers_accept_fractional_readback_and_send_partial_command(self):
        entry = await self.entry()
        selected = device() | {"commands": [
            {"kind": "legacy_automatic_temperature"},
            {"kind": "legacy_automatic_humidity"},
            {"kind": "legacy_timer"},
        ]}
        old = {"available": True, "freshness": "fresh", "state": {
            "automatic_temperature_threshold_f": 105.1,
            "automatic_humidity_threshold_percent": 30.1,
            "timer_original_minutes": 0,
        }}
        new = old | {"state": old["state"] | {"automatic_temperature_threshold_f": 110.0}}
        client = AsyncMock()
        client.fetch_devices.return_value = [selected]
        client.fetch_state.side_effect = [old, new]
        coordinator = UpdraftCoordinator(self.hass, client, selected, entry)
        coordinator.async_set_updated_data(old)
        self.hass.data["updraft"] = {entry.entry_id: coordinator}
        entities = []
        await updraft_number.async_setup_entry(self.hass, entry, entities.extend)
        self.assertEqual(len(entities), 3)
        temperature, humidity, timer = entities
        self.assertTrue(temperature.available)
        self.assertEqual(temperature.native_value, 105.1)
        self.assertTrue(humidity.available)
        self.assertEqual(humidity.native_value, 30.1)
        self.assertEqual(timer.native_min_value, 0)
        self.assertEqual(timer.native_step, 1)
        await temperature.async_set_native_value(110)
        client.set_control.assert_awaited_once_with("configured", {
            "kind": "legacy_automatic_temperature", "temperature_f": 110,
        })
        for invalid in [89, 121, 105.1, True, float("nan"), float("inf")]:
            with self.assertRaises(HomeAssistantError):
                await temperature.async_set_native_value(invalid)
        self.assertEqual(client.set_control.await_count, 1)

    async def asyncSetUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.hass = HomeAssistant(self.directory.name)
        self.hass.config_entries = ConfigEntries(self.hass, {})
        await ir.async_load(self.hass)
        dr.async_setup(self.hass)
        await dr.async_load(self.hass)
        await er.async_load(self.hass)
        self.devices = dr.async_get(self.hass)
        self.entities = er.async_get(self.hass)

    async def asyncTearDown(self):
        await self.hass.async_stop()
        self.directory.cleanup()

    async def test_refresh_button_reads_unavailable_device_and_applies_returned_state(self):
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        client.refresh.return_value = {"available": True, "freshness": "fresh", "state": {"temperature_f": 100}}
        client.fetch_state.return_value = client.refresh.return_value
        coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
        coordinator.async_set_updated_data({"available": False, "state": None})
        button = UpdraftRefreshButton(coordinator, entry)
        self.assertTrue(button.available)
        await button.async_press()
        client.refresh.assert_awaited_once_with("configured", "legacy_ble")
        client.fetch_state.assert_awaited_once_with("configured")
        self.assertEqual(coordinator.data["state"]["temperature_f"], 100)

    async def test_refresh_failure_preserves_current_data_and_reports_failure(self):
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        client.refresh.side_effect = ApiError("device refresh did not complete")
        coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
        old = {"available": True, "state": {"temperature_f": 99}}
        coordinator.async_set_updated_data(old)
        with self.assertRaises(HomeAssistantError):
            await UpdraftRefreshButton(coordinator, entry).async_press()
        self.assertEqual(coordinator.data, old)
        client.refresh.assert_awaited_once()

    async def test_refresh_rechecks_proxy_identity_and_http_ownership(self):
        entry = await self.entry()
        for current in (device(owner="mqtt"), device("650e8400-e29b-41d4-a716-446655440000")):
            client = AsyncMock()
            client.fetch_devices.return_value = [current]
            coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
            with self.assertRaises(HomeAssistantError):
                await UpdraftRefreshButton(coordinator, entry).async_press()
            client.refresh.assert_not_called()

    async def test_refresh_reports_failure_if_followup_cannot_get_current_readings(self):
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        client.refresh.return_value = {"available": True, "state": {"temperature_f": 100}}
        client.fetch_state.return_value = {"available": False, "state": None}
        coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
        with self.assertRaises(HomeAssistantError):
            await UpdraftRefreshButton(coordinator, entry).async_press()

    async def test_refresh_completion_rejects_owner_or_proxy_changed_during_read(self):
        entry = await self.entry()
        for changed in (device(owner="mqtt"), device("650e8400-e29b-41d4-a716-446655440000")):
            client = AsyncMock()
            client.fetch_devices.side_effect = [[device()], [changed]]
            client.refresh.return_value = {"available": True, "state": {"temperature_f": 99}}
            coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
            old = {"available": False, "state": None}
            coordinator.async_set_updated_data(old)
            with self.assertRaises(HomeAssistantError):
                await UpdraftRefreshButton(coordinator, entry).async_press()
            self.assertEqual(coordinator.data, old)

    async def test_late_refresh_response_does_not_replace_newer_periodic_data(self):
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device()]
        newer = {"available": True, "freshness": "fresh", "state": {"temperature_f": 110}}
        older = {"available": True, "freshness": "fresh", "state": {"temperature_f": 100}}
        coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
        async def delayed_response(*_):
            coordinator.async_set_updated_data(newer)
            return older
        client.refresh.side_effect = delayed_response
        client.fetch_state.return_value = newer
        await UpdraftRefreshButton(coordinator, entry).async_press()
        self.assertEqual(coordinator.data, newer)

    async def entry(self, proxy_id=PROXY_ID, domain="updraft"):
        entry = ConfigEntry(
            version=2, minor_version=1,
            domain=domain, title="Vent", source="user",
            data={"api_url": "http://127.0.0.1:8787", "device_id": "configured", "proxy_id": proxy_id, "backend": "legacy_ble"},
            options={}, unique_id=f"updraft_{proxy_id}_configured",
            discovery_keys=MappingProxyType({}), subentries_data=[],
        )
        with patch.object(ConfigEntries, "async_setup", AsyncMock(return_value=True)):
            await self.hass.config_entries.async_add(entry)
        return entry

    def registered_sensor(self, entry, key="temperature"):
        registered_device = self.devices.async_get_or_create(
            config_entry_id=entry.entry_id,
            identifiers={(entry.domain, entry.unique_id)}, name="Vent",
        )
        entity = self.entities.async_get_or_create(
            "sensor", entry.domain, f"{entry.unique_id}_{key}",
            config_entry=entry, device_id=registered_device.id,
        )
        return entity, registered_device

    async def test_mqtt_handoff_removes_http_entities_and_empty_device(self):
        entry = await self.entry()
        sensor, old_device = self.registered_sensor(entry)
        coordinator = UpdraftCoordinator(self.hass, AsyncMock(), device(), entry)
        coordinator.device = device(owner="mqtt")
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNone(self.entities.async_get(sensor.entity_id))
        self.assertIsNone(self.devices.async_get(old_device.id))

    async def test_handoff_preserves_another_proxy_and_mqtt_registry(self):
        entry = await self.entry()
        other = await self.entry("650e8400-e29b-41d4-a716-446655440000")
        mqtt = await self.entry(domain="mqtt")
        old_sensor, old_device = self.registered_sensor(entry)
        other_sensor, other_device = self.registered_sensor(other)
        mqtt_sensor, mqtt_device = self.registered_sensor(mqtt)
        coordinator = UpdraftCoordinator(self.hass, AsyncMock(), device(), entry)
        coordinator.device = device(owner="mqtt")
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNone(self.entities.async_get(old_sensor.entity_id))
        self.assertIsNone(self.devices.async_get(old_device.id))
        self.assertIsNotNone(self.entities.async_get(other_sensor.entity_id))
        self.assertIsNotNone(self.devices.async_get(other_device.id))
        self.assertIsNotNone(self.entities.async_get(mqtt_sensor.entity_id))
        self.assertIsNotNone(self.devices.async_get(mqtt_device.id))

    async def test_http_return_preserves_current_entities_and_prunes_removed_keys(self):
        entry = await self.entry()
        sensor, registered = self.registered_sensor(entry)
        obsolete, _ = self.registered_sensor(entry, "obsolete")
        coordinator = UpdraftCoordinator(self.hass, AsyncMock(), device(owner="mqtt"), entry)
        coordinator.device = device()
        with patch.object(ConfigEntries, "async_reload", AsyncMock(return_value=True)):
            await coordinator._reload_entry()
        self.assertIsNotNone(self.entities.async_get(sensor.entity_id))
        self.assertIsNone(self.entities.async_get(obsolete.entity_id))
        self.assertIsNotNone(self.devices.async_get(registered.id))

    async def test_proxy_identity_change_cannot_supply_readings_for_old_device(self):
        entry = await self.entry()
        client = AsyncMock()
        client.fetch_devices.return_value = [device("650e8400-e29b-41d4-a716-446655440000")]
        coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
        from homeassistant.helpers.update_coordinator import UpdateFailed
        with self.assertRaises(UpdateFailed):
            await coordinator._async_update_data()
        client.fetch_state.assert_not_called()

    async def test_failed_inventory_cannot_bypass_identity_and_ownership_validation(self):
        entry = await self.entry()
        client = AsyncMock()
        from custom_components.updraft.client import ApiError
        from homeassistant.helpers.update_coordinator import UpdateFailed
        client.fetch_devices.side_effect = ApiError("inventory unavailable")
        client.fetch_state.return_value = {"available": True, "state": {"temperature_f": 100}}
        coordinator = UpdraftCoordinator(self.hass, client, device(), entry)
        with self.assertRaises(UpdateFailed):
            await coordinator._async_update_data()
        client.fetch_state.assert_not_called()

    async def test_generated_mqtt_templates_accept_nullable_payloads(self):
        fixture = os.environ.get("UPDRAFT_DISCOVERY_FIXTURE")
        if not fixture:
            self.skipTest("set UPDRAFT_DISCOVERY_FIXTURE to generated discovery configs")
        configs = json.loads(Path(fixture).read_text())
        normal = {
            "available": True, "inventory_status": "present", "last_error": None,
            "state": {"temperature_f": 98.6, "humidity_percent": 42.1,
                      "diagnostics": {"firmware_version": "3.0.0"},
                      "settings": {"automatic_temperature_tenths_f": 1050,
                                   "automatic_humidity_tenths_percent": 300,
                                   "mode": "automatic", "timer_remaining_minutes": 0,
                                   "timer_original_minutes": 0, "controller_fan_on": False}},
        }
        initial = {"available": False, "inventory_status": "unknown", "state": None, "last_error": None}
        expired = initial | {"inventory_status": "present", "last_error": "device state expired"}
        partial = normal | {"state": {"temperature_f": None, "humidity_percent": None, "diagnostics": None,
                                     "settings": {"automatic_temperature_tenths_f": None, "automatic_humidity_tenths_percent": None}}}
        for topic, config in configs:
            schema = mqtt_select.DISCOVERY_SCHEMA if "/select/" in topic else mqtt_sensor.DISCOVERY_SCHEMA
            schema(config)
            template = config.get("value_template")
            if not template or topic.endswith("/control_result/config"):
                continue
            rendered = [Template(template, self.hass).async_render({"value_json": payload})
                        for payload in (initial, expired, partial, normal)]
            if topic.endswith("/automatic_temperature_threshold/config"):
                self.assertEqual(rendered, [None, None, None, 105.0])
            if topic.endswith("/freshness/config"):
                self.assertEqual(rendered, ["unknown", "stale", "fresh", "fresh"])


if __name__ == "__main__":
    unittest.main()
