"""Device polling, transport ownership and serialized controls."""

from __future__ import annotations

import asyncio
import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ApiClient
from .const import CONF_DEVICE_ID, CONF_PROXY_ID, UPDATE_INTERVAL
from .controls import (
    CONTROL_PRESETS,
    QUICKCONNECT_MODES,
    NumberControl,
    entity_keys,
    preset_matches,
)
from .models import ApiError, Backend, Device, DeviceState, JsonObject, Readings

LOGGER = logging.getLogger(__name__)


class GafctlCoordinator(DataUpdateCoordinator[DeviceState]):
    def __init__(
        self,
        hass: HomeAssistant,
        client: ApiClient,
        device: Device,
        entry: GafctlConfigEntry,
    ) -> None:
        self.client = client
        self.device = device
        self.device_id = device.id
        self.entry = entry
        self.loaded_entity_keys = entity_keys(device)
        self.command_lock = asyncio.Lock()
        self._reload_scheduled = False
        self._entities_loaded = False
        super().__init__(
            hass,
            config_entry=entry,
            logger=LOGGER,
            name=f"Gafctl {device.name}",
            update_interval=UPDATE_INTERVAL,
        )

    async def _async_update_data(self) -> DeviceState:
        try:
            current = await self._async_resolve_device()
            if current is None:
                raise UpdateFailed("configured device is absent from this proxy")
            state = await self.client.fetch_state(self.device_id)
            if state.backend != current.backend:
                raise ApiError("proxy returned state for a different backend")
            return state
        except ApiError as error:
            raise UpdateFailed(str(error)) from error

    async def _async_resolve_device(self) -> Device | None:
        devices = await self.client.fetch_devices()
        current = next(
            (
                device
                for device in devices
                if device.id == self.device_id
                and device.proxy_id == self.entry.data[CONF_PROXY_ID]
                and device.backend == self.entry.data["backend"]
            ),
            None,
        )
        self.device = current or unavailable_device(self.entry)
        self._reload_changed_entities()
        return current

    async def async_refresh_device(self) -> None:
        async with self.command_lock:
            current = await self._async_http_state_device()
            await self.client.refresh(self.device_id, current.backend)
            await self._async_http_state_device()
            await self.async_refresh()
            if self.current_readings is None:
                raise ApiError(
                    "device was refreshed, but current HTTP readings are unavailable"
                )

    async def _async_http_state_device(self) -> Device:
        current = await self._async_resolve_device()
        if current is None or not current.read_state or not self.http_owned:
            raise ApiError("this device does not own HTTP readings")
        return current

    def _reload_changed_entities(self) -> None:
        if (
            self._entities_loaded
            and entity_keys(self.device) != self.loaded_entity_keys
            and not self._reload_scheduled
        ):
            self._reload_scheduled = True
            self.hass.async_create_task(self._reload_entry())

    async def _reload_entry(self) -> None:
        try:
            async_cleanup_registry(self.hass, self.entry, self.device)
            reloaded = await self.hass.config_entries.async_reload(self.entry.entry_id)
            if not reloaded:
                self._reload_scheduled = False
        except Exception:
            self._reload_scheduled = False
            LOGGER.exception("Could not reload changed Gafctl entities")

    async def async_set_mode(
        self, mode: str, *, only_if_current: str | None = None
    ) -> None:
        if mode not in QUICKCONNECT_MODES or (
            only_if_current is not None
            and (only_if_current not in QUICKCONNECT_MODES or mode != "off")
        ):
            raise ApiError("unsupported device mode")
        async with self.command_lock:
            await self.async_refresh()
            state = self._require_control("quick_connect_mode", "quick_connect")
            if only_if_current is not None:
                if state.settings.mode not in QUICKCONNECT_MODES:
                    raise ApiError("current device mode is unknown")
                if state.settings.mode != only_if_current:
                    return
            command = (
                {
                    "kind": "quick_connect_conditional_off",
                    "only_if_current": only_if_current,
                }
                if only_if_current is not None
                else {"kind": "quick_connect_mode", "mode": mode}
            )
            await self._async_submit_control(command)
            state = self._require_control("quick_connect_mode", "quick_connect")
            matches = (
                state.settings.mode == mode
                if only_if_current is None
                else state.settings.mode in QUICKCONNECT_MODES
                and state.settings.mode != only_if_current
            )
            if not matches:
                raise ApiError("confirmed control has no matching current mode")

    async def async_set_number(self, control: NumberControl, value: float) -> None:
        validated = control.validate(value)
        async with self.command_lock:
            await self.async_refresh()
            state = self._require_control(control.capability, control.backend)
            if not control.current_supported(state):
                raise ApiError("the selected device has no supported current setting")
            await self._async_submit_control(control.command(validated))
            state = self._require_control(control.capability, control.backend)
            if control.reading(state) != validated:
                raise ApiError("confirmed control has no matching current setting")

    async def async_set_preset(self, preset: str) -> None:
        if preset not in CONTROL_PRESETS:
            raise ApiError("unsupported control preset")
        async with self.command_lock:
            await self.async_refresh()
            self._require_control("legacy_preset", "legacy_ble")
            await self._async_submit_control(preset)
            state = self._require_control("legacy_preset", "legacy_ble")
            if not preset_matches(state, preset):
                raise ApiError("confirmed control has no matching current preset")

    async def _async_submit_control(self, command: str | JsonObject) -> None:
        control_error = None
        try:
            await self.client.set_control(self.device_id, command)
        except ApiError as error:
            control_error = error
        try:
            await self.async_refresh()
            if self.current_readings is None:
                raise ApiError("current state refresh failed")
        except Exception as error:
            if control_error is not None:
                raise control_error from error
            raise ApiError(
                "control was confirmed, but current state refresh failed"
            ) from error
        if control_error is not None:
            raise control_error

    def _require_control(self, capability: str, backend: Backend) -> Readings:
        state = self.control_readings(capability, backend)
        if state is None:
            raise ApiError("the selected device has no current control")
        return state

    @property
    def current_readings(self) -> Readings | None:
        data = self.data
        if not (
            self.last_update_success
            and self.http_owned
            and self.device.read_state
            and data
            and data.available
            and data.freshness == "fresh"
        ):
            return None
        return data.state

    def control_readings(self, capability: str, backend: Backend) -> Readings | None:
        if self.device.backend != backend or not self.supports(capability):
            return None
        return self.current_readings

    @property
    def mode_control_available(self) -> bool:
        return self.control_readings("quick_connect_mode", "quick_connect") is not None

    def number_control_available(self, control: NumberControl) -> bool:
        state = self.control_readings(control.capability, control.backend)
        return state is not None and control.current_supported(state)

    def supports(self, command_kind: str) -> bool:
        return self.http_owned and command_kind in self.device.commands

    @property
    def http_owned(self) -> bool:
        return self.device.owner == "http"


def unavailable_device(entry: GafctlConfigEntry) -> Device:
    return Device(
        proxy_id=entry.data[CONF_PROXY_ID],
        id=entry.data[CONF_DEVICE_ID],
        name=entry.title,
        backend=entry.data["backend"],
        read_state=False,
        commands=frozenset(),
        owner="http",
    )


@callback
def async_cleanup_registry(
    hass: HomeAssistant, entry: GafctlConfigEntry, device: Device
) -> None:
    expected = {
        (platform, f"{entry.unique_id}_{key}")
        for platform, keys in entity_keys(device).items()
        for key in keys
    }
    entities = er.async_get(hass)
    for entity in er.async_entries_for_config_entry(entities, entry.entry_id):
        if (entity.domain, entity.unique_id) not in expected:
            entities.async_remove(entity.entity_id)
    devices = dr.async_get(hass)
    for registered in dr.async_entries_for_config_entry(devices, entry.entry_id):
        if not er.async_entries_for_device(entities, registered.id):
            devices.async_remove_device(registered.id)


type GafctlConfigEntry = ConfigEntry[GafctlCoordinator]
