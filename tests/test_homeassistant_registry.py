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
