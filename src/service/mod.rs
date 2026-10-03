use self::{
    control::RecentV2ControlResults, legacy::LegacyBleRuntime, quickconnect::QuickConnectRuntime,
};
use crate::backend::{DeviceRegistry, DeviceRuntime, RefreshReceiver, RefreshReservation};
use crate::quickconnect_control::QuickConnectControlService;
use anyhow::Result;
use gafctl_api::{DeviceBackend, DeviceDescriptor, DeviceId, EntitySource, EntitySources};
use gafctl_api::{
    DeviceListV2Response, DeviceRefreshStatus, DeviceRefreshV2Response, DeviceStateV2Response,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::RwLock;
#[cfg(feature = "mqtt")]
use tokio::sync::watch;

pub(crate) mod control;
mod legacy;
#[cfg(feature = "mqtt")]
pub(crate) mod mqtt;
#[cfg(feature = "mqtt")]
pub(crate) mod publication;
pub(crate) mod quickconnect;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ServiceError {
    #[error("unknown device")]
    UnknownDevice,
    #[error("device does not support reading state")]
    UnsupportedRead,
    #[error("device backend is unavailable")]
    BackendUnavailable,
    #[error("device worker is unavailable")]
    WorkerUnavailable,
    #[error("state and command sources must have the same owner")]
    InvalidSources,
    #[error("MQTT discovery is unavailable")]
    OwnershipUnavailable,
    #[error("could not persist entity ownership")]
    Persistence,
}

const DEVICE_REFRESH_TIMEOUT: Duration = Duration::from_secs(270);

#[derive(Clone)]
pub(crate) struct DeviceService {
    registry: Arc<RwLock<DeviceRegistry>>,
    ble_device: Option<Arc<LegacyBleRuntime>>,
    #[cfg(feature = "mqtt")]
    state_updates: Option<watch::Sender<Arc<publication::StateSnapshot>>>,
    #[cfg(feature = "mqtt")]
    snapshot_publication: Arc<tokio::sync::Mutex<()>>,
    #[cfg(feature = "mqtt")]
    mqtt_discovery_enabled: bool,
    quickconnect_control: Option<QuickConnectControlService>,
    quickconnect_runtime: Option<QuickConnectRuntime>,
    v2_control_results: Arc<tokio::sync::Mutex<RecentV2ControlResults>>,
}

impl DeviceService {
    pub(crate) fn with_registry(registry: DeviceRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ble_device: None,
            #[cfg(feature = "mqtt")]
            state_updates: None,
            #[cfg(feature = "mqtt")]
            snapshot_publication: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(feature = "mqtt")]
            mqtt_discovery_enabled: false,
            quickconnect_control: None,
            quickconnect_runtime: None,
            v2_control_results: Arc::default(),
        }
    }

    pub(crate) fn with_ble_device(device_id: String, mut registry: DeviceRegistry) -> Self {
        let device = registry.register_configured_ble();
        Self {
            ble_device: Some(Arc::new(LegacyBleRuntime::new(device_id, device))),
            ..Self::with_registry(registry)
        }
    }

    #[cfg(not(feature = "mqtt"))]
    async fn publish_state(&self) {}

    pub(crate) async fn poll_and_publish_state(&self) {
        if self.ble_device.is_some() {
            if let Err(status) = self.refresh_device(&DeviceId::configured_ble()).await {
                tracing::warn!(%status, "device refresh worker failed");
            }
        } else {
            self.publish_state().await;
        }
    }

    pub(crate) async fn refresh_device(
        &self,
        id: &DeviceId,
    ) -> Result<Arc<DeviceRefreshV2Response>, ServiceError> {
        let (backend, runtime) = self.refresh_target(id).await?;
        let receiver = match runtime.reserve_refresh().await {
            RefreshReservation::Join(receiver) => receiver,
            RefreshReservation::Execute {
                receiver,
                completion,
            } => {
                let state = self.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    if let Ok(response) = state.execute_device_refresh(&id, backend, &runtime).await
                    {
                        completion.send_replace(Some(Arc::new(response)));
                    }
                });
                receiver
            }
        };
        wait_for_device_refresh(receiver).await
    }

    async fn refresh_target(
        &self,
        id: &DeviceId,
    ) -> Result<(DeviceBackend, Arc<DeviceRuntime>), ServiceError> {
        let registry = self.registry.read().await;
        let descriptor = registry
            .descriptors()
            .find(|device| &device.id == id)
            .ok_or(ServiceError::UnknownDevice)?;
        if !descriptor.capabilities.read_state {
            return Err(ServiceError::UnsupportedRead);
        }
        let configured = match descriptor.backend {
            DeviceBackend::LegacyBle => self.ble_device.is_some(),
            DeviceBackend::QuickConnect => self.quickconnect_runtime.is_some(),
        };
        if !configured {
            return Err(ServiceError::BackendUnavailable);
        }
        Ok((
            descriptor.backend,
            registry.runtime(id).ok_or(ServiceError::UnknownDevice)?,
        ))
    }

    async fn execute_device_refresh(
        &self,
        id: &DeviceId,
        backend: DeviceBackend,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshV2Response, ServiceError> {
        let response = match tokio::time::timeout(
            DEVICE_REFRESH_TIMEOUT,
            self.read_device_locked(id, backend, runtime),
        )
        .await
        {
            Ok(response) => response?,
            Err(_) => {
                let mut device = device_state_v2_data(self, id).await?;
                device.last_error = Some("device refresh deadline exceeded".to_owned());
                DeviceRefreshV2Response {
                    status: DeviceRefreshStatus::Failed,
                    device,
                }
            }
        };
        self.publish_state().await;
        Ok(response)
    }

    async fn read_device_locked(
        &self,
        id: &DeviceId,
        backend: DeviceBackend,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshV2Response, ServiceError> {
        let _transaction = runtime.acquire_transaction().await;
        let status = match backend {
            DeviceBackend::LegacyBle => {
                self.ble_device
                    .as_ref()
                    .ok_or(ServiceError::BackendUnavailable)?
                    .read_state_locked()
                    .await
            }
            DeviceBackend::QuickConnect => self.read_quickconnect_state_locked(id, runtime).await?,
        };
        Ok(DeviceRefreshV2Response {
            status,
            device: device_state_v2_data(self, id).await?,
        })
    }
}

impl DeviceService {
    pub(crate) async fn inventory(&self) -> DeviceListV2Response {
        DeviceListV2Response {
            devices: self.registry.read().await.descriptors().cloned().collect(),
        }
    }

    pub(crate) async fn state(&self, id: &DeviceId) -> Result<DeviceStateV2Response, ServiceError> {
        device_state_v2_data(self, id).await
    }

    pub(crate) fn state_polling_enabled(&self) -> bool {
        self.ble_device.is_some() || {
            #[cfg(feature = "mqtt")]
            {
                self.state_updates.is_some()
            }
            #[cfg(not(feature = "mqtt"))]
            {
                false
            }
        }
    }

    pub(crate) fn quickconnect_polling_enabled(&self) -> bool {
        self.quickconnect_runtime.is_some()
    }

    pub(crate) async fn finish_backend_cleanup(&self, deadline: tokio::time::Instant) {
        if let Some(ble) = &self.ble_device
            && tokio::time::timeout_at(deadline, ble.ble_client.wait_until_idle())
                .await
                .is_err()
        {
            tracing::warn!("BLE shutdown cleanup deadline exceeded");
        }
    }

    pub(crate) async fn set_sources(
        &self,
        id: &DeviceId,
        sources: EntitySources,
    ) -> Result<DeviceDescriptor, ServiceError> {
        if sources.state_source != sources.command_source {
            return Err(ServiceError::InvalidSources);
        }
        let descriptor = {
            let mut registry = self.registry.write().await;
            if registry.runtime(id).is_none() {
                return Err(ServiceError::UnknownDevice);
            }
            if sources.state_source == EntitySource::Mqtt && !mqtt_ownership_available(self) {
                return Err(ServiceError::OwnershipUnavailable);
            }
            registry
                .set_entity_sources(id, sources.state_source, sources.command_source)
                .map_err(|error| {
                    tracing::error!(%error, "could not persist entity ownership");
                    ServiceError::Persistence
                })?;
            registry
                .descriptors()
                .find(|descriptor| &descriptor.id == id)
                .cloned()
                .ok_or(ServiceError::UnknownDevice)?
        };
        self.publish_state().await;
        Ok(descriptor)
    }
}
async fn wait_for_device_refresh(
    mut receiver: RefreshReceiver,
) -> Result<Arc<DeviceRefreshV2Response>, ServiceError> {
    receiver
        .wait_for(|result| result.is_some())
        .await
        .map_err(|_| ServiceError::WorkerUnavailable)?
        .as_ref()
        .cloned()
        .ok_or(ServiceError::WorkerUnavailable)
}

async fn device_state_v2_data(
    state: &DeviceService,
    id: &DeviceId,
) -> Result<DeviceStateV2Response, ServiceError> {
    let registry = state.registry.read().await;
    let descriptor = registry
        .descriptors()
        .find(|descriptor| descriptor.id == *id)
        .ok_or(ServiceError::UnknownDevice)?;
    let runtime = registry.runtime(id).ok_or(ServiceError::UnknownDevice)?;
    let snapshot = runtime.snapshot().await;
    let (last_error, inventory_status) = match (&descriptor.backend, state.ble_device.as_ref()) {
        (DeviceBackend::LegacyBle, Some(device)) if *id == DeviceId::configured_ble() => {
            let reconciler = device.reconciler.read().await;
            let poll_error = reconciler.last_error().map(str::to_owned);
            let inventory_status = if poll_error.is_some() && reconciler.latest_snapshot().is_none()
            {
                crate::backend::DeviceInventoryStatus::Unavailable
            } else {
                snapshot.inventory_status
            };
            (poll_error.or(snapshot.last_error), inventory_status)
        }
        _ => (snapshot.last_error, snapshot.inventory_status),
    };
    Ok(DeviceStateV2Response {
        id: id.clone(),
        backend: descriptor.backend,
        available: snapshot.state.is_some(),
        inventory_status,
        last_error,
        state: snapshot.state,
    })
}

#[cfg(feature = "mqtt")]
fn mqtt_ownership_available(state: &DeviceService) -> bool {
    state.mqtt_discovery_enabled && state.state_updates.is_some()
}

#[cfg(not(feature = "mqtt"))]
fn mqtt_ownership_available(_: &DeviceService) -> bool {
    false
}

#[cfg(test)]
mod tests;
