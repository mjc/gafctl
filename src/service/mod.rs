use self::{
    control::RecentV2ControlResults, legacy::LegacyBleRuntime, quickconnect::QuickConnectBackend,
};
use crate::backend::DeviceRegistry;
use gafctl_api::{DeviceId, DeviceRefreshStatus};
use std::sync::Arc;
use tokio::sync::RwLock;
pub(crate) mod control;
mod inventory;
mod legacy;
#[cfg(feature = "mqtt")]
pub(crate) mod publication;
pub(crate) mod quickconnect;
mod refresh;

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

#[derive(Clone)]
pub(crate) struct DeviceService {
    registry: Arc<RwLock<DeviceRegistry>>,
    ble_device: Option<Arc<LegacyBleRuntime>>,
    #[cfg(feature = "mqtt")]
    publication: Option<Arc<publication::StatePublication>>,
    quickconnect: Option<QuickConnectBackend>,
    v2_control_results: Arc<tokio::sync::Mutex<RecentV2ControlResults>>,
}

impl DeviceService {
    pub(crate) fn with_registry(registry: DeviceRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ble_device: None,
            #[cfg(feature = "mqtt")]
            publication: None,
            quickconnect: None,
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

    pub(crate) async fn poll_and_publish_state(&self) -> DeviceRefreshStatus {
        if self.ble_device.is_some() {
            match self.refresh_device(&DeviceId::configured_ble()).await {
                Ok(response) => response.status,
                Err(status) => {
                    tracing::warn!(%status, "device refresh worker failed");
                    DeviceRefreshStatus::Failed
                }
            }
        } else {
            self.publish_state().await;
            DeviceRefreshStatus::Fresh
        }
    }

    pub(crate) fn state_polling_enabled(&self) -> bool {
        self.ble_device.is_some() || {
            #[cfg(feature = "mqtt")]
            {
                self.publication.is_some()
            }
            #[cfg(not(feature = "mqtt"))]
            {
                false
            }
        }
    }

    #[cfg(feature = "mqtt")]
    pub(crate) fn state_publication_enabled(&self) -> bool {
        self.publication.is_some()
    }

    pub(crate) fn quickconnect_polling_enabled(&self) -> bool {
        self.quickconnect.is_some()
    }

    pub(crate) fn begin_backend_shutdown(&self) {
        if let Some(ble) = &self.ble_device {
            ble.begin_shutdown();
        }
    }

    pub(crate) async fn finish_backend_cleanup(&self, deadline: tokio::time::Instant) {
        if let Some(ble) = &self.ble_device {
            match tokio::time::timeout_at(deadline, ble.wait_until_idle()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "BLE shutdown cleanup failed");
                }
                Err(_) => tracing::warn!("BLE shutdown cleanup deadline exceeded"),
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod test_support;
