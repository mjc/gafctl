"""Device polling, transport ownership and serialized controls."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Callable
from math import isfinite
from time import time
from typing import Any

from homeassistant.config_entries import ConfigEntry, ConfigEntryState
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.event import async_call_later
from homeassistant.helpers.storage import Store
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .client import ApiClient, _devices_response, _state_response
from .const import (
    CLOUD_UPDATE_INTERVAL,
    CONF_API_URL,
    CONF_DEVICE_ID,
    DOMAIN,
    MAX_CLOCK_SKEW,
    READING_MAX_AGE,
    UPDATE_INTERVAL,
)
from .controls import (
    QUICKCONNECT_MODES,
    NumberControl,
    command_kinds,
    entity_keys,
    legacy_mode,
)
from .models import (
    ApiError,
    Backend,
    Device,
    DeviceState,
    JsonObject,
    ProxyReadFailed,
    Readings,
    configured_identity,
    device_identity,
)

LOGGER = logging.getLogger(__name__)


class GafctlCoordinator(DataUpdateCoordinator[DeviceState]):
    def __init__(
        self, hass: HomeAssistant, client: ApiClient, entry: GafctlConfigEntry
    ) -> None:
        self.client = client
        self.device: Device | None = None
        self.device_id = entry.data[CONF_DEVICE_ID]
        self._identity = configured_identity(entry.data)
        self.entry = entry
        self.loaded_entity_keys: dict[str, set[str]] = {}
        self.command_lock = asyncio.Lock()
        self._reload_task: asyncio.Task[None] | None = None
        self._expiry_cancel: Callable[[], None] | None = None
        self._reading_expiry: tuple[float, float] | None = None
        self._unloaded = False
        self._entities_loaded = False
        self.api_error: str | None = None
        self._snapshot: dict[str, Any] = {}
        self._save_pending = False
        self._store = Store[dict[str, Any]](
            hass, 1, f"{DOMAIN}.{entry.entry_id}", private=True, atomic_writes=True
        )
        super().__init__(
            hass,
            config_entry=entry,
            logger=LOGGER,
            name=f"Gafctl {entry.title}",
            update_interval=UPDATE_INTERVAL
            if entry.data["backend"] == "legacy_ble"
            else CLOUD_UPDATE_INTERVAL,
        )

    async def _async_setup(self) -> None:
        record = await self._store.async_load()
        if not record:
            return
        try:
            device = _devices_response({"devices": [record["device"]]})[0]
            state = _state_response(record["response"], self.device_id)
            expires_at = record["expires_at"]
            if (
                record["api_url"] != self.entry.data[CONF_API_URL]
                or device_identity(device) != self._identity
                or state["backend"] != device["backend"]
                or device["state_source"] != "http"
                or not device["capabilities"]["read_state"]
                or type(expires_at) not in (int, float)
                or not isfinite(expires_at)
            ):
                raise ApiError("stored readings do not match this integration")
            state = self._schedule_reading_expiry(state, expires_at=expires_at)
            if not state["available"]:
                raise ApiError("stored readings expired")
        except ApiError, KeyError, TypeError:
            await self._store.async_save({})
            return
        self.device = device
        self.data = state
        self._snapshot = record
        self.api_error = "waiting for the local proxy"

    @callback
    def _snapshot_to_save(self) -> dict[str, Any]:
        self._save_pending = False
        return self._snapshot

    @callback
    def _forget_snapshot(self) -> None:
        if self._snapshot:
            self._snapshot = {}
            self._save_pending = True
            self._store.async_delay_save(self._snapshot_to_save)

    async def _discard_snapshot(self, message: str) -> None:
        self._cancel_reading_expiry()
        if self.data:
            self.data = self.data | {
                "available": False,
                "state": None,
                "last_error": message,
            }
        if self._snapshot:
            self._snapshot = {}
            self._save_pending = False
            await self._store.async_save({})

    async def _async_update_data(self) -> DeviceState:
        try:
            current = await self._async_resolve_device()
            if current is None:
                raise ApiError("configured device is absent from this proxy")
            state = await self.client.fetch_state(self.device_id)
            if self._unloaded:
                raise UpdateFailed("integration entry unloaded during state read")
            if state["backend"] != current["backend"]:
                raise ApiError("proxy returned state for a different backend")
            state = self._schedule_reading_expiry(state)
            self.api_error = None
            if (
                state["available"]
                and self.http_owned
                and current["capabilities"]["read_state"]
            ):
                self._snapshot = {
                    "api_url": self.entry.data[CONF_API_URL],
                    "device": current,
                    "response": state,
                    "expires_at": self._reading_expiry[1],
                }
                if not self._save_pending:
                    self._save_pending = True
                    self._store.async_delay_save(self._snapshot_to_save, 30)
            else:
                await self._discard_snapshot(
                    state["last_error"] or "device unavailable"
                )
            return state
        except ProxyReadFailed as error:
            if self._unloaded:
                raise UpdateFailed("integration entry unloaded during read") from error
            self.api_error = str(error)
            if self.data and self.http_owned:
                cached = self._schedule_reading_expiry(self.data)
                if cached["available"]:
                    return cached
                self.data = cached
                self._forget_snapshot()
            raise UpdateFailed(str(error)) from error
        except ApiError as error:
            if self._unloaded:
                raise UpdateFailed("integration entry unloaded during read") from error
            self.api_error = str(error)
            await self._discard_snapshot(str(error))
            raise UpdateFailed(str(error)) from error

    @callback
    def async_set_updated_data(self, data: DeviceState) -> None:
        data = self._schedule_reading_expiry(data)
        if not data["available"]:
            self._forget_snapshot()
        super().async_set_updated_data(data)

    @callback
    def _cancel_reading_expiry(self) -> None:
        if self._expiry_cancel is not None:
            self._expiry_cancel()
            self._expiry_cancel = None

    @staticmethod
    def _expired_readings(data: DeviceState) -> DeviceState:
        return data | {
            "available": False,
            "state": None,
            "last_error": "device state expired",
        }

    @callback
    def _schedule_reading_expiry(
        self, data: DeviceState, *, expires_at: float | None = None
    ) -> DeviceState:
        self._cancel_reading_expiry()
        if not data["available"] or data["state"] is None or self._unloaded:
            return data
        provenance = data["state"]["provenance"]
        fetched = provenance["fetched_at_unix_ms"]
        timestamps = [
            value / 1000
            for value in (fetched, provenance["observed_at_unix_ms"])
            if value is not None
        ]
        now = time()
        if fetched is None or any(value > now + MAX_CLOCK_SKEW for value in timestamps):
            return self._expired_readings(data)
        observed = min(timestamps)
        deadline = min(now, observed) + READING_MAX_AGE
        if expires_at is not None:
            deadline = min(deadline, expires_at)
        if self._reading_expiry is not None and self._reading_expiry[0] == observed:
            deadline = min(deadline, self._reading_expiry[1])
        self._reading_expiry = (observed, deadline)
        remaining = deadline - now
        if remaining <= 0:
            return self._expired_readings(data)

        @callback
        def expire(_):
            if self.data is data and not self._unloaded:
                self._expiry_cancel = None
                self.async_set_updated_data(self._expired_readings(data))

        self._expiry_cancel = async_call_later(self.hass, remaining, expire)
        return data

    async def _async_resolve_device(self) -> Device | None:
        devices = await self.client.fetch_devices()
        if self._unloaded:
            raise UpdateFailed("integration entry unloaded during inventory read")
        current = next(
            (device for device in devices if device_identity(device) == self._identity),
            None,
        )
        if self.device is not None and self.device != current:
            await self._discard_snapshot(
                "device identity, ownership or capabilities changed"
            )
            if self._unloaded:
                raise UpdateFailed("integration entry unloaded during inventory read")
        self.device = current
        if not self._entities_loaded:
            async_cleanup_registry(self.hass, self.entry, self.device)
        self._reload_changed_entities()
        return current

    def _require_active(self) -> None:
        if (
            self._unloaded
            or self.entry.state is ConfigEntryState.UNLOAD_IN_PROGRESS
            or getattr(self.entry, "runtime_data", None) is not self
        ):
            raise ApiError("this device's integration entry is no longer active")

    def _reload_changed_entities(self) -> None:
        if (
            self._entities_loaded
            and not self._unloaded
            and entity_keys(self.device) != self.loaded_entity_keys
            and (self._reload_task is None or self._reload_task.done())
        ):
            self._reload_task = self.hass.async_create_task(self._reload_entry())

    async def async_unload(self) -> None:
        """Invalidate pending work without cancelling our own reload's unload."""
        self._unloaded = True
        self._cancel_reading_expiry()
        self._save_pending = False
        await self._store.async_save(self._snapshot)
        if (
            self._reload_task is not None
            and self._reload_task is not asyncio.current_task()
        ):
            self._reload_task.cancel()
            await asyncio.gather(self._reload_task, return_exceptions=True)

    async def _reload_entry(self) -> None:
        if self._unloaded or self.entry.runtime_data is not self:
            return
        try:
            async_cleanup_registry(self.hass, self.entry, self.device)
            await self.hass.config_entries.async_reload(self.entry.entry_id)
        except Exception:
            LOGGER.exception("Could not reload changed Gafctl entities")

    async def async_set_mode(
        self, mode: str, *, only_if_current: str | None = None
    ) -> None:
        if self.entry.data["backend"] == "legacy_ble":
            if only_if_current is not None:
                raise ApiError(
                    "conditional mode changes are unsupported for this device"
                )
            await self._async_set_legacy_mode(mode)
            return
        if mode not in QUICKCONNECT_MODES or (
            only_if_current is not None
            and (only_if_current not in QUICKCONNECT_MODES or mode != "off")
        ):
            raise ApiError("unsupported device mode")
        async with self.command_lock:
            self._require_active()
            await self.async_refresh()
            current = self._require_control("quick_connect_mode", "quick_connect")[
                "settings"
            ]["mode"]
            if only_if_current is not None:
                if current not in QUICKCONNECT_MODES:
                    raise ApiError("current device mode is unknown")
                if current != only_if_current:
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
            current = self._require_control("quick_connect_mode", "quick_connect")[
                "settings"
            ]["mode"]
            matches = (
                current == mode
                if only_if_current is None
                else current in QUICKCONNECT_MODES and current != only_if_current
            )
            if not matches:
                raise ApiError("confirmed control has no matching current mode")

    async def _async_set_legacy_mode(self, mode: str) -> None:
        if mode not in ("automatic", "timer", "off"):
            raise ApiError("unsupported device mode")
        async with self.command_lock:
            self._require_active()
            await self.async_refresh()
            self._require_control("legacy_mode", "legacy_ble")
            await self._async_submit_control({"kind": "legacy_mode", "mode": mode})
            current = self._require_control("legacy_mode", "legacy_ble")
            expected = (
                "automatic"
                if mode == "timer" and self.data["timer_duration_minutes"] == 0
                else mode
            )
            if legacy_mode(current) != expected:
                raise ApiError("confirmed control has no matching current mode")

    async def async_set_number(self, control: NumberControl, value: float) -> None:
        validated = control.validate(value)
        async with self.command_lock:
            self._require_active()
            await self.async_refresh()
            self._require_control(control.capability, control.backend)
            if not control.current_supported(self.data):
                raise ApiError("the selected device has no supported current setting")
            await self._async_submit_control(control.command(validated))
            self._require_control(control.capability, control.backend)
            if control.reading(self.data) != validated:
                raise ApiError("confirmed control has no matching current setting")

    async def _async_submit_control(self, command: JsonObject) -> None:
        self._require_active()
        control_error = None
        try:
            await self.client.set_control(self.device_id, command)
        except ApiError as error:
            control_error = error
        try:
            self._require_active()
            await self.async_refresh()
            if self.current_readings is None or self.api_error is not None:
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
        self._require_active()
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
            and self.device is not None
            and self.device["capabilities"]["read_state"]
            and data
            and data["available"]
        ):
            return None
        return data["state"]

    def control_readings(self, capability: str, backend: Backend) -> Readings | None:
        if (
            self.api_error is not None
            or self.device is None
            or self.device["backend"] != backend
            or capability not in command_kinds(self.device)
        ):
            return None
        return self.current_readings

    @property
    def mode_control_available(self) -> bool:
        return self.control_readings("quick_connect_mode", "quick_connect") is not None

    def number_control_available(self, control: NumberControl) -> bool:
        state = self.control_readings(control.capability, control.backend)
        return state is not None and control.current_supported(self.data)

    @property
    def http_owned(self) -> bool:
        return self.device is not None and self.device["state_source"] == "http"


@callback
def async_cleanup_registry(
    hass: HomeAssistant, entry: GafctlConfigEntry, device: Device | None
) -> None:
    if device is None:
        return
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
        if not er.async_entries_for_device(
            entities, registered.id, include_disabled_entities=True
        ):
            devices.async_remove_device(registered.id)


type GafctlConfigEntry = ConfigEntry[GafctlCoordinator]
